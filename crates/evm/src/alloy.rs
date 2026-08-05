//! Alloy EVM trait adapter for Taiko execution semantics.
use std::ops::{Deref, DerefMut};

use alloy_evm::{Database, Evm, EvmEnv};
use alloy_primitives::{Address, Bytes, TxKind, U256};
// Re-export from primitives so downstream consumers can use the lighter crate.
pub use alethia_reth_primitives::addresses::TAIKO_GOLDEN_TOUCH_ADDRESS;
use reth_revm::{
    Context, Inspector,
    context::{
        BlockEnv, CfgEnv, ContextSetters, ContextTr, JournalTr, TxEnv,
        result::{
            EVMError, ExecutionResult, HaltReason, Output, ResultAndState, ResultGas, SuccessReason,
        },
    },
    handler::{EthFrame, Handler, PrecompileProvider},
    inspector::InspectorHandler,
    interpreter::{InterpreterResult, interpreter::EthInterpreter},
    state::EvmState,
};
use tracing::debug;

use crate::{
    evm::{TaikoEvm, TaikoEvmExtraExecutionCtx},
    handler::{TaikoEvmHandler, get_treasury_address},
    spec::TaikoSpecId,
    zk_gas::{
        adapter::ZkGasInspector,
        meter::{ZkGasMeter, ZkGasOutcome},
    },
};

/// Maximum transaction gas limit enforced once Osaka/Unzen semantics are active.
const MAX_SYSTEM_CALL_GAS_LIMIT: u64 = 16_777_216;

/// Base Taiko EVM implementation before optional JIT dispatch is applied.
type BaseTaikoEvm<DB, I, P> = TaikoEvm<TaikoEvmContext<DB>, ZkGasInspector<I>, P>;

/// Taiko EVM implementation used by the Alloy adapter when JIT support is compiled in.
#[cfg(feature = "jit")]
type InnerTaikoEvm<DB, I, P> = revmc::revm_evm::JitEvm<BaseTaikoEvm<DB, I, P>>;

/// Taiko EVM implementation used by the Alloy adapter without JIT support.
#[cfg(not(feature = "jit"))]
type InnerTaikoEvm<DB, I, P> = BaseTaikoEvm<DB, I, P>;

/// A wrapper around the Taiko EVM that implements the `Evm` trait in `alloy_evm`.
pub struct TaikoEvmWrapper<DB: Database, I, P> {
    /// Wrapped Taiko EVM instance implementing execution behavior.
    ///
    /// WARNING (jit builds): revmc's `JitEvm` publicly implements `ExecuteEvm`/`InspectEvm`
    /// backed by revm's `MainnetHandler`. Never call those entry points on this field — always
    /// drive it with [`TaikoEvmHandler`] (see `transact_raw`), otherwise Taiko's anchor and
    /// fee-share semantics are silently dropped.
    inner: InnerTaikoEvm<DB, I, P>,
    /// Whether to run transactions through the inspector execution path.
    inspect: bool,
    /// Whether [`Self::maybe_derive_anchor_execution_ctx`] may install a derived anchor
    /// context for replay-style execution. Enabled by default; the block executor turns it off
    /// because it installs the authoritative context through the anchor system call, and a
    /// missing pre-execution initialization must keep failing loudly there.
    derive_anchor_ctx: bool,
}

impl<DB: Database, I, P> TaikoEvmWrapper<DB, I, P> {
    /// Creates an interpreter-backed [`TaikoEvmWrapper`] instance.
    #[cfg(feature = "jit")]
    pub fn new(evm: BaseTaikoEvm<DB, I, P>, inspect: bool) -> Self
    where
        P: PrecompileProvider<TaikoEvmContext<DB>, Output = InterpreterResult>,
    {
        Self { inner: revmc::revm_evm::JitEvm::disabled(evm), inspect, derive_anchor_ctx: true }
    }

