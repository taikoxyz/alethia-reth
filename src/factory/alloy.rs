use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
};

use alloy_evm::{Database, Evm, EvmEnv, eth::EthEvmContext};
use alloy_primitives::hex;
use reth::revm::{
    Context, ExecuteEvm, InspectEvm, Inspector,
    context::{
        BlockEnv, TxEnv,
        result::{EVMError, ExecutionResult, HaltReason, Output, ResultAndState, SuccessReason},
    },
    primitives::{Address, Bytes, TxKind, U256, hardfork::SpecId},
};
use reth_evm::precompiles::PrecompilesMap;
use tracing::debug;

use crate::evm::evm::{TaikoEvm, TaikoEvmExtraContext};

pub const TAIKO_GOLDEN_TOUCH_ADDRESS: [u8; 20] = hex!("0x0000777735367b36bc9b61c50022d9d0700db4ec");

pub struct TaikoEvmWrapper<DB: Database, INSP> {
    inner: TaikoEvm<EthEvmContext<DB>, INSP>,
    inspect: bool,
}

impl<DB: Database, INSP> TaikoEvmWrapper<DB, INSP> {
    /// Creates a new Taiko EVM instance.
    pub const fn new(evm: TaikoEvm<EthEvmContext<DB>, INSP>, inspect: bool) -> Self {
        Self {
            inner: evm,
            inspect,
        }
    }

    /// Consumes self and return the inner EVM instance.
    pub fn into_inner(self) -> TaikoEvm<EthEvmContext<DB>, INSP> {
        self.inner
    }

    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &EthEvmContext<DB> {
        &self.inner.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut EthEvmContext<DB> {
        &mut self.inner.inner.ctx
    }
}

impl<DB: Database, I> Deref for TaikoEvmWrapper<DB, I> {
    type Target = EthEvmContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for TaikoEvmWrapper<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, I> Evm for TaikoEvmWrapper<DB, I>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    /// Reference to [`BlockEnv`].
    fn block(&self) -> &BlockEnv {
        &self.block
    }

    /// Returns the chain ID of the environment.
    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    /// Executes a transaction and returns the outcome.
    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            self.inner.set_tx(tx);
            self.inner.inspect_replay()
        } else {
            self.inner.transact(tx)
        }
    }

    /// Executes a system call.
    ///
    /// Note: this will only keep the target `contract` in the state. This is done because revm is
    /// loading [`BlockEnv::beneficiary`] into state by default, and we need to avoid it by also
    /// covering edge cases when beneficiary is set to the system contract address.
    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        // NOTE: we use this workaround to mark the nonce of the Anchor transaction in the current block.
        if caller == Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS) {
            let (basefee_share_pctg, caller_nonce) = decode_anchor_system_call_data(&data).ok_or(
                EVMError::Custom("invalid encoded anchor system call data".to_string()),
            )?;
            debug!(
                "Anchor system call detected: basefee_share_pctg = {}, caller_nonce = {}",
                basefee_share_pctg, caller_nonce
            );
            self.inner.extra_context =
                TaikoEvmExtraContext::new(basefee_share_pctg, Some(caller), Some(caller_nonce));

            // Return a dummy result and state to avoid further processing.
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

        self.inner.extra_context = TaikoEvmExtraContext::new(0, None, None);
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
    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>)
    where
        Self: Sized,
    {
        let Context {
            block: block_env,
            cfg: cfg_env,
            journaled_state,
            ..
        } = self.inner.inner.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn precompiles(&self) -> &Self::Precompiles {
        &self.inner.inner.precompiles
    }

    fn precompiles_mut(&mut self) -> &mut Self::Precompiles {
        &mut self.inner.inner.precompiles
    }

    fn inspector(&self) -> &Self::Inspector {
        &self.inner.inner.inspector
    }

    fn inspector_mut(&mut self) -> &mut Self::Inspector {
        &mut self.inner.inner.inspector
    }
}

fn decode_anchor_system_call_data(bytes: &Bytes) -> Option<(u64, u64)> {
    if bytes.len() != 16 {
        return None;
    }
    let basefee_share_pctg = u64::from_be_bytes(bytes[0..8].try_into().ok()?);
    let caller_nonce = u64::from_be_bytes(bytes[8..16].try_into().ok()?);
    Some((basefee_share_pctg, caller_nonce))
}
