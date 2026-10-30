//! Taiko block executor integrating anchor pre-execution and tx filtering.
#[cfg(feature = "prover")]
use alloy_consensus::transaction::Recovered;
use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::{Encodable2718, eip7685::Requests};
use alloy_evm::{
    FromRecoveredTx, FromTxWithEncoded, RecoveredTx,
    block::GasOutput,
    eth::{EthTxResult, receipt_builder::ReceiptBuilder},
};
use alloy_primitives::{Address, Bytes, Log, U256, Uint};
use reth_evm::{
    Evm,
    block::{
        BlockExecutionError, BlockExecutor, BlockValidationError, CommitChanges, ExecutableTx,
        InternalBlockExecutionError, StateDB, SystemCaller,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
};
use reth_execution_types::BlockExecutionResult;
use reth_revm::context::{Block as _, result::ResultAndState};
use revm_database_interface::{Database, DatabaseCommit};

use crate::factory::TaikoBlockExecutionCtx;
use alethia_reth_chainspec::spec::TaikoExecutorSpec;
use alethia_reth_evm::{
    alloy::{TAIKO_GOLDEN_TOUCH_ADDRESS, TaikoAnchorEvm, TaikoZkGasEvm},
    handler::get_treasury_address,
    zk_gas::{
        adapter::ZK_GAS_LIMIT_ERR,
        meter::ZkGasOutcome,
        pricing::{TransactionPayloadSelectors, ZkGasMode, ZkGasShadowReport, ZkGasShadowStatus},
    },
};
use alethia_reth_primitives::decode_shasta_basefee_sharing_pctg;

/// Block execution artifacts for transactions that were accepted by prover filtering.
#[cfg(feature = "prover")]
#[derive(Debug)]
pub struct CommittedBlockExecutionOutcome<T, R> {
    /// Execution result after applying all committed transactions.
    pub execution_result: BlockExecutionResult<R>,
    /// Transactions that were accepted by the executor and included in execution.
    pub committed_transactions: Vec<Recovered<T>>,
    /// Non-enforcing block-local pricing report when Shadow mode is enabled.
    pub zk_gas_shadow_report: Option<ZkGasShadowReport>,
    /// Non-enforcing report across scopes begun before revm validation.
    ///
    /// Includes validation failures and declared access lists, but excludes executor rejections
    /// for remaining native block gas or intrinsic zk gas, which occur before the scope begins.
    pub attempted_zk_gas_shadow_report: Option<ZkGasShadowReport>,
    /// Number of attempted transaction scopes, including the anchor transaction.
    ///
    /// Zero when Shadow mode is disabled or its collector is unavailable.
    pub attempted_transaction_count: u64,
}

/// Dedicated block-execution error raised when a block hits the zk gas limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZkGasLimitExceeded;

impl std::fmt::Display for ZkGasLimitExceeded {
    /// Formats the dedicated zk gas limit error message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(ZK_GAS_LIMIT_ERR)
    }
}

impl std::error::Error for ZkGasLimitExceeded {}

/// Dedicated block-execution error raised when imported `header.difficulty` does not match
/// the recomputed finalized block zk gas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZkGasDifficultyMismatch {
    /// Difficulty carried by the imported block header.
    pub expected: U256,
    /// Finalized block zk gas recomputed during execution.
    pub got: U256,
}

impl std::fmt::Display for ZkGasDifficultyMismatch {
    /// Formats the dedicated zk gas difficulty mismatch message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "zk gas header difficulty mismatch: expected {}, got {}", self.expected, self.got)
    }
}

impl std::error::Error for ZkGasDifficultyMismatch {}

/// Returns `true` when `error` represents the dedicated zk gas truncation condition.
pub fn is_zk_gas_limit_exceeded(error: &BlockExecutionError) -> bool {
    match error {
        BlockExecutionError::Internal(err) => err.is_other::<ZkGasLimitExceeded>(),
        _ => false,
    }
}

/// Returns `true` when `error` represents a zk gas difficulty commitment mismatch.
pub fn is_zk_gas_difficulty_mismatch(error: &BlockExecutionError) -> bool {
    match error {
        BlockExecutionError::Internal(err) => err.is_other::<ZkGasDifficultyMismatch>(),
        _ => false,
    }
}

/// Returns `true` when `error` is the kind of failure that prover-style execution tolerates for a
/// non-anchor transaction by skipping it: zk gas truncation, an invalid transaction, or a gas limit
/// exceeding the block's remaining gas.
///
/// This is the single definition of the recoverable error set, shared by the prover executor's
/// `try_execute_filtered` and the tx-list witness debug RPC, so the two cannot drift when a new
/// recoverable variant is added. Callers remain responsible for never applying it to the anchor
/// transaction, which must always be fatal.
pub fn is_recoverable_non_anchor_tx_error(error: &BlockExecutionError) -> bool {
    is_zk_gas_limit_exceeded(error) ||
        matches!(
            error,
            BlockExecutionError::Validation(
                BlockValidationError::InvalidTx { .. } |
                    BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. }
            )
        )
}

/// Block executor for Taiko network.
pub struct TaikoBlockExecutor<'a, Evm, Spec, R: ReceiptBuilder> {
    /// Reference to the specification object.
    spec: Spec,

    /// Context for block execution.
    pub ctx: TaikoBlockExecutionCtx<'a>,
    /// Inner EVM.
    evm: Evm,
    /// Utility to call system smart contracts.
    system_caller: SystemCaller<Spec>,
    /// Receipt builder.
    receipt_builder: R,

    /// Receipts of executed transactions.
    receipts: Vec<R::Receipt>,
    /// Total gas used by transactions in this block.
    gas_used: u64,
    /// Flag indicating that zk gas exhausted the block and later transactions must not run.
    zk_gas_exhausted: bool,
    /// Flag indicating whether the executor has been initialized with the anchor transaction info
    /// in `apply_pre_execution_changes`.
    evm_extra_execution_ctx_initialized: bool,
    /// Non-consensus pricing collection mode selected by runtime configuration.
    zk_gas_mode: ZkGasMode,
}