    /// Creates an interpreter-backed [`TaikoEvmWrapper`] instance.
    #[cfg(not(feature = "jit"))]
    pub const fn new(evm: BaseTaikoEvm<DB, I, P>, inspect: bool) -> Self {
        Self { inner: evm, inspect, derive_anchor_ctx: true }
    }

    /// Creates a wrapper around an already configured optional JIT dispatcher.
    pub(crate) fn new_with_inner(evm: InnerTaikoEvm<DB, I, P>, inspect: bool) -> Self {
        Self { inner: evm, inspect, derive_anchor_ctx: true }
    }

    /// Consumes self and return the inner EVM instance.
    pub fn into_inner(self) -> BaseTaikoEvm<DB, I, P> {
        #[cfg(feature = "jit")]
        {
            self.inner.into_inner()
        }
        #[cfg(not(feature = "jit"))]
        self.inner
    }

    /// Returns the base Taiko EVM wrapped by the optional JIT dispatcher.
    fn base_evm(&self) -> &BaseTaikoEvm<DB, I, P> {
        #[cfg(feature = "jit")]
        {
            self.inner.inner()
        }
        #[cfg(not(feature = "jit"))]
        &self.inner
    }

    /// Returns the mutable base Taiko EVM wrapped by the optional JIT dispatcher.
    fn base_evm_mut(&mut self) -> &mut BaseTaikoEvm<DB, I, P> {
        #[cfg(feature = "jit")]
        {
            self.inner.inner_mut()
        }
        #[cfg(not(feature = "jit"))]
        &mut self.inner
    }

    /// Provides a reference to the EVM context.
    pub fn ctx(&self) -> &TaikoEvmContext<DB> {
        &self.base_evm().inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut TaikoEvmContext<DB> {
        &mut self.base_evm_mut().inner.ctx
    }

    /// Returns a reference to the active zk gas meter, if metering is enabled.
    ///
    /// Returns `None` when the active spec/chain combination has no zk gas schedule
    /// (pre-Unzen specs).
    pub fn meter(&self) -> Option<&ZkGasMeter<'static>> {
        let evm = self.base_evm();
        evm.zk_gas_meter().or_else(|| evm.inner.inspector.meter())
    }

    /// Returns a mutable reference to the active zk gas meter, if metering is enabled.
    ///
    /// Returns `None` when the active spec/chain combination has no zk gas schedule
    /// (pre-Unzen specs).
    pub fn meter_mut(&mut self) -> Option<&mut ZkGasMeter<'static>> {
        if self.base_evm().zk_gas_meter().is_some() {
            self.base_evm_mut().zk_gas_meter_mut()
        } else {
            self.base_evm_mut().inner.inspector.meter_mut()
        }
    }

    /// Derives and installs the anchor execution context from database state when no
    /// authoritative context is present and the incoming transaction is anchor-shaped
    /// (golden touch calling the network treasury).
    ///
    /// Block execution installs the context through the anchor system call before any
    /// transaction runs, so this only fires on replay-style paths (`debug_trace*`, `trace_*`
    /// and the `eth_call` family) that execute block transactions without the block executor.
    /// Without a context those paths fail the anchor's balance check, which is what made every
    /// trace of a real block error with `insufficient funds`.
    ///
    /// The golden-touch nonce is snapshotted on first derivation and the context is kept for
    /// the EVM's lifetime, mirroring the per-block snapshot the system call takes: a crafted
    /// golden-touch transaction later in the block does not match the snapshot nonce and keeps
    /// consensus semantics (normal balance check).
    ///
    /// That protection is per EVM instance. Tracing helpers that create a fresh EVM per
    /// transaction (`debug_traceBlock*`, and the target EVM of `debug_traceTransaction`)
    /// re-derive from the already-advanced database nonce, so a deliberately crafted
    /// golden-touch -> treasury transaction placed after the anchor is still wrongly exempted
    /// in those traces while consensus charges it fees. Real anchors are unaffected in every
    /// topology because the anchor system-call marker commits no state. Closing that gap needs
    /// the block-start nonce carried through the EVM environment; see
    /// `fresh_replay_evm_still_exempts_crafted_golden_touch_tx_after_anchor`.
    fn maybe_derive_anchor_execution_ctx(&mut self, tx: &TxEnv) -> Result<(), EVMError<DB::Error>> {
        if !self.derive_anchor_ctx || self.base_evm().extra_execution_ctx.is_some() {
            return Ok(());
        }
        let golden_touch = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        if tx.caller != golden_touch ||
            tx.kind != TxKind::Call(get_treasury_address(self.ctx().cfg.chain_id))
        {
            return Ok(());
        }
        let nonce = self
            .ctx_mut()
            .journaled_state
            .database
            .basic(golden_touch)
            .map_err(EVMError::Database)?
            .map_or(0, |account| account.nonce);
        self.base_evm_mut().extra_execution_ctx =
            Some(TaikoEvmExtraExecutionCtx::derived(golden_touch, nonce));
        Ok(())
    }
}

