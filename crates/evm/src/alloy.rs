//! Alloy EVM trait adapter for Taiko execution semantics.
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
};

use alloy_evm::{Database, Evm, EvmEnv};
use alloy_primitives::hex;
use reth_revm::{
    Context, ExecuteEvm, InspectEvm, Inspector,
    context::{
        BlockEnv, TxEnv,
        result::{EVMError, ExecutionResult, HaltReason, Output, ResultAndState, SuccessReason},
    },
    handler::PrecompileProvider,
    interpreter::InterpreterResult,
    primitives::{Address, Bytes, TxKind, U256},
};
use tracing::debug;

use crate::{
    context::TaikoEvmContext, evm::TaikoEvm, handler::get_treasury_address, spec::TaikoSpecId,
};

/// System caller address used for Taiko anchor system-call pre-execution.
pub const TAIKO_GOLDEN_TOUCH_ADDRESS: [u8; 20] = hex!("0x0000777735367b36bc9b61c50022d9d0700db4ec");

/// A wrapper around the Taiko EVM that implements the `Evm` trait in `alloy_evm`.
pub struct TaikoEvmWrapper<DB: Database, INSP, P> {
    /// Wrapped Taiko EVM instance implementing execution behavior.
    inner: TaikoEvm<TaikoEvmContext<DB>, INSP, P>,
    /// Whether to run transactions through the inspector execution path.
    inspect: bool,
}

impl<DB: Database, INSP, P> TaikoEvmWrapper<DB, INSP, P> {
    /// Creates a new [`TaikoEvmWrapper`] instance.
    pub const fn new(evm: TaikoEvm<TaikoEvmContext<DB>, INSP, P>, inspect: bool) -> Self {
        Self { inner: evm, inspect }
    }

    /// Consumes self and return the inner EVM instance.
    pub fn into_inner(self) -> TaikoEvm<TaikoEvmContext<DB>, INSP, P> {
        self.inner
    }

    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &TaikoEvmContext<DB> {
        &self.inner.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut TaikoEvmContext<DB> {
        &mut self.inner.inner.ctx
    }
}

impl<DB: Database, I, P> Deref for TaikoEvmWrapper<DB, I, P> {
    type Target = TaikoEvmContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I, P> DerefMut for TaikoEvmWrapper<DB, I, P> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

/// An instance of an ethereum virtual machine.
///
/// An EVM is commonly initialized with the corresponding block context and state and it's only
/// purpose is to execute transactions.
///
/// Executing a transaction will return the outcome of the transaction.
impl<DB, I, P> Evm for TaikoEvmWrapper<DB, I, P>
where
    DB: Database,
    I: Inspector<TaikoEvmContext<DB>>,
    P: PrecompileProvider<TaikoEvmContext<DB>, Output = InterpreterResult>,
{
    /// Database type held by the EVM.
    type DB = DB;
    /// The transaction object that the EVM will execute.
    type Tx = TxEnv;
    /// Error type returned by EVM. Contains either errors related to invalid transactions or
    /// internal irrecoverable execution errors.
    type Error = EVMError<DB::Error>;
    /// Halt reason. Enum over all possible reasons for halting the execution. When execution halts,
    /// it means that transaction is valid, however, it's execution was interrupted (e.g because of
    /// running out of gas or overflowing stack).
    type HaltReason = HaltReason;
    /// Identifier of the EVM specification. EVM is expected to use this identifier to determine
    /// which features are enabled.
    type Spec = TaikoSpecId;
    /// Block environment used by the EVM.
    type BlockEnv = BlockEnv;
    /// Precompiles used by the EVM.
    type Precompiles = P;
    /// Evm inspector.
    type Inspector = I;

    /// Reference to [`BlockEnv`].
    fn block(&self) -> &BlockEnv {
        &self.block
    }

    /// Returns the chain ID of the environment.
    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    /// Provides immutable references to the database, inspector and precompiles.
    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (
            &self.inner.inner.ctx.journaled_state.database,
            &self.inner.inner.inspector,
            &self.inner.inner.precompiles,
        )
    }