impl<'a, Evm, Spec, R> TaikoBlockExecutor<'a, Evm, Spec, R>
where
    Spec: Clone,
    R: ReceiptBuilder,
{
    /// Creates a new [`TaikoBlockExecutor`]
    pub fn new(evm: Evm, ctx: TaikoBlockExecutionCtx<'a>, spec: Spec, receipt_builder: R) -> Self
    where
        Evm: TaikoAnchorEvm + TaikoZkGasEvm,
    {
        Self::new_with_zk_gas_mode(evm, ctx, spec, receipt_builder, ZkGasMode::Disabled)
    }

    /// Creates a block executor with an explicit non-consensus pricing collection mode.
    pub fn new_with_zk_gas_mode(
        mut evm: Evm,
        ctx: TaikoBlockExecutionCtx<'a>,
        spec: Spec,
        receipt_builder: R,
        zk_gas_mode: ZkGasMode,
    ) -> Self
    where
        Evm: TaikoAnchorEvm + TaikoZkGasEvm,
    {
        // The executor installs the authoritative anchor context through the anchor system
        // call in `apply_pre_execution_changes`; replay-only derivation must stay off so a
        // missing initialization keeps failing loudly.
        evm.set_anchor_ctx_derivation_enabled(false);
        // The executor owns the per-transaction zk gas bracket (reset, intrinsic charge,
        // commit) in `execute_transaction_without_commit`; the wrapper's per-transact entry
        // reset must stay off or it would wipe the intrinsic charged before `transact` runs.
        evm.set_per_transact_zk_gas_reset_enabled(false);
        evm.set_zk_gas_pricing_enabled(zk_gas_mode == ZkGasMode::Shadow);
        Self {
            evm,
            ctx,
            receipts: Vec::new(),
            gas_used: 0,
            zk_gas_exhausted: false,
            system_caller: SystemCaller::new(spec.clone()),
            spec,
            receipt_builder,
            evm_extra_execution_ctx_initialized: false,
            zk_gas_mode,
        }
    }

    /// Returns the current non-enforcing pricing Shadow report, or `None` when collection is
    /// disabled.
    pub fn zk_gas_shadow_report(&self) -> Option<ZkGasShadowReport>
    where
        Evm: TaikoZkGasEvm,
    {
        match self.zk_gas_mode {
            ZkGasMode::Disabled => None,
            ZkGasMode::Shadow => Some(
                self.evm
                    .zk_gas_pricing_snapshot()
                    .map(|snapshot| snapshot.shadow_report())
                    .unwrap_or_else(ZkGasShadowReport::collector_unavailable),
            ),
        }
    }

    /// Returns the pricing report and count across scopes begun before revm validation.
    ///
    /// Revm validation failures and declared access lists are included. Remaining-block-gas and
    /// intrinsic-zk-gas precheck failures are excluded because they happen before `transact_raw`.
    pub fn attempted_zk_gas_shadow_report(&self) -> Option<(ZkGasShadowReport, u64)>
    where
        Evm: TaikoZkGasEvm,
    {
        match self.zk_gas_mode {
            ZkGasMode::Disabled => None,
            ZkGasMode::Shadow => self
                .evm
                .zk_gas_attempted_pricing_snapshot()
                .map(|snapshot| (snapshot.block.shadow_report(), snapshot.transaction_count))
                .or_else(|| Some((ZkGasShadowReport::collector_unavailable(), 0))),
        }
    }

    /// Emits one bounded-cardinality block-level Shadow observation.
    fn record_zk_gas_shadow_report(&self)
    where
        Evm: TaikoZkGasEvm,
    {
        let Some(report) = self.zk_gas_shadow_report() else { return };
        match report.status() {
            ZkGasShadowStatus::Incomplete { known, .. } => tracing::info!(
                target: "zkgas",
                model_id = report.model_id(),
                status = report.status().label(),
                evm_local_known_subtotal_pgu = ?known.evm_local_known_subtotal_pgu(),
                fixed_scaled = ?known.fixed_scaled(),
                authorization_validations = known.authorization_validations(),
                authorization_scaled = ?known.authorization_scaled(),
                operations_scaled = ?known.operations_scaled(),
                evm_local_resources_scaled = ?known.evm_local_resources_scaled(),
                "zk gas pricing shadow block report"
            ),
            ZkGasShadowStatus::Diagnostic(_) => tracing::warn!(
                target: "zkgas",
                model_id = report.model_id(),
                status = report.status().label(),
                "zk gas pricing shadow block diagnostic"
            ),
        }
    }

    /// Returns the dedicated truncation error used when zk gas exhausts the block.
    fn zk_gas_limit_error() -> BlockExecutionError {
        BlockExecutionError::other(ZkGasLimitExceeded)
    }

    /// Synchronizes the finalized zk gas total from the EVM meter into the execution
    /// context that the assembler later reads.
    fn sync_finalized_block_zk_gas(&self)
    where
        Evm: TaikoZkGasEvm,
    {
        if let Some(zk_gas) = self.evm.block_zk_gas_used() {
            self.ctx.set_finalized_block_zk_gas(zk_gas);
        }
    }

    /// Reserves finalized block zk gas without executing or committing a transaction.
    ///
    /// This supports simulations that need to preserve budget for a mandatory transaction they
    /// cannot execute. Pre-Unzen EVMs have no meter, so the reservation is a successful no-op.
    pub fn reserve_block_zk_gas(&mut self, amount: u64) -> Result<(), BlockExecutionError>
    where
        Evm: TaikoZkGasEvm,
    {
        match self.evm.reserve_block_zk_gas(amount) {
            Ok(Some(zk_gas)) => self.ctx.set_finalized_block_zk_gas(zk_gas),
            Ok(None) => {}
            Err(ZkGasOutcome::LimitExceeded) => return Err(Self::zk_gas_limit_error()),
        }
        Ok(())
    }

    /// Clears in-flight metered zk gas while preserving the committed block total.
    ///
    /// An installed Shadow selector scope is retained in its attempted prefix.
    fn reset_current_transaction_zk_gas(&mut self)
    where
        Evm: TaikoZkGasEvm,
    {
        self.evm.reset_transaction_zk_gas();
        self.sync_finalized_block_zk_gas();
    }

    /// Commits the current transaction's zk gas and publishes the updated block total.
    ///
    /// # Panics
    ///
    /// If committing would exceed the block zk gas budget. [`BlockExecutor::commit_transaction`]
    /// is infallible in alloy-evm 0.39, so `execute_transaction_without_commit` pre-checks the
    /// budget while the failure can still be reported; reaching the exceeded branch here means
    /// the executor was driven with a result that skipped that check.
    fn commit_current_transaction_zk_gas(&mut self)
    where
        Evm: TaikoZkGasEvm,
    {
        self.evm
            .commit_transaction_zk_gas_pricing()
            .expect("validated zk gas pricing selectors must commit atomically");
        match self.evm.commit_transaction_zk_gas() {
            Ok(Some(zk_gas)) => self.ctx.set_finalized_block_zk_gas(zk_gas),
            Ok(None) => {}
            Err(ZkGasOutcome::LimitExceeded) => unreachable!(
                "zk gas commit exceeded the block budget; \
                 execute_transaction_without_commit must pre-check the budget"
            ),
        }
    }

    /// Validates the imported header difficulty, when present, against the finalized block
    /// zk gas recomputed by execution.
    fn validate_expected_zk_gas_difficulty(&self) -> Result<(), BlockExecutionError> {
        let Some(expected) = self.ctx.expected_difficulty() else { return Ok(()) };
        let got = U256::from(self.ctx.finalized_block_zk_gas());
        if got == expected {
            return Ok(());
        }

        Err(BlockExecutionError::other(ZkGasDifficultyMismatch { expected, got }))
    }
}

#[cfg(feature = "prover")]
impl<E, Spec, R> TaikoBlockExecutor<'_, E, Spec, R>
where
    E: Evm<
            DB: StateDB + DatabaseCommit,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
        > + TaikoZkGasEvm,
    Spec: TaikoExecutorSpec + Clone,
    R: ReceiptBuilder<
            Transaction: Transaction + Encodable2718 + Clone,
            Receipt: TxReceipt<Log = Log>,
        >,
    <R::Transaction as TransactionEnvelope>::TxType: Send + 'static,
{
    /// Executes a prover candidate block and returns the transactions that were actually committed.
    pub fn execute_block_with_committed_transactions<'tx>(
        mut self,
        transactions: impl IntoIterator<Item = Recovered<&'tx R::Transaction>>,
    ) -> Result<CommittedBlockExecutionOutcome<R::Transaction, R::Receipt>, BlockExecutionError>
    where
        R::Transaction: 'tx,
    {
        self.apply_pre_execution_changes()?;

        let mut committed_transactions = Vec::new();
        for (idx, tx) in transactions.into_iter().enumerate() {
            let is_anchor_transaction = idx == 0;
            if !is_anchor_transaction && tx.signer() == Address::ZERO {
                continue;
            }

            let committed_tx = Recovered::new_unchecked((*tx.inner()).clone(), tx.signer());
            if self.try_execute_filtered(tx, is_anchor_transaction)? {
                committed_transactions.push(committed_tx);
            }
            if self.zk_gas_exhausted {
                break;
            }
        }

        let zk_gas_shadow_report = self.zk_gas_shadow_report();
        let (attempted_zk_gas_shadow_report, attempted_transaction_count) = self
            .attempted_zk_gas_shadow_report()
            .map_or((None, 0), |(report, count)| (Some(report), count));
        let execution_result = self.apply_post_execution_changes()?;
        Ok(CommittedBlockExecutionOutcome {
            execution_result,
            committed_transactions,
            zk_gas_shadow_report,
            attempted_zk_gas_shadow_report,
            attempted_transaction_count,
        })
    }
}