/// EVM extension trait controlling the replay-only anchor context derivation.
pub trait TaikoAnchorEvm {
    /// Enables or disables on-the-fly anchor context derivation for replay-style execution.
    ///
    /// The block executor disables it before executing a block: it installs the authoritative
    /// context through the anchor system call, and executing an anchor without that
    /// initialization must keep failing loudly rather than silently falling back to derived
    /// replay semantics.
    fn set_anchor_ctx_derivation_enabled(&mut self, enabled: bool);
}

impl<DB: Database, I, P> TaikoAnchorEvm for TaikoEvmWrapper<DB, I, P> {
    /// Enables or disables on-the-fly anchor context derivation for replay-style execution.
    fn set_anchor_ctx_derivation_enabled(&mut self, enabled: bool) {
        self.derive_anchor_ctx = enabled;
    }
}

/// EVM extension trait for reading and mutating the zk gas meter state.
pub trait TaikoZkGasEvm {
    /// Discards any in-flight zk gas recorded for the current transaction.
    fn reset_transaction_zk_gas(&mut self);

    /// Commits the current transaction's zk gas into the block total and returns the new total.
    fn commit_transaction_zk_gas(&mut self) -> Result<Option<u64>, ZkGasOutcome>;

    /// Returns `true` when committing the current transaction's zk gas would exceed the block
    /// budget, i.e. when [`Self::commit_transaction_zk_gas`] would fail. Always `false` when no
    /// meter is installed (pre-Unzen specs).
    fn transaction_zk_gas_commit_would_exceed(&self) -> bool;

    /// Returns the finalized block zk gas that has already been committed.
    fn block_zk_gas_used(&self) -> Option<u64>;

    /// Charges the fixed per-transaction intrinsic zk gas defined by the active schedule.
    ///
    /// Returns `Ok(())` when the EVM has no meter installed (pre-Unzen specs).
    fn charge_tx_intrinsic_zk_gas(&mut self) -> Result<(), ZkGasOutcome>;
}