    /// Provides mutable references to the database, inspector and precompiles.
    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.inner.ctx.journaled_state.database,
            &mut self.inner.inner.inspector,
            &mut self.inner.inner.precompiles,
        )
    }

    /// Executes a transaction and returns the outcome.
    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect { self.inner.inspect_tx(tx) } else { self.inner.transact(tx) }
    }

    /// Executes a system call.
    ///
    /// Note: this will only keep the target `contract` in the state. This is done because revm is
    /// loading [`BlockEnv::beneficiary`] into state by default, and we need to avoid it by also
    /// covering edge cases when beneficiary is set to the system contract address.
    /// NOTE: we use this call as a workaround to mark the Anchor transaction and base fee share
    /// percentage in the current block.
    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        // NOTE: we use this workaround to mark the Anchor transaction and base fee share percentage
        // in this block.
        if caller == Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS) &&
            contract == get_treasury_address(self.chain_id())
        {
            let (base_fee_share_pctg, caller_nonce) = decode_anchor_system_call_data(&data)
                .ok_or(EVMError::Custom("invalid encoded anchor system call data".to_string()))?;
            debug!(target: "taiko_evm", "Anchor system call detected: base_fee_share_pctg = {}, caller_nonce = {}", base_fee_share_pctg, caller_nonce);

            // Set the Anchor transaction information for the later EVM execution.
            self.inner.with_extra_execution_context(base_fee_share_pctg, caller, caller_nonce);

            // Return a dummy execution result and state to avoid further processing.
            return Ok(ResultAndState {
                result: ExecutionResult::Success {
                    reason: SuccessReason::Return,
                    gas_used: 0,
                    gas_refunded: 0,
                    logs: vec![],
                    output: Output::Call(Bytes::new()),
                },
                state: HashMap::default(),
            });
        }

        let tx = TxEnv {
            caller,
            kind: TxKind::Call(contract),
            // Explicitly set nonce to 0 so revm does not do any nonce checks
            nonce: 0,
            gas_limit: 30_000_000,
            value: U256::ZERO,
            data,
            // Setting the gas price to zero enforces that no value is transferred as part of the
            // call, and that the call will not count against the block's gas limit
            gas_price: 0,
            // The chain ID check is not relevant here and is disabled if set to None
            chain_id: None,
            // Setting the gas priority fee to None ensures the effective gas price is derived from
            // the `gas_price` field, which we need to be zero
            gas_priority_fee: None,
            access_list: Default::default(),
            // blob fields can be None for this tx
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: 0,
            authorization_list: Default::default(),
        };

        let mut gas_limit = tx.gas_limit;
        let mut basefee = 0;
        let mut disable_nonce_check = true;

        // ensure the block gas limit is >= the tx
        core::mem::swap(&mut self.block.gas_limit, &mut gas_limit);
        // disable the base fee check for this call by setting the base fee to zero
        core::mem::swap(&mut self.block.basefee, &mut basefee);
        // disable the nonce check
        core::mem::swap(&mut self.cfg.disable_nonce_check, &mut disable_nonce_check);

        let mut res = self.transact(tx);

        // swap back to the previous gas limit
        core::mem::swap(&mut self.block.gas_limit, &mut gas_limit);
        // swap back to the previous base fee
        core::mem::swap(&mut self.block.basefee, &mut basefee);
        // swap back to the previous nonce check flag
        core::mem::swap(&mut self.cfg.disable_nonce_check, &mut disable_nonce_check);

        // NOTE: We assume that only the contract storage is modified. Revm currently marks the
        // caller and block beneficiary accounts as "touched" when we do the above transact calls,
        // and includes them in the result.
        //
        // We're doing this state cleanup to make sure that changeset only includes the changed
        // contract storage.
        if let Ok(res) = &mut res {
            res.state.retain(|addr, _| *addr == contract);
        }

        res
    }

    /// Returns a mutable reference to the underlying database.
    fn db_mut(&mut self) -> &mut Self::DB {
        &mut self.journaled_state.database
    }

    /// Consumes the EVM and returns the inner [`EvmEnv`].
    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>)
    where
        Self: Sized,
    {
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } = self.inner.inner.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    /// Determines whether additional transactions should be inspected or not.
    ///
    /// See also [`EvmFactory::create_evm_with_inspector`].
    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    /// Getter of precompiles.
    fn precompiles(&self) -> &Self::Precompiles {
        &self.inner.inner.precompiles
    }

    /// Mutable getter of precompiles.
    fn precompiles_mut(&mut self) -> &mut Self::Precompiles {
        &mut self.inner.inner.precompiles
    }

    /// Getter of inspector.
    fn inspector(&self) -> &Self::Inspector {
        &self.inner.inner.inspector
    }

    /// Mutable getter of inspector.
    fn inspector_mut(&mut self) -> &mut Self::Inspector {
        &mut self.inner.inner.inspector
    }
}

/// Decode encoded anchor system-call bytes into `(base_fee_share_pctg, caller_nonce)`.
#[inline]
pub fn decode_anchor_system_call_data(bytes: &Bytes) -> Option<(u64, u64)> {
    if bytes.len() != 16 {
        return None;
    }
    let base_fee_share_pctg = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
    let caller_nonce = u64::from_be_bytes(bytes[8..16].try_into().ok()?);
    Some((base_fee_share_pctg, caller_nonce))
}