/// Shared prover-mode filter: executes a single transaction and classifies the failure.
///
/// Lives on the inherent impl so both [`Self::execute_block_with_committed_transactions`] and the
/// trait-level [`BlockExecutor::execute_block`] route through one place — the filtering rules
/// (zk gas truncation, invalid tx, gas-exceeds-block) cannot drift between the two prover entry
/// points.
#[cfg(feature = "prover")]
impl<E, Spec, R> TaikoBlockExecutor<'_, E, Spec, R>
where
    E: Evm<
            DB: StateDB + DatabaseCommit,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
        > + TaikoZkGasEvm,
    Spec: TaikoExecutorSpec + Clone,
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
    <R::Transaction as TransactionEnvelope>::TxType: Send + 'static,
{
    /// Executes `tx` and returns `Ok(true)` if it committed, `Ok(false)` if it was filtered as a
    /// recoverable non-anchor failure, or `Err` for fatal / anchor errors.
    fn try_execute_filtered(
        &mut self,
        tx: impl ExecutableTx<Self>,
        is_anchor_transaction: bool,
    ) -> Result<bool, BlockExecutionError> {
        match self.execute_transaction(tx) {
            Ok(_) => Ok(true),
            // We don't allow the anchor transaction to be discarded even if it would otherwise be a
            // recoverable failure; this should never happen in practice.
            Err(err) if !is_anchor_transaction && is_recoverable_non_anchor_tx_error(&err) => {
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }
}

impl<E, Spec, R> BlockExecutor for TaikoBlockExecutor<'_, E, Spec, R>
where
    E: Evm<
            DB: StateDB + DatabaseCommit,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
        > + TaikoZkGasEvm,
    Spec: TaikoExecutorSpec + Clone,
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
    <R::Transaction as TransactionEnvelope>::TxType: Send + 'static,
{
    /// Input transaction type.
    type Transaction = R::Transaction;
    /// Receipt type this executor produces.
    type Receipt = R::Receipt;
    /// EVM used by the executor.
    type Evm = E;
    /// Result of transaction execution.
    type Result = EthTxResult<E::HaltReason, <R::Transaction as TransactionEnvelope>::TxType>;

    /// Applies any necessary changes before executing the block's transactions.
    /// NOTE: Here we use a system call to set the Anchor transact sender account information and
    /// decode the base fee share percentage from the block's extra data.
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.system_caller.apply_blockhashes_contract_call(self.ctx.parent_hash, &mut self.evm)?;
        self.system_caller
            .apply_beacon_root_contract_call(self.ctx.parent_beacon_block_root, &mut self.evm)?;

        // Initialize the golden touch address nonce if it is not already set.
        if !self.evm_extra_execution_ctx_initialized {
            let account_info =
                self.evm.db_mut().basic(Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS)).map_err(
                    |e| BlockExecutionError::Internal(InternalBlockExecutionError::Other(e.into())),
                )?;

            // Decode the base fee share percentage from the block's extra data.
            let base_fee_share_pgtg =
                if self.spec.is_shasta_active(self.evm.block().timestamp().to()) {
                    decode_shasta_basefee_sharing_pctg(self.ctx.extra_data.as_ref()) as u64
                } else if self.spec.is_ontake_active_at_block(self.evm.block().number().to()) {
                    decode_post_ontake_extra_data(self.ctx.extra_data.clone())
                } else {
                    0
                };

            self.evm
                .transact_system_call(
                    Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS),
                    get_treasury_address(self.evm().chain_id()),
                    encode_anchor_system_call_data(
                        base_fee_share_pgtg,
                        account_info.map_or(0, |account| account.nonce),
                    ),
                )
                .map_err(|e| {
                    BlockExecutionError::Internal(InternalBlockExecutionError::Other(e.into()))
                })?;

            self.evm_extra_execution_ctx_initialized = true;
        }

        Ok(())
    }

    /// Executes a transaction and returns the resulting state diff without persisting it; the
    /// caller decides whether to commit.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&Self::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let output = self.execute_transaction_without_commit(tx)?;

        if !f(&output).should_commit() {
            self.reset_current_transaction_zk_gas();
            return Ok(None);
        }

        Ok(Some(self.commit_transaction(output)))
    }

    /// Executes a transaction and returns the resulting state diff without persisting it; the
    /// caller decides whether to commit.
    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        if self.zk_gas_exhausted {
            return Err(Self::zk_gas_limit_error());
        }

        let (tx_env, tx) = tx.into_parts();

        // The sum of the transaction's gas limit, Tg, and the gas utilized in this block prior,
        // must be no greater than the block's gasLimit.
        let block_available_gas = self.evm.block().gas_limit() - self.gas_used;

        if tx.tx().gas_limit() > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: tx.tx().gas_limit(),
                block_available_gas,
            }
            .into());
        }

        self.reset_current_transaction_zk_gas();

        // Charge the per-transaction intrinsic zk gas before EVM execution begins, as required
        // by the Unzen zk gas spec (taikoxyz/taiko-mono#21669). The intrinsic sits in the
        // in-flight tx total so it is committed alongside opcode/precompile usage for every valid
        // included receipt, including REVERT and HALT. It is discarded only when the executor
        // omits the transaction (validation/execution error, budget rejection, or commit predicate
        // false). A schedule with a zero intrinsic makes this a no-op.
        // If the intrinsic alone would exceed the remaining block budget, mirror the
        // mid-tx exhaustion path used by the inspector.
        if let Err(ZkGasOutcome::LimitExceeded) = self.evm.charge_tx_intrinsic_zk_gas() {
            self.zk_gas_exhausted = true;
            self.reset_current_transaction_zk_gas();
            return Err(Self::zk_gas_limit_error());
        }

        // Only Shadow needs canonical signed-envelope metadata. Counting the encoded size
        // does not serialize/copy large calldata, and Disabled keeps the legacy fast path.
        let payload =
            (self.zk_gas_mode == ZkGasMode::Shadow).then(|| TransactionPayloadSelectors {
                canonical_transaction_bytes: u64::try_from(tx.tx().encode_2718_len())
                    .unwrap_or(u64::MAX),
                nonempty_payload_transactions: u64::from(!tx.tx().input().is_empty()),
            });
        let execution = self.evm.transact(tx_env);
        if let Some(payload) = payload {
            self.evm.record_transaction_payload(payload);
        }
        let result = match execution {
            Ok(result) => result,
            Err(err) if err.to_string() == ZK_GAS_LIMIT_ERR => {
                self.zk_gas_exhausted = true;
                self.reset_current_transaction_zk_gas();
                return Err(Self::zk_gas_limit_error());
            }
            Err(err) => {
                self.reset_current_transaction_zk_gas();
                return Err(BlockExecutionError::evm(err, tx.tx().trie_hash()));
            }
        };

        // The trait's `commit_transaction` is infallible in alloy-evm 0.39, so the block zk gas
        // budget must be checked here, where truncation can still be reported. The in-flight
        // total is final at this point — nothing meters between execution and commit.
        if self.evm.transaction_zk_gas_commit_would_exceed() {
            self.zk_gas_exhausted = true;
            self.reset_current_transaction_zk_gas();
            return Err(Self::zk_gas_limit_error());
        }

        Ok(EthTxResult {
            result,
            blob_gas_used: tx.tx().blob_gas_used().unwrap_or_default(),
            tx_type: tx.tx().tx_type(),
        })
    }

    /// Commits a previously executed transaction: updates receipts, gas accounting, and writes the
    /// buffered state changes to the database.
    ///
    /// `output` must be a result produced by `execute_transaction_without_commit` on this
    /// executor: that is the only place the block zk gas budget is pre-checked while truncation
    /// can still be reported (committing is infallible), and
    /// `commit_current_transaction_zk_gas` panics on a result that skipped the check.
    ///
    /// State hooks fire from the `State` database on commit (reth v2.4.0 moved hook delivery off
    /// the block executor), so no explicit hook call is needed here.
    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        let EthTxResult { result: ResultAndState { result, state }, tx_type, .. } = output;

        let gas_used = result.tx_gas_used();
        self.commit_current_transaction_zk_gas();

        // append gas used
        self.gas_used += gas_used;

        // Push transaction changeset and calculate header bloom filter for receipt.
        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx_type,
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.gas_used,
        }));

        // Commit the state changes.
        self.evm.db_mut().commit(state);

        GasOutput::new(gas_used)
    }

    /// Applies any necessary changes after executing the block's transactions, completes execution
    /// and returns the underlying EVM along with execution result.
    fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        self.sync_finalized_block_zk_gas();
        self.validate_expected_zk_gas_difficulty()?;
        self.record_zk_gas_shadow_report();
        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Requests::default(),
                gas_used: self.gas_used,
                blob_gas_used: 0,
            },
        ))
    }

    /// Exposes mutable reference to EVM.
    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    /// Exposes immutable reference to EVM.
    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    /// Returns a reference to all recorded receipts.
    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }

    /// Executes all transactions in a block, applying pre and post execution changes.
    /// NOTE: For proving system, we skip the invalid transactions directly inside this function.
    #[cfg(feature = "prover")]
    fn execute_block(
        mut self,
        transactions: impl IntoIterator<Item = impl ExecutableTx<Self>>,
    ) -> Result<BlockExecutionResult<Self::Receipt>, BlockExecutionError>
    where
        Self: Sized,
    {
        self.apply_pre_execution_changes()?;

        for (idx, tx) in transactions.into_iter().enumerate() {
            let is_anchor_transaction = idx == 0;
            let (tx_env, tx) = tx.into_parts();
            // Check transaction signature at first, if invalid, skip it directly.
            if !is_anchor_transaction && *tx.signer() == Address::ZERO {
                continue;
            }
            self.try_execute_filtered((tx_env, tx), is_anchor_transaction)?;
            if self.zk_gas_exhausted {
                break;
            }
        }

        self.apply_post_execution_changes()
    }
}