impl<DB, I, P> TaikoZkGasEvm for TaikoEvmWrapper<DB, I, P>
where
    DB: Database,
    I: Inspector<TaikoEvmContext<DB>>,
    P: PrecompileProvider<TaikoEvmContext<DB>, Output = InterpreterResult>,
{
    /// Discards any in-flight zk gas recorded for the current transaction.
    fn reset_transaction_zk_gas(&mut self) {
        if let Some(meter) = self.meter_mut() {
            meter.reset_transaction();
        }
    }

    /// Commits the current transaction's zk gas into the block total and returns the new total.
    fn commit_transaction_zk_gas(&mut self) -> Result<Option<u64>, ZkGasOutcome> {
        let Some(meter) = self.meter_mut() else {
            return Ok(None);
        };
        meter.commit_transaction()?;
        Ok(Some(meter.block_zk_gas_used()))
    }

    /// Returns whether committing the current transaction's zk gas would exceed the budget.
    fn transaction_zk_gas_commit_would_exceed(&self) -> bool {
        self.meter().is_some_and(|m| m.commit_would_exceed_block_limit())
    }

    /// Returns the finalized block zk gas that has already been committed.
    fn block_zk_gas_used(&self) -> Option<u64> {
        self.meter().map(|m| m.block_zk_gas_used())
    }

    /// Charges the fixed per-transaction intrinsic zk gas through the meter.
    fn charge_tx_intrinsic_zk_gas(&mut self) -> Result<(), ZkGasOutcome> {
        let Some(meter) = self.meter_mut() else {
            return Ok(());
        };
        meter.charge_tx_intrinsic()
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

/// Canonical Taiko EVM context type used by the Alloy adapter.
pub type TaikoEvmContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<TaikoSpecId>, DB>;

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

    /// Reference to [`CfgEnv`].
    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        &self.cfg
    }

    /// Returns the chain ID of the environment.
    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    /// Provides immutable references to the database, inspector and precompiles.
    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        let evm = self.base_evm();
        (
            &evm.inner.ctx.journaled_state.database,
            evm.inner.inspector.inner(),
            &evm.inner.precompiles,
        )
    }

    /// Provides mutable references to the database, inspector and precompiles.
    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        let evm = self.base_evm_mut();
        (
            &mut evm.inner.ctx.journaled_state.database,
            evm.inner.inspector.inner_mut(),
            &mut evm.inner.precompiles,
        )
    }

    /// Executes a transaction and returns the outcome.
    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.maybe_derive_anchor_execution_ctx(&tx)?;
        self.ctx_mut().set_tx(tx);
        // Run [`TaikoEvmHandler`] against the (possibly JIT-dispatching) inner EVM directly:
        // revmc's own `ExecuteEvm`/`InspectEvm` entry points would run revm's `MainnetHandler`
        // and silently drop Taiko's anchor and fee-share semantics.
        //
        // Those entry points also re-validate revmc's per-instance lookup cache against the
        // active spec (`JitEvm::invalidate_cache`, private upstream). Skipping that here is
        // sound only because reth constructs a fresh EVM per block environment, so the spec
        // never changes within this instance's lifetime.
        let mut handler = TaikoEvmHandler::<_, EVMError<DB::Error>, EthFrame<EthInterpreter>>::new(
            self.base_evm().extra_execution_ctx.clone(),
        );
        let result = if self.inspect {
            // Trace correctness requires inspected execution to run on the disabled JIT backend:
            // compiled code cannot deliver per-step inspector callbacks (revmc forwards only
            // log, selfdestruct, and frame-end events). `create_evm_with_inspector` and
            // `set_inspector_enabled` both pin the disabled backend; this guards the invariant
            // against future refactors.
            #[cfg(feature = "jit")]
            debug_assert!(
                !self.inner.backend().enabled(),
                "inspected execution must run with the disabled JIT backend",
            );
            handler.inspect_run(&mut self.inner)
        } else {
            handler.run(&mut self.inner)
        };
        // Finalize before propagating errors, mirroring revm's `ExecuteEvm::transact`: payload
        // building and derived-block execution skip invalid transactions and keep executing on
        // this EVM, and an errored run must not leave the journal for the next transaction.
        let state = self.ctx_mut().journal_mut().finalize();
        Ok(ResultAndState::new(result?, state))
    }

    /// Executes a system call.
    ///
    /// For regular system calls, only the target `contract` is kept in the returned changeset.
    /// This avoids revm's default [`BlockEnv::beneficiary`] state load, including the edge case
    /// where the beneficiary is set to the system contract address.
    ///
    /// Anchor system calls are handled as a metadata marker for the current block: they retain the
    /// caller and contract accounts in the internal journal for witness generation, but still
    /// return an empty changeset.
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
            self.base_evm_mut().with_extra_execution_context(
                base_fee_share_pctg,
                caller,
                caller_nonce,
            );

            // Load both system-call participants through the journal so witness generation can
            // include the same pre-execution dependencies that stateless validation will read
            // later.
            //
            // Commit the synthetic pre-execution load immediately afterwards. This keeps both the
            // `caller` and `contract` accounts in the internal journal state for witness
            // generation, but advances the journal transaction id so the first real anchor
            // transaction still pays normal cold-access costs and preserves mainline gas
            // accounting.
            let journal = self.ctx_mut().journal_mut();
            journal.load_account(caller)?;
            journal.load_account(contract)?;
            journal.commit_tx();

            // Return a dummy execution result with an empty `ResultAndState.state` changeset to
            // avoid further processing. The internal journal state above is intentionally retained.
            return Ok(ResultAndState {
                result: ExecutionResult::Success {
                    reason: SuccessReason::Return,
                    gas: ResultGas::new_with_state_gas(0, 0, 0, 0),
                    logs: vec![],
                    output: Output::Call(Bytes::new()),
                },
                state: EvmState::default(),
            });
        }

        let tx = TxEnv {
            caller,
            kind: TxKind::Call(contract),
            // Explicitly set nonce to 0 so revm does not do any nonce checks
            nonce: 0,
            // Osaka caps any single transaction gas limit, including internal system calls.
            gas_limit: MAX_SYSTEM_CALL_GAS_LIMIT,
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
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } =
            self.into_inner().inner.ctx;

        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    /// Determines whether additional transactions should be inspected or not.
    ///
    /// See also [`EvmFactory::create_evm_with_inspector`].
    fn set_inspector_enabled(&mut self, enabled: bool) {
        // `create_evm` installs zk-gas on the production TaikoEvm wrapper and keeps the inner
        // inspector unmetered. Enabling the NoOp inspector would route around that production
        // meter, so only EVMs built through `create_evm_with_inspector` may switch to inspect mode.
        if enabled && self.base_evm().zk_gas_meter().is_some() {
            return;
        }
        // Inspected execution cannot dispatch to compiled code (no per-step inspector
        // callbacks), so switching an instance to inspect mode permanently downgrades it to a
        // disabled backend. EVMs built through `create_evm_with_inspector` already hold one;
        // this covers EVMs built through `create_evm` on non-metered specs.
        #[cfg(feature = "jit")]
        if enabled {
            self.inner.set_backend(crate::jit::JitBackend::disabled());
        }
        self.inspect = enabled;
    }

    /// Getter of precompiles.
    fn precompiles(&self) -> &Self::Precompiles {
        &self.base_evm().inner.precompiles
    }

    /// Mutable getter of precompiles.
    fn precompiles_mut(&mut self) -> &mut Self::Precompiles {
        &mut self.base_evm_mut().inner.precompiles
    }

    /// Getter of inspector.
    fn inspector(&self) -> &Self::Inspector {
        self.base_evm().inner.inspector.inner()
    }

    /// Mutable getter of inspector.
    fn inspector_mut(&mut self) -> &mut Self::Inspector {
        self.base_evm_mut().inner.inspector.inner_mut()
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

#[cfg(test)]
mod tests {
    use alloy_evm::{Evm, EvmEnv, EvmFactory};
    use alloy_primitives::U256;
    use reth_revm::{
        context::{ContextTr, result::InvalidTransaction},
        db::InMemoryDB,
        state::AccountInfo,
    };

    use super::*;
    use crate::{factory::TaikoEvmFactory, spec::TaikoSpecId};

    fn encode_anchor_system_call_data(base_fee_share_pctg: u64, caller_nonce: u64) -> Bytes {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&base_fee_share_pctg.to_be_bytes());
        bytes.extend_from_slice(&caller_nonce.to_be_bytes());
        bytes.into()
    }

    #[test]
    fn anchor_system_call_records_witness_accounts_without_warming_next_tx() {
        let golden_touch = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        let chain_id = 167_000;
        let treasury = get_treasury_address(chain_id);

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            golden_touch,
            AccountInfo { nonce: 7, balance: U256::ZERO, ..Default::default() },
        );
        db.insert_account_info(
            treasury,
            AccountInfo { nonce: 0, balance: U256::ZERO, ..Default::default() },
        );

        let mut env: EvmEnv<TaikoSpecId> = EvmEnv::default();
        env.cfg_env.chain_id = chain_id;
        let mut evm = TaikoEvmFactory::default().create_evm(db, env);

        evm.transact_system_call(golden_touch, treasury, encode_anchor_system_call_data(25, 7))
            .expect("anchor system call should short-circuit successfully");

        let journal = evm.ctx().journal();
        let witness_state = &journal.state;
        assert!(
            witness_state.contains_key(&golden_touch),
            "golden touch must be recorded in journal state for witness generation"
        );
        assert!(
            witness_state.contains_key(&treasury),
            "treasury must be recorded in journal state for witness generation"
        );

        let next_tx_id = journal.transaction_id;
        assert_eq!(next_tx_id.get(), 1, "synthetic pre-execution load should advance tx id");

        let golden_touch_account = witness_state
            .get(&golden_touch)
            .expect("golden touch account must stay in witness state");
        assert!(
            golden_touch_account.is_cold_transaction_id(next_tx_id),
            "golden touch must be cold again for the first real transaction"
        );

        let treasury_account =
            witness_state.get(&treasury).expect("treasury account must stay in witness state");
        assert!(
            treasury_account.is_cold_transaction_id(next_tx_id),
            "treasury must be cold again for the first real transaction"
        );
    }

    #[test]
    fn errored_transaction_finalizes_the_journal_for_the_next_transaction() {
        // Payload building and derived-block execution skip invalid transactions and keep
        // executing on the same EVM, so an errored `transact_raw` must leave the journal as
        // finalized as a successful one — revm's `ExecuteEvm::transact` finalizes
        // unconditionally for exactly this reason.
        let broke_caller = Address::with_last_byte(0xBC);
        let mut env: EvmEnv<TaikoSpecId> = EvmEnv::default();
        env.cfg_env.chain_id = 167_000;
        let mut evm = TaikoEvmFactory::default().create_evm(InMemoryDB::default(), env);

        let tx = TxEnv::builder()
            .caller(broke_caller)
            .kind(TxKind::Call(Address::ZERO))
            // Send value from an unfunded account: validation loads the caller through the
            // journal and only then errors, so the failed transaction has journal state to
            // leak. A pre-state error (e.g. a chain-id mismatch) would not exercise this.
            .value(U256::from(1))
            .gas_limit(21_000)
            .chain_id(None)
            .build()
            .expect("valid tx env");
        evm.transact_raw(tx).expect_err("transaction from an unfunded caller must error");

        assert!(
            evm.ctx().journal().state.is_empty(),
            "an errored transaction must finalize the journal before the next transaction runs"
        );
    }

    /// Golden-touch dust balance observed on mainnet: non-zero, but far below the anchor's
    /// upfront cost of `gas_limit * gas_price` (`1_000_000 * 10_000_000 = 1e13` wei).
    const GOLDEN_TOUCH_DUST_BALANCE: u64 = 316_794_861_226;

    /// Basefee used by the replay tests, matching the anchor's gas price (wei).
    const REPLAY_BASEFEE: u64 = 10_000_000;

    /// Builds an [`EvmEnv`] the way RPC replay paths do: block context only, no anchor
    /// system call ever happens on the resulting EVM.
    fn replay_env(chain_id: u64) -> EvmEnv<TaikoSpecId> {
        let mut env: EvmEnv<TaikoSpecId> = EvmEnv::default();
        env.cfg_env.chain_id = chain_id;
        env.block_env.basefee = REPLAY_BASEFEE;
        env.block_env.gas_limit = 30_000_000;
        env
    }

    /// Builds an anchor-shaped transaction: golden touch calling the treasury at the given
    /// nonce, paying exactly the basefee.
    fn anchor_tx(treasury: Address, nonce: u64) -> TxEnv {
        TxEnv::builder()
            .caller(Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS))
            .kind(TxKind::Call(treasury))
            .nonce(nonce)
            .gas_limit(1_000_000)
            .gas_price(u128::from(REPLAY_BASEFEE))
            .chain_id(None)
            .build()
            .expect("valid anchor tx env")
    }

    /// Builds a plain funded-user transfer paying exactly the basefee.
    fn user_tx(caller: Address, nonce: u64) -> TxEnv {
        TxEnv::builder()
            .caller(caller)
            .kind(TxKind::Call(Address::with_last_byte(0xB0)))
            .nonce(nonce)
            .gas_limit(21_000)
            .gas_price(u128::from(REPLAY_BASEFEE))
            .chain_id(None)
            .build()
            .expect("valid user tx env")
    }

    /// Seeds a database with the golden-touch account at the given nonce plus an empty
    /// treasury account, mirroring on-chain pre-block state.
    fn replay_db(golden_touch_nonce: u64, treasury: Address) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS),
            AccountInfo {
                nonce: golden_touch_nonce,
                balance: U256::from(GOLDEN_TOUCH_DUST_BALANCE),
                ..Default::default()
            },
        );
        db.insert_account_info(treasury, AccountInfo::default());
        db
    }

    #[test]
    fn replayed_anchor_transaction_executes_without_prior_system_call() {
        // RPC trace/replay paths (`debug_trace*`, `trace_*`) create the EVM straight from the
        // factory and never issue the anchor system call, so the anchor exemption must be
        // derivable from the transaction itself plus database state.
        let chain_id = 167_000;
        let golden_touch = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        let treasury = get_treasury_address(chain_id);

        let mut evm =
            TaikoEvmFactory::default().create_evm(replay_db(7, treasury), replay_env(chain_id));

        let result = evm
            .transact(anchor_tx(treasury, 7))
            .expect("anchor must execute during replay without a prior anchor system call");
        assert!(result.result.is_success(), "anchor replay must succeed: {:?}", result.result);

        let golden_touch_state =
            result.state.get(&golden_touch).expect("golden touch must appear in the state");
        assert_eq!(
            golden_touch_state.info.balance,
            U256::from(GOLDEN_TOUCH_DUST_BALANCE),
            "anchor must not pay fees during replay"
        );
        assert_eq!(golden_touch_state.info.nonce, 8, "anchor must bump the golden touch nonce");
    }

    #[test]
    fn derived_anchor_context_only_exempts_the_snapshot_nonce() {
        // The derived context snapshots the golden-touch nonce before the anchor runs. A
        // crafted golden-touch -> treasury transaction later in the same block is a normal
        // transaction under consensus, so the replay must apply the balance check to it.
        let chain_id = 167_000;
        let treasury = get_treasury_address(chain_id);

        let mut evm =
            TaikoEvmFactory::default().create_evm(replay_db(7, treasury), replay_env(chain_id));

        evm.transact_commit(anchor_tx(treasury, 7)).expect("anchor must execute during replay");

        let err = evm
            .transact(anchor_tx(treasury, 8))
            .expect_err("a follow-up golden-touch tx must not inherit the anchor exemption");
        assert!(
            matches!(err, EVMError::Transaction(InvalidTransaction::LackOfFundForMaxFee { .. })),
            "expected a balance-check failure, got: {err:?}"
        );
    }

    #[test]
    fn derived_anchor_context_does_not_apply_base_fee_sharing() {
        // The basefee-share percentage comes from block extra data, which only the
        // authoritative anchor system call knows. A derived replay context must keep the
        // pre-existing replay behavior of not redistributing basefee income.
        let chain_id = 167_000;
        let treasury = get_treasury_address(chain_id);
        let alice = Address::with_last_byte(0xA1);

        let mut db = replay_db(7, treasury);
        db.insert_account_info(
            alice,
            AccountInfo { balance: U256::from(10).pow(U256::from(18)), ..Default::default() },
        );
        let mut evm = TaikoEvmFactory::default().create_evm(db, replay_env(chain_id));

        evm.transact_commit(anchor_tx(treasury, 7)).expect("anchor must execute during replay");

        let result = evm.transact(user_tx(alice, 0)).expect("funded user tx must execute");
        assert!(result.result.is_success(), "user tx must succeed: {:?}", result.result);
        assert!(
            result.state.get(&treasury).is_none_or(|account| account.info.balance.is_zero()),
            "derived context must not route basefee income to the treasury"
        );
    }

    #[test]
    fn fresh_replay_evm_still_exempts_crafted_golden_touch_tx_after_anchor() {
        // KNOWN LIMITATION, deliberately pinned: reth's tracing helpers create a fresh EVM per
        // transaction (`debug_traceBlock*`) or for the traced target (`debug_traceTransaction`),
        // sharing only the database between instances. A fresh EVM created after the real
        // anchor committed sees the advanced golden-touch nonce and re-derives the anchor
        // context from it, so a deliberately crafted golden-touch -> treasury transaction
        // placed later in the block is wrongly exempted here, while consensus executes it as a
        // fee-paying normal transaction. Real anchors are unaffected in every topology: the
        // anchor system-call marker commits no state, so a block-start EVM always sees the
        // pre-anchor nonce.
        //
        // Closing this requires the block-start golden-touch nonce (block-position metadata)
        // to be carried through the EVM environment. When that lands, this test must flip to
        // assert that the balance check applies to the crafted transaction again.
        let chain_id = 167_000;
        let golden_touch = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        let treasury = get_treasury_address(chain_id);

        // EVM #1 replays the real anchor (nonce 7) and commits it, advancing the golden-touch
        // nonce to 8 in the shared database.
        let mut evm =
            TaikoEvmFactory::default().create_evm(replay_db(7, treasury), replay_env(chain_id));
        evm.transact_commit(anchor_tx(treasury, 7)).expect("anchor must execute during replay");
        let (db, env) = evm.finish();

        // EVM #2 mirrors tracing a later crafted golden-touch -> treasury transaction: a fresh
        // instance over the same database. The snapshot protection covered by
        // `derived_anchor_context_only_exempts_the_snapshot_nonce` does not carry over.
        let mut fresh_evm = TaikoEvmFactory::default().create_evm(db, env);
        let result = fresh_evm
            .transact(anchor_tx(treasury, 8))
            .expect("fresh EVM wrongly exempts the crafted tx (see known limitation above)");
        assert!(result.result.is_success(), "crafted tx executes: {:?}", result.result);
        assert_eq!(
            result.state.get(&golden_touch).expect("golden touch state").info.balance,
            U256::from(GOLDEN_TOUCH_DUST_BALANCE),
            "fresh EVM grants the fee exemption; consensus would charge gas fees here"
        );
    }

    #[test]
    fn system_call_context_shares_base_fee_for_regular_transactions() {
        // The authoritative context installed by the anchor system call must keep sharing
        // basefee income between coinbase and treasury for non-anchor transactions.
        let chain_id = 167_000;
        let golden_touch = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        let treasury = get_treasury_address(chain_id);
        let alice = Address::with_last_byte(0xA1);

        let mut db = replay_db(7, treasury);
        db.insert_account_info(
            alice,
            AccountInfo { balance: U256::from(10).pow(U256::from(18)), ..Default::default() },
        );
        let mut evm = TaikoEvmFactory::default().create_evm(db, replay_env(chain_id));

        evm.transact_system_call(golden_touch, treasury, encode_anchor_system_call_data(25, 7))
            .expect("anchor system call must succeed");

        let result = evm.transact(user_tx(alice, 0)).expect("funded user tx must execute");
        assert!(result.result.is_success(), "user tx must succeed: {:?}", result.result);

        let base_fee_income = U256::from(21_000u64) * U256::from(REPLAY_BASEFEE);
        let coinbase_share = base_fee_income * U256::from(25u64) / U256::from(100u64);
        assert_eq!(
            result.state.get(&treasury).expect("treasury must be rewarded").info.balance,
            base_fee_income - coinbase_share,
            "treasury must receive the non-coinbase share of the basefee income"
        );
    }
}