/// Encode anchor pre-execution context for the treasury system call.
///
/// The payload is a fixed 16-byte big-endian blob (`u64 base_fee_share_pctg || u64 caller_nonce`)
/// that the EVM-side system-call hook decodes before transaction execution.
fn encode_anchor_system_call_data(base_fee_share_pctg: u64, caller_nonce: u64) -> Bytes {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&base_fee_share_pctg.to_be_bytes());
    buf[8..].copy_from_slice(&caller_nonce.to_be_bytes());
    Bytes::copy_from_slice(&buf)
}

/// Decode post-Ontake `extra_data` into the configured base-fee sharing percentage.
///
/// Ontake+ blocks store the percentage as a `uint256`; only the low 64 bits are consumed here.
fn decode_post_ontake_extra_data(extradata: Bytes) -> u64 {
    let value = Uint::<256, 4>::from_be_slice(&extradata);
    value.as_limbs()[0]
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use alloy_consensus::{Signed, TxLegacy};
    use alloy_evm::EvmFactory;
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U64, U256};
    use reth_ethereum_primitives::TransactionSigned;
    use reth_evm::{ConfigureEvm, block::BlockExecutor};
    use reth_evm_ethereum::RethReceiptBuilder;
    use reth_primitives_traits::SignedTransaction;
    use reth_revm::{
        State,
        db::{CacheDB, EmptyDB},
        state::{AccountInfo, Bytecode, bytecode::opcode},
    };

    use alethia_reth_evm::{
        alloy::{TaikoZkGasEvm, decode_anchor_system_call_data},
        factory::TaikoEvmFactory,
        spec::TaikoSpecId,
        zk_gas::{
            pricing::{ZkGasIncompleteness, ZkGasMode, ZkGasShadowStatus},
            unzen::{BLOCK_ZK_GAS_LIMIT, TX_INTRINSIC_ZK_GAS},
        },
    };

    use super::*;
    use crate::{
        config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
        testutil::{
            BENCH_LIMIT_TARGET, BENCH_SUCCESS_TARGET, db_with_contracts, insert_contract,
            recovered_tx, unzen_chain_spec, unzen_evm_env, unzen_execution_ctx,
        },
    };
    use alethia_reth_chainspec::spec::TaikoChainSpec;
    const BENCH_CALLER: Address = Address::with_last_byte(0x30);

    #[test]
    fn shadow_mode_preserves_canonical_execution_and_unzen_difficulty_input() {
        fn run(mode: ZkGasMode) -> (BlockExecutionResult<reth_ethereum_primitives::Receipt>, u64) {
            let chain_spec = Arc::new(unzen_chain_spec());
            let mut state = State::builder()
                .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
                .with_bundle_update()
                .build();
            let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
            let ctx = unzen_execution_ctx();
            let mut executor = TaikoBlockExecutor::new_with_zk_gas_mode(
                evm,
                ctx.clone(),
                chain_spec,
                RethReceiptBuilder::default(),
                mode,
            );
            executor
                .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1))
                .expect("transaction executes");

            match mode {
                ZkGasMode::Disabled => {
                    assert!(executor.zk_gas_shadow_report().is_none());
                }
                ZkGasMode::Shadow => {
                    let report = executor.zk_gas_shadow_report().expect("shadow report");
                    assert!(matches!(
                        report.status(),
                        ZkGasShadowStatus::Incomplete {
                            missing:
                                ZkGasIncompleteness::ExactBlockResourcesAndMptSelectorsUnavailable,
                            ..
                        }
                    ));
                }
            }

            let (_, result) = executor.finish().expect("executor finishes");
            (result, ctx.finalized_block_zk_gas())
        }

        let (disabled, disabled_difficulty) = run(ZkGasMode::Disabled);
        let (shadow, shadow_difficulty) = run(ZkGasMode::Shadow);
        assert_eq!(shadow.receipts, disabled.receipts);
        assert_eq!(shadow.requests, disabled.requests);
        assert_eq!(shadow.gas_used, disabled.gas_used);
        assert_eq!(shadow.blob_gas_used, disabled.blob_gas_used);
        assert_eq!(shadow_difficulty, disabled_difficulty);
    }

    #[test]
    fn shadow_cross_executor_commit_retains_a_diagnostic_instead_of_panicking() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut producer_state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let producer_evm = TaikoEvmFactory.create_evm(&mut producer_state, unzen_evm_env());
        let mut producer = TaikoBlockExecutor::new_with_zk_gas_mode(
            producer_evm,
            unzen_execution_ctx(),
            chain_spec.clone(),
            RethReceiptBuilder::default(),
            ZkGasMode::Shadow,
        );
        let output = producer
            .execute_transaction_without_commit(recovered_tx(
                BENCH_CALLER,
                BENCH_SUCCESS_TARGET,
                0,
                1,
            ))
            .expect("BAL worker executes transaction");

        let mut consumer_state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let consumer_evm = TaikoEvmFactory.create_evm(&mut consumer_state, unzen_evm_env());
        let mut consumer = TaikoBlockExecutor::new_with_zk_gas_mode(
            consumer_evm,
            unzen_execution_ctx(),
            chain_spec,
            RethReceiptBuilder::default(),
            ZkGasMode::Shadow,
        );

        consumer.commit_transaction(output);
        assert!(matches!(
            consumer.zk_gas_shadow_report().expect("shadow report").status(),
            ZkGasShadowStatus::Diagnostic(
                alethia_reth_evm::zk_gas::pricing::ZkGasShadowDiagnostic::Collector(
                    alethia_reth_evm::zk_gas::pricing::ZkGasAccumulatorError::NoActiveTransaction,
                )
            )
        ));
        let (attempted, attempted_count) = consumer
            .attempted_zk_gas_shadow_report()
            .expect("attempted shadow report remains available");
        assert_eq!(attempted_count, 0);
        assert!(matches!(
            attempted.status(),
            ZkGasShadowStatus::Diagnostic(
                alethia_reth_evm::zk_gas::pricing::ZkGasShadowDiagnostic::Collector(
                    alethia_reth_evm::zk_gas::pricing::ZkGasAccumulatorError::NoActiveTransaction,
                )
            )
        ));
    }

    #[test]
    fn test_encode_anchor_system_call_data() {
        let base_fee_share_pctg = U64::random().to::<u64>();
        let caller_nonce = U64::random().to::<u64>();
        let encoded_data = encode_anchor_system_call_data(base_fee_share_pctg, caller_nonce);
        assert_eq!(encoded_data.len(), 16);
        assert_eq!(&encoded_data[..8], &base_fee_share_pctg.to_be_bytes());
        assert_eq!(&encoded_data[8..], &caller_nonce.to_be_bytes());

        let (decoded_pctg, decoded_nonce) =
            decode_anchor_system_call_data(&encoded_data).expect("decoding should succeed");
        assert_eq!(decoded_pctg, base_fee_share_pctg);
        assert_eq!(decoded_nonce, caller_nonce);
    }

    #[test]
    fn test_decode_post_ontake_extra_data() {
        let base_fee_share_pctg = U64::random().to::<u64>();

        assert_eq!(
            decode_post_ontake_extra_data(Bytes::copy_from_slice(
                &U256::from_limbs([base_fee_share_pctg, 0, 0, 0]).to_be_bytes::<32>(),
            )),
            base_fee_share_pctg
        );
    }

    #[test]
    fn executor_reserves_block_zk_gas_and_synchronizes_execution_context() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state =
            State::builder().with_database(db_with_contracts(&[])).with_bundle_update().build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let ctx = unzen_execution_ctx();
        let mut executor =
            TaikoBlockExecutor::new(evm, ctx.clone(), chain_spec, RethReceiptBuilder::default());

        executor.reserve_block_zk_gas(2_000_000).expect("reserve should fit");

        assert_eq!(ctx.finalized_block_zk_gas(), 2_000_000);
    }

    #[test]
    fn executor_reserves_block_zk_gas_as_noop_without_meter() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state =
            State::builder().with_database(db_with_contracts(&[])).with_bundle_update().build();
        let mut env = unzen_evm_env();
        env.cfg_env.spec = TaikoSpecId::SHASTA;
        let evm = TaikoEvmFactory.create_evm(&mut state, env);
        let ctx = unzen_execution_ctx();
        let mut executor =
            TaikoBlockExecutor::new(evm, ctx.clone(), chain_spec, RethReceiptBuilder::default());

        executor.reserve_block_zk_gas(2_000_000).expect("pre-Unzen reserve should be a no-op");

        assert_eq!(ctx.finalized_block_zk_gas(), 0);
    }

    #[test]
    fn executor_discards_limit_exceeded_tx_and_stops_after_it() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        assert_eq!(evm.block_zk_gas_used(), Some(0));
        let ctx = unzen_execution_ctx();
        let mut executor = TaikoBlockExecutor::new_with_zk_gas_mode(
            evm,
            ctx.clone(),
            chain_spec.clone(),
            RethReceiptBuilder::default(),
            ZkGasMode::Shadow,
        );

        let gas_used = executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1))
            .expect("first transaction should commit");
        let finalized_after_first = ctx.finalized_block_zk_gas();
        assert!(finalized_after_first > 0);
        let pricing_after_first = executor.evm.zk_gas_pricing_snapshot().expect("pricing enabled");
        assert_eq!(pricing_after_first.started_non_anchor_transactions, 1);
        assert_eq!(
            executor
                .evm
                .zk_gas_attempted_pricing_snapshot()
                .expect("attempted pricing enabled")
                .transaction_count,
            1
        );

        let err = executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_LIMIT_TARGET, 1, 1))
            .expect_err("second transaction should be discarded");
        assert!(is_zk_gas_limit_exceeded(&err));
        assert_eq!(ctx.finalized_block_zk_gas(), finalized_after_first);
        assert_eq!(executor.evm.zk_gas_pricing_snapshot(), Some(pricing_after_first.clone()));
        let attempted_after_limit =
            executor.evm.zk_gas_attempted_pricing_snapshot().expect("attempted pricing enabled");
        assert_eq!(attempted_after_limit.transaction_count, 2);
        assert_ne!(attempted_after_limit.block, pricing_after_first);

        let repeated_err = executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 1, 1))
            .expect_err("later transactions should not execute after zk gas exhaustion");
        assert!(is_zk_gas_limit_exceeded(&repeated_err));
        assert_eq!(executor.evm.zk_gas_pricing_snapshot(), Some(pricing_after_first));
        assert_eq!(
            executor.evm.zk_gas_attempted_pricing_snapshot().expect("attempted pricing enabled"),
            attempted_after_limit
        );

        let (_, result) = executor.finish().expect("executor should finish after truncation");
        assert_eq!(result.receipts.len(), 1);
        assert_eq!(result.gas_used, gas_used.tx_gas_used());
    }

    #[test]
    fn executor_does_not_count_candidate_rejected_before_pricing_scope_begins() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let mut env = unzen_evm_env();
        env.block_env.gas_limit = 4_000_000;
        let evm = TaikoEvmFactory.create_evm(&mut state, env);
        let mut executor = TaikoBlockExecutor::new_with_zk_gas_mode(
            evm,
            unzen_execution_ctx(),
            chain_spec,
            RethReceiptBuilder::default(),
            ZkGasMode::Shadow,
        );

        let err = executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1))
            .expect_err("candidate exceeds remaining native block gas before execution");
        assert!(matches!(
            err,
            BlockExecutionError::Validation(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. }
            )
        ));
        let attempted =
            executor.evm.zk_gas_attempted_pricing_snapshot().expect("attempted pricing enabled");
        assert_eq!(attempted.transaction_count, 0);
        assert_eq!(attempted.block, Default::default());
    }

    #[test]
    fn executor_pricing_commits_included_halt_and_discards_predicate_and_invalid_transactions() {
        let halt_target = Address::with_last_byte(0x24);
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut db = db_with_contracts(&[(BENCH_CALLER, 0)]);
        insert_contract(&mut db, halt_target, Bytecode::new_raw(Bytes::from(vec![opcode::ADD])));
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let ctx = unzen_execution_ctx();
        let mut executor = TaikoBlockExecutor::new_with_zk_gas_mode(
            evm,
            ctx,
            chain_spec,
            RethReceiptBuilder::default(),
            ZkGasMode::Shadow,
        );

        let omitted = executor
            .execute_transaction_with_commit_condition(
                recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1),
                |_| CommitChanges::No,
            )
            .expect("predicate rejection is not an execution error");
        assert!(omitted.is_none());
        assert_eq!(
            executor
                .evm
                .zk_gas_attempted_pricing_snapshot()
                .expect("attempted pricing enabled")
                .transaction_count,
            1
        );
        assert_eq!(
            executor
                .evm
                .zk_gas_pricing_snapshot()
                .expect("pricing enabled")
                .started_non_anchor_transactions,
            0
        );

        executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 1, 1))
            .expect_err("invalid nonce is discarded");
        assert_eq!(
            executor
                .evm
                .zk_gas_attempted_pricing_snapshot()
                .expect("attempted pricing enabled")
                .transaction_count,
            2
        );
        assert_eq!(
            executor
                .evm
                .zk_gas_pricing_snapshot()
                .expect("pricing enabled")
                .started_non_anchor_transactions,
            0
        );

        executor
            .execute_transaction(recovered_tx(BENCH_CALLER, halt_target, 0, 1))
            .expect("valid HALT yields an included receipt");
        assert_eq!(
            executor
                .evm
                .zk_gas_attempted_pricing_snapshot()
                .expect("attempted pricing enabled")
                .transaction_count,
            3
        );
        let snapshot = executor.evm.zk_gas_pricing_snapshot().expect("pricing enabled");
        assert_eq!(snapshot.started_non_anchor_transactions, 1);
        assert!(snapshot.operation_routes.iter().any(|row| {
            row.route == alethia_reth_evm::zk_gas::pricing::OperationRoute::Opcode(opcode::ADD)
        }));
    }

    #[test]
    fn executor_does_not_count_intrinsic_zk_gas_rejection_before_pricing_scope_begins() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let mut executor = TaikoBlockExecutor::new_with_zk_gas_mode(
            evm,
            unzen_execution_ctx(),
            chain_spec,
            RethReceiptBuilder::default(),
            ZkGasMode::Shadow,
        );
        executor
            .reserve_block_zk_gas(BLOCK_ZK_GAS_LIMIT - TX_INTRINSIC_ZK_GAS + 1)
            .expect("reservation leaves less than one intrinsic charge");

        let err = executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1))
            .expect_err("intrinsic zk gas exceeds the remaining budget");
        assert!(is_zk_gas_limit_exceeded(&err));
        let attempted =
            executor.evm.zk_gas_attempted_pricing_snapshot().expect("attempted pricing enabled");
        assert_eq!(attempted.transaction_count, 0);
        assert_eq!(attempted.block, Default::default());
    }

    #[test]
    fn executor_includes_tx_intrinsic_in_committed_block_zk_gas() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let ctx = unzen_execution_ctx();
        let mut executor =
            TaikoBlockExecutor::new(evm, ctx.clone(), chain_spec, RethReceiptBuilder::default());

        executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1))
            .expect("successful tx should commit");

        let finalized = ctx.finalized_block_zk_gas();
        assert!(
            finalized >= TX_INTRINSIC_ZK_GAS,
            "finalized block zk gas ({finalized}) must include the per-tx intrinsic charge ({TX_INTRINSIC_ZK_GAS})"
        );
    }

    #[cfg(feature = "prover")]
    #[test]
    fn execute_block_stops_after_non_anchor_zk_gas_exhaustion() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let ctx = unzen_execution_ctx();
        let executor =
            TaikoBlockExecutor::new(evm, ctx.clone(), chain_spec, RethReceiptBuilder::default());

        let result = executor.execute_block(vec![
            recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1),
            recovered_tx(BENCH_CALLER, BENCH_LIMIT_TARGET, 1, 1),
            recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 2, 1),
        ]);

        let result = result.expect("prover execute_block should truncate instead of erroring");
        assert_eq!(result.receipts.len(), 1);
        assert!(result.gas_used > 0);
        assert!(ctx.finalized_block_zk_gas() > 0);
    }

    #[test]
    fn executor_rejects_imported_unzen_block_when_difficulty_mismatches_finalized_zk_gas() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let mut ctx = unzen_execution_ctx();
        ctx.expected_difficulty = Some(U256::ZERO);
        let mut executor =
            TaikoBlockExecutor::new(evm, ctx, chain_spec, RethReceiptBuilder::default());

        executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1))
            .expect("transaction should execute successfully");

        let err: BlockExecutionError = match executor.finish() {
            Ok(_) => panic!("imported Unzen blocks must reject difficulty mismatches"),
            Err(err) => err,
        };
        assert!(is_zk_gas_difficulty_mismatch(&err));
        assert!(err.to_string().contains("difficulty"));
    }

    #[test]
    fn is_recoverable_non_anchor_tx_error_classifies_recoverable_set() {
        let gas_err = BlockExecutionError::Validation(
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: 2,
                block_available_gas: 1,
            },
        );
        assert!(is_recoverable_non_anchor_tx_error(&gas_err));

        // A zk gas difficulty mismatch is fatal, not a recoverable per-transaction failure.
        let difficulty_err = BlockExecutionError::other(ZkGasDifficultyMismatch {
            expected: U256::from(1u64),
            got: U256::from(2u64),
        });
        assert!(!is_recoverable_non_anchor_tx_error(&difficulty_err));
    }

    #[test]
    fn test_apply_pre_execution_changes_initializes_anchor_context_from_account_nonce() {
        let chain_spec = Arc::new(TaikoChainSpec::default());
        let config = TaikoEvmConfig::new(chain_spec.clone());
        let golden_touch = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        let treasury = get_treasury_address(chain_spec.inner.chain().id());
        let nonce = 7;

        let make_executor = || {
            let mut db = CacheDB::<EmptyDB>::default();
            db.insert_account_info(
                golden_touch,
                AccountInfo { nonce, balance: U256::ZERO, ..Default::default() },
            );

            let evm_env = config
                .next_evm_env(
                    &alloy_consensus::Header::default(),
                    &TaikoNextBlockEnvAttributes {
                        timestamp: 1,
                        suggested_fee_recipient: Address::ZERO,
                        prev_randao: B256::ZERO,
                        gas_limit: 30_000_000,
                        extra_data: Bytes::new(),
                        base_fee_per_gas: 1,
                        parent_beacon_block_root: None,
                    },
                )
                .expect("next block env should build");
            let evm = config.evm_factory().create_evm(db, evm_env);

            TaikoBlockExecutor::new_with_zk_gas_mode(
                evm,
                TaikoBlockExecutionCtx {
                    parent_hash: B256::ZERO,
                    parent_beacon_block_root: None,
                    ommers: &[],
                    withdrawals: None,
                    basefee_per_gas: 1,
                    extra_data: Bytes::new(),
                    is_unzen_active: false,
                    expected_difficulty: None,
                    finalized_block_zk_gas: Default::default(),
                },
                chain_spec.clone(),
                RethReceiptBuilder::default(),
                ZkGasMode::Shadow,
            )
        };

        let tx = TxLegacy {
            chain_id: Some(ChainId::from(chain_spec.inner.chain().id())),
            nonce,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(treasury),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signature = Signature::new(U256::from(1), U256::from(2), false);
        let signed: TransactionSigned = Signed::new_unchecked(tx, signature, B256::ZERO).into();
        let expected_payload_bytes = signed.encode_2718_len() as u64;
        let anchor_tx = signed.with_signer(golden_touch);

        let mut executor_without_init = make_executor();
        assert!(
            executor_without_init.execute_transaction(anchor_tx.clone()).is_err(),
            "without pre-execution initialization, the zero-balance golden-touch tx must not be treated as anchor"
        );

        let mut executor_with_init = make_executor();
        executor_with_init
            .apply_pre_execution_changes()
            .expect("pre-execution changes should succeed");
        assert!(
            executor_with_init.execute_transaction(anchor_tx).is_ok(),
            "pre-execution initialization should seed anchor detection from the golden-touch account nonce"
        );
        let snapshot = executor_with_init.evm.zk_gas_pricing_snapshot().unwrap();
        assert_eq!(snapshot.started_non_anchor_transactions, 0);
        assert_eq!(
            snapshot.transaction_payload.canonical_transaction_bytes,
            expected_payload_bytes
        );
        assert_eq!(snapshot.transaction_payload.nonempty_payload_transactions, 0);
        let ZkGasShadowStatus::Incomplete { known, .. } = snapshot.shadow_report().status() else {
            panic!("anchor signed payload is known");
        };
        assert_eq!(
            known.transaction_payload_scaled(),
            256 * i128::from(expected_payload_bytes) *
                alethia_reth_evm::zk_gas::pricing::FIXED_POINT_SCALE
        );
    }
}
