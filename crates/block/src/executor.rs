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
#[cfg(all(feature = "execution-observer", feature = "prover"))]
use alloy_primitives::{B256, KECCAK256_EMPTY};
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
#[cfg(all(feature = "execution-observer", feature = "prover"))]
use alethia_reth_evm::zk_gas::observer::{BlockStopReason, TransactionDisposition};
#[cfg(feature = "execution-observer")]
use alethia_reth_evm::zk_gas::observer::{
    ChargeComponent, ChargeOutcome, ExecutionEvent, ExecutionPhase, RawGasSource,
    SharedExecutionObserver, TransactionExecutionClass,
};
use alethia_reth_evm::{
    alloy::{TAIKO_GOLDEN_TOUCH_ADDRESS, TaikoAnchorEvm, TaikoZkGasEvm},
    handler::get_treasury_address,
    zk_gas::{adapter::ZK_GAS_LIMIT_ERR, meter::ZkGasOutcome},
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
    recoverable_non_anchor_tx_error(error).is_some()
}

/// Recoverable filter reasons owned by the prover executor's existing transaction filter.
#[derive(Clone, Copy)]
enum RecoverableNonAnchorTxError {
    /// The active zk gas budget stopped further candidate execution.
    ZkGasLimit,
    /// Transaction validation rejected the candidate before EVM execution.
    Invalid,
    /// Transaction gas limit exceeds the remaining block gas.
    BlockGasLimit,
}

/// Maps the one authoritative recoverable-error set to its structured reason.
fn recoverable_non_anchor_tx_error(
    error: &BlockExecutionError,
) -> Option<RecoverableNonAnchorTxError> {
    if is_zk_gas_limit_exceeded(error) {
        return Some(RecoverableNonAnchorTxError::ZkGasLimit);
    }
    match error {
        BlockExecutionError::Validation(BlockValidationError::InvalidTx { .. }) => {
            Some(RecoverableNonAnchorTxError::Invalid)
        }
        BlockExecutionError::Validation(
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
        ) => Some(RecoverableNonAnchorTxError::BlockGasLimit),
        _ => None,
    }
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
    /// Feature-gated host observer that cannot influence execution decisions.
    #[cfg(feature = "execution-observer")]
    observer: Option<SharedExecutionObserver>,
    /// Current executor transaction index mirrored into intrinsic charge events.
    #[cfg(feature = "execution-observer")]
    observer_tx_index: Option<u64>,
    /// Whether the current successfully committed EVM transaction returned a revert result.
    #[cfg(feature = "execution-observer")]
    last_transaction_reverted: Option<bool>,
    /// In-flight meter total captured before the current transaction resets or commits.
    #[cfg(feature = "execution-observer")]
    observer_observed_current_zkgas: Option<u64>,
    /// Provisional start class, replaced after normal top-level frame initialization when present.
    #[cfg(feature = "execution-observer")]
    observer_execution_class: Option<TransactionExecutionClass>,
}

impl<'a, Evm, Spec, R> TaikoBlockExecutor<'a, Evm, Spec, R>
where
    Spec: Clone,
    R: ReceiptBuilder,
{
    /// Creates a new [`TaikoBlockExecutor`]
    pub fn new(
        mut evm: Evm,
        ctx: TaikoBlockExecutionCtx<'a>,
        spec: Spec,
        receipt_builder: R,
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
            #[cfg(feature = "execution-observer")]
            observer: None,
            #[cfg(feature = "execution-observer")]
            observer_tx_index: None,
            #[cfg(feature = "execution-observer")]
            last_transaction_reverted: None,
            #[cfg(feature = "execution-observer")]
            observer_observed_current_zkgas: None,
            #[cfg(feature = "execution-observer")]
            observer_execution_class: None,
        }
    }

    /// Creates an executor that additionally publishes feature-gated host observation events.
    #[cfg(feature = "execution-observer")]
    pub fn new_with_execution_observer(
        evm: Evm,
        ctx: TaikoBlockExecutionCtx<'a>,
        spec: Spec,
        receipt_builder: R,
        observer: SharedExecutionObserver,
    ) -> Self
    where
        Evm: TaikoAnchorEvm + TaikoZkGasEvm,
    {
        let mut executor = Self::new(evm, ctx, spec, receipt_builder);
        executor.observer = Some(observer);
        executor
    }

    /// Publishes an owned event only for the feature-gated observer constructor.
    #[cfg(feature = "execution-observer")]
    fn observe(&self, event: ExecutionEvent) {
        if let Some(observer) = &self.observer {
            observer.on_event(event);
        }
    }

    /// Aligns adapter-side operation events with the executor-owned transaction boundary.
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    fn set_observer_context(&mut self, phase: ExecutionPhase, tx_index: Option<u64>)
    where
        Evm: TaikoZkGasEvm,
    {
        self.observer_tx_index = tx_index;
        self.evm.set_execution_observer_context(phase, tx_index);
    }

    /// Captures attempted current zk gas before the executor crosses a reset or commit boundary.
    #[cfg(feature = "execution-observer")]
    fn capture_observed_current_zkgas(&mut self)
    where
        Evm: TaikoZkGasEvm,
    {
        self.observer_observed_current_zkgas = self.evm.transaction_zk_gas_used();
    }

    /// Repairs the provisional start class from the code hash captured at the normal top-level
    /// execution frame. No database access is performed here.
    #[cfg(feature = "execution-observer")]
    fn update_observer_execution_class_from_execution(&mut self)
    where
        Evm: TaikoZkGasEvm,
    {
        // Active precompiles and already-loaded contracts are authoritatively known at the
        // executor boundary. In particular, a precompile's normal frame has no account bytecode,
        // so the adapter's code-hash observation must not downgrade it to a no-code call.
        if self.observer_execution_class == Some(TransactionExecutionClass::ContractCall) {
            return;
        }
        let Some(execution_class) = self.evm.observed_transaction_execution_class() else {
            return;
        };
        self.observer_execution_class = Some(execution_class);
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
            Err(ZkGasOutcome::LimitExceeded) => {
                return Err(Self::zk_gas_limit_error());
            }
        }
        Ok(())
    }

    /// Discards any in-flight zk gas for the current transaction while preserving the committed
    /// block total.
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
    /// is infallible in alloy-evm 0.37, so `execute_transaction_without_commit` pre-checks the
    /// budget while the failure can still be reported; reaching the exceeded branch here means
    /// the executor was driven with a result that skipped that check.
    fn commit_current_transaction_zk_gas(&mut self)
    where
        Evm: TaikoZkGasEvm,
    {
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
        #[cfg(feature = "execution-observer")]
        {
            self.set_observer_context(ExecutionPhase::PreExecutionSystem, None);
            self.observe(ExecutionEvent::PhaseStart { phase: ExecutionPhase::PreExecutionSystem });
        }
        if let Err(error) = self.apply_pre_execution_changes() {
            #[cfg(feature = "execution-observer")]
            self.observe(ExecutionEvent::BlockStop {
                reason: BlockStopReason::Fatal,
                first_unattempted_tx_index: None,
            });
            return Err(error);
        }
        #[cfg(feature = "execution-observer")]
        {
            self.observe(ExecutionEvent::PhaseEnd { phase: ExecutionPhase::PreExecutionSystem });
            self.set_observer_context(ExecutionPhase::Transactions, None);
            self.observe(ExecutionEvent::PhaseStart { phase: ExecutionPhase::Transactions });
        }

        let recovered_transactions: Vec<_> = transactions.into_iter().collect();
        #[cfg(all(feature = "execution-observer", feature = "prover"))]
        let recovered_tx_count = recovered_transactions.len();
        #[cfg(all(feature = "execution-observer", feature = "prover"))]
        let mut truncation_first_unattempted = None;
        let mut committed_transactions = Vec::new();
        for (idx, tx) in recovered_transactions.into_iter().enumerate() {
            let is_anchor_transaction = idx == 0;
            #[cfg(feature = "execution-observer")]
            {
                let tx_index = u64::try_from(idx).expect("transaction index must fit u64");
                self.set_observer_context(ExecutionPhase::Transactions, Some(tx_index));
                // Every attempted transaction starts with an explicit zero snapshot. This keeps
                // pre-execution filters from inheriting the preceding transaction's committed
                // total when they never cross the reset/commit bracket.
                self.observer_observed_current_zkgas = Some(0);
                let execution_class = self.classify_transaction(tx.inner());
                self.observer_execution_class = Some(execution_class);
                self.observe(ExecutionEvent::TransactionStart {
                    tx_index,
                    tx_hash: *tx.inner().trie_hash().as_ref(),
                    is_anchor: is_anchor_transaction,
                    execution_class,
                });
            }
            if !is_anchor_transaction && tx.signer() == Address::ZERO {
                #[cfg(feature = "execution-observer")]
                self.observe_transaction_end(idx, TransactionDisposition::FilteredZeroSigner);
                continue;
            }

            let committed_tx = Recovered::new_unchecked((*tx.inner()).clone(), tx.signer());
            match self.try_execute_filtered(tx, is_anchor_transaction) {
                Ok(None) => {
                    committed_transactions.push(committed_tx);
                    #[cfg(feature = "execution-observer")]
                    self.observe_transaction_end(
                        idx,
                        if self.last_transaction_reverted == Some(true) {
                            TransactionDisposition::CommittedRevert
                        } else {
                            TransactionDisposition::CommittedSuccess
                        },
                    );
                }
                Ok(Some(reason)) => {
                    #[cfg(feature = "execution-observer")]
                    self.observe_transaction_end(
                        idx,
                        match reason {
                            RecoverableNonAnchorTxError::ZkGasLimit => {
                                TransactionDisposition::FilteredZkGasLimit
                            }
                            RecoverableNonAnchorTxError::Invalid => {
                                TransactionDisposition::FilteredInvalid
                            }
                            RecoverableNonAnchorTxError::BlockGasLimit => {
                                TransactionDisposition::FilteredBlockGasLimit
                            }
                        },
                    );
                    #[cfg(not(feature = "execution-observer"))]
                    let _ = reason;
                }
                Err(error) => {
                    #[cfg(feature = "execution-observer")]
                    {
                        self.observe_transaction_end(idx, TransactionDisposition::Fatal);
                        self.observe(ExecutionEvent::BlockStop {
                            reason: BlockStopReason::Fatal,
                            first_unattempted_tx_index: None,
                        });
                    }
                    return Err(error);
                }
            }
            if self.zk_gas_exhausted {
                #[cfg(all(feature = "execution-observer", feature = "prover"))]
                {
                    truncation_first_unattempted = (idx + 1 < recovered_tx_count)
                        .then(|| u64::try_from(idx + 1).expect("transaction index must fit u64"));
                }
                break;
            }
        }

        #[cfg(feature = "execution-observer")]
        self.observe(ExecutionEvent::PhaseEnd { phase: ExecutionPhase::Transactions });
        #[cfg(feature = "execution-observer")]
        let terminal_observer = self.observer.clone();
        #[cfg(feature = "execution-observer")]
        let terminal_stop = if self.zk_gas_exhausted {
            BlockStopReason::ZkGasTruncated
        } else {
            BlockStopReason::Complete
        };
        let execution_result = match self.apply_post_execution_changes() {
            Ok(execution_result) => execution_result,
            Err(error) => {
                #[cfg(feature = "execution-observer")]
                if let Some(observer) = terminal_observer {
                    observer.on_event(ExecutionEvent::BlockStop {
                        reason: BlockStopReason::Fatal,
                        first_unattempted_tx_index: None,
                    });
                }
                return Err(error);
            }
        };
        #[cfg(feature = "execution-observer")]
        if let Some(observer) = terminal_observer {
            observer.on_event(ExecutionEvent::BlockStop {
                reason: terminal_stop,
                first_unattempted_tx_index: match terminal_stop {
                    BlockStopReason::ZkGasTruncated => truncation_first_unattempted,
                    BlockStopReason::Complete | BlockStopReason::Fatal => None,
                },
            });
        }
        Ok(CommittedBlockExecutionOutcome { execution_result, committed_transactions })
    }

    /// Emits the executor-owned conclusion for an attempted transaction.
    #[cfg(feature = "execution-observer")]
    fn observe_transaction_end(&self, index: usize, disposition: TransactionDisposition) {
        let tx_index = u64::try_from(index).expect("transaction index must fit u64");
        let committed = self.ctx.finalized_block_zk_gas();
        let observed = self.observer_observed_current_zkgas.unwrap_or(committed);
        self.observe(ExecutionEvent::TransactionEnd {
            tx_index,
            disposition,
            execution_class: self
                .observer_execution_class
                .unwrap_or(TransactionExecutionClass::Other),
            observed_current_zkgas: observed,
            committed_current_zkgas: committed,
        });
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
    /// Executes `tx` and returns `Ok(None)` if it committed, the exact recoverable non-anchor
    /// filter reason when it was skipped, or `Err` for fatal / anchor errors.
    fn try_execute_filtered(
        &mut self,
        tx: impl ExecutableTx<Self>,
        is_anchor_transaction: bool,
    ) -> Result<Option<RecoverableNonAnchorTxError>, BlockExecutionError> {
        match self.execute_transaction(tx) {
            Ok(_) => Ok(None),
            // We don't allow the anchor transaction to be discarded even if it would otherwise be a
            // recoverable failure; this should never happen in practice.
            Err(err) if !is_anchor_transaction => {
                if let Some(reason) = recoverable_non_anchor_tx_error(&err) {
                    Ok(Some(reason))
                } else {
                    Err(err)
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Classifies a transaction from structured envelope data and already-loaded execution facts.
    #[cfg(feature = "execution-observer")]
    fn classify_transaction(&mut self, tx: &R::Transaction) -> TransactionExecutionClass {
        if tx.is_create() {
            return TransactionExecutionClass::ContractCreate;
        }
        let Some(recipient) = tx.to() else {
            return TransactionExecutionClass::Other;
        };
        // Active precompiles execute at this call boundary even though no account code needs to be
        // present in the database. Ordinary account facts come only from the existing journal:
        // querying `basic` here would insert an observer-only, fallible database access before
        // zero-signer and block-gas filtering.
        let has_executable_code = self.evm.is_active_precompile(&recipient) ||
            self.evm.loaded_account_code_hash(&recipient).is_some_and(|code_hash| {
                code_hash != B256::ZERO && code_hash != KECCAK256_EMPTY
            });
        if has_executable_code {
            TransactionExecutionClass::ContractCall
        } else if tx.value().is_zero() {
            TransactionExecutionClass::NoCodeNoValue
        } else {
            TransactionExecutionClass::NativeValueTransfer
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
            #[cfg(feature = "execution-observer")]
            self.capture_observed_current_zkgas();
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
        #[cfg(feature = "execution-observer")]
        {
            self.last_transaction_reverted = None;
        }
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
        // in-flight tx total so it is committed alongside opcode/precompile usage on success
        // and discarded on revert/failure. A schedule with a zero intrinsic makes this a no-op.
        // If the intrinsic alone would exceed the remaining block budget, mirror the
        // mid-tx exhaustion path used by the inspector.
        let intrinsic_charge = self.evm.charge_tx_intrinsic_zk_gas();
        #[cfg(feature = "execution-observer")]
        if let Some(amount) = self.evm.tx_intrinsic_zk_gas() {
            let outcome = match intrinsic_charge {
                Ok(()) => ChargeOutcome::Applied,
                Err(ZkGasOutcome::LimitExceeded) => ChargeOutcome::LimitExceeded,
            };
            self.observe(ExecutionEvent::ChargeAttempt {
                operation_id: None,
                phase: ExecutionPhase::Transactions,
                tx_index: self.observer_tx_index,
                component: ChargeComponent::TxIntrinsic { amount },
                charge_raw_gas: None,
                raw_gas_source: RawGasSource::IntrinsicFixed,
                multiplier: None,
                requested_current_zkgas: Some(amount),
                outcome,
            });
        }
        if intrinsic_charge.is_err() {
            self.zk_gas_exhausted = true;
            #[cfg(feature = "execution-observer")]
            self.capture_observed_current_zkgas();
            self.reset_current_transaction_zk_gas();
            return Err(Self::zk_gas_limit_error());
        }

        let result = match self.evm.transact(tx_env) {
            Ok(result) => result,
            Err(err) if err.to_string() == ZK_GAS_LIMIT_ERR => {
                #[cfg(feature = "execution-observer")]
                self.update_observer_execution_class_from_execution();
                self.zk_gas_exhausted = true;
                #[cfg(feature = "execution-observer")]
                self.capture_observed_current_zkgas();
                self.reset_current_transaction_zk_gas();
                return Err(Self::zk_gas_limit_error());
            }
            Err(err) => {
                #[cfg(feature = "execution-observer")]
                self.update_observer_execution_class_from_execution();
                #[cfg(feature = "execution-observer")]
                self.capture_observed_current_zkgas();
                self.reset_current_transaction_zk_gas();
                return Err(BlockExecutionError::evm(err, tx.tx().trie_hash()));
            }
        };
        #[cfg(feature = "execution-observer")]
        {
            self.update_observer_execution_class_from_execution();
            self.last_transaction_reverted = Some(!result.result.is_success());
        }

        // The trait's `commit_transaction` is infallible in alloy-evm 0.37, so the block zk gas
        // budget must be checked here, where truncation can still be reported. The in-flight
        // total is final at this point — nothing meters between execution and commit.
        if self.evm.transaction_zk_gas_commit_would_exceed() {
            self.zk_gas_exhausted = true;
            #[cfg(feature = "execution-observer")]
            self.capture_observed_current_zkgas();
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
        #[cfg(feature = "execution-observer")]
        self.capture_observed_current_zkgas();
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
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    use std::sync::Mutex;

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    use alloy_consensus::SignableTransaction;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_evm::EvmFactory;
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U64, U256};
    use reth_ethereum_primitives::TransactionSigned;
    use reth_evm::{ConfigureEvm, block::BlockExecutor};
    use reth_evm_ethereum::RethReceiptBuilder;
    use reth_primitives_traits::SignedTransaction;
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    use reth_revm::context::{ContextTr, JournalTr};
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    use reth_revm::state::{Bytecode, bytecode::opcode};
    use reth_revm::{
        State,
        db::{CacheDB, EmptyDB},
        state::AccountInfo,
    };

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    use alethia_reth_evm::zk_gas::observer::{ExecutionEvent, ExecutionObserver};
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    use alethia_reth_evm::zk_gas::unzen::UNZEN_ZK_GAS_SCHEDULE;
    use alethia_reth_evm::{
        alloy::decode_anchor_system_call_data, factory::TaikoEvmFactory, spec::TaikoSpecId,
        zk_gas::unzen::TX_INTRINSIC_ZK_GAS,
    };

    use super::*;
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    use crate::testutil::insert_contract;
    use crate::{
        config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
        testutil::{
            BENCH_LIMIT_TARGET, BENCH_SUCCESS_TARGET, db_with_contracts, recovered_tx,
            unzen_chain_spec, unzen_evm_env, unzen_execution_ctx,
        },
    };
    use alethia_reth_chainspec::spec::TaikoChainSpec;
    const BENCH_CALLER: Address = Address::with_last_byte(0x30);

    /// No-op sink used to construct the observer EVM path for classification tests.
    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    struct NoopExecutionObserver;

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    impl ExecutionObserver for NoopExecutionObserver {
        fn on_event(&self, _event: ExecutionEvent) {}
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[derive(Default)]
    struct RecordingExecutionObserver(Mutex<Vec<ExecutionEvent>>);

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    impl ExecutionObserver for RecordingExecutionObserver {
        fn on_event(&self, event: ExecutionEvent) {
            self.0.lock().expect("observer lock should not be poisoned").push(event);
        }
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_classifies_structured_transaction_shapes() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let observer = Arc::new(NoopExecutionObserver);
        let evm = TaikoEvmFactory.create_evm_with_execution_observer(
            &mut state,
            unzen_evm_env(),
            observer.clone(),
        );
        let mut executor = TaikoBlockExecutor::new_with_execution_observer(
            evm,
            unzen_execution_ctx(),
            chain_spec,
            RethReceiptBuilder::default(),
            observer,
        );
        let tx = |to, value| -> TransactionSigned {
            TxLegacy {
                chain_id: Some(ChainId::from(167_u64)),
                nonce: 0,
                gas_price: 1,
                gas_limit: 100_000,
                to,
                value,
                input: Bytes::new(),
            }
            .into_signed(Signature::new(U256::from(1), U256::from(2), false))
            .into()
        };

        assert_eq!(
            executor.classify_transaction(&tx(
                TxKind::Call(Address::with_last_byte(0x41)),
                U256::from(1)
            )),
            TransactionExecutionClass::NativeValueTransfer
        );
        assert_eq!(
            executor.classify_transaction(&tx(
                TxKind::Call(Address::with_last_byte(0x42),),
                U256::ZERO
            )),
            TransactionExecutionClass::NoCodeNoValue
        );
        executor
            .evm_mut()
            .ctx_mut()
            .journal_mut()
            .load_account(BENCH_SUCCESS_TARGET)
            .expect("normal execution lookup should populate the journal");
        assert_eq!(
            executor.classify_transaction(&tx(TxKind::Call(BENCH_SUCCESS_TARGET), U256::ZERO)),
            TransactionExecutionClass::ContractCall
        );
        assert_eq!(
            executor.classify_transaction(&tx(TxKind::Create, U256::ZERO)),
            TransactionExecutionClass::ContractCreate
        );

        let persisted_code_target = Address::with_last_byte(0x43);
        executor.evm_mut().db_mut().database.insert_account_info(
            persisted_code_target,
            AccountInfo { code_hash: B256::with_last_byte(0x01), code: None, ..Default::default() },
        );
        executor
            .evm_mut()
            .ctx_mut()
            .journal_mut()
            .load_account(persisted_code_target)
            .expect("normal execution lookup should populate the journal");
        assert_eq!(
            executor.classify_transaction(&tx(TxKind::Call(persisted_code_target), U256::ZERO)),
            TransactionExecutionClass::ContractCall,
            "a non-empty persisted code hash is executable even when bytecode is not cached"
        );
        assert_eq!(
            executor
                .classify_transaction(&tx(TxKind::Call(Address::with_last_byte(0x04)), U256::ZERO)),
            TransactionExecutionClass::ContractCall,
            "active precompile destinations execute code without a database account"
        );

        for (address, code_hash, value) in [
            (Address::with_last_byte(0x44), B256::ZERO, U256::ZERO),
            (Address::with_last_byte(0x45), KECCAK256_EMPTY, U256::from(1)),
        ] {
            executor.evm_mut().db_mut().database.insert_account_info(
                address,
                AccountInfo { code_hash, code: None, ..Default::default() },
            );
            executor
                .evm_mut()
                .ctx_mut()
                .journal_mut()
                .load_account(address)
                .expect("normal execution lookup should populate the journal");
            assert_eq!(
                executor.classify_transaction(&tx(TxKind::Call(address), value)),
                if value.is_zero() {
                    TransactionExecutionClass::NoCodeNoValue
                } else {
                    TransactionExecutionClass::NativeValueTransfer
                },
                "both empty-code hash encodings must remain non-contract calls"
            );
        }
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
        let mut executor = TaikoBlockExecutor::new(
            evm,
            ctx.clone(),
            chain_spec.clone(),
            RethReceiptBuilder::default(),
        );

        let gas_used = executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1))
            .expect("first transaction should commit");
        let finalized_after_first = ctx.finalized_block_zk_gas();
        assert!(finalized_after_first > 0);

        let err = executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_LIMIT_TARGET, 1, 1))
            .expect_err("second transaction should be discarded");
        assert!(is_zk_gas_limit_exceeded(&err));
        assert_eq!(ctx.finalized_block_zk_gas(), finalized_after_first);

        let repeated_err = executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 1, 1))
            .expect_err("later transactions should not execute after zk gas exhaustion");
        assert!(is_zk_gas_limit_exceeded(&repeated_err));

        let (_, result) = executor.finish().expect("executor should finish after truncation");
        assert_eq!(result.receipts.len(), 1);
        assert_eq!(result.gas_used, gas_used.tx_gas_used());
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

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_emits_one_fatal_terminal_after_difficulty_mismatch() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let observer = Arc::new(RecordingExecutionObserver::default());
        let evm = TaikoEvmFactory.create_evm_with_execution_observer(
            &mut state,
            unzen_evm_env(),
            observer.clone(),
        );
        let mut ctx = unzen_execution_ctx();
        ctx.expected_difficulty = Some(U256::ZERO);
        let executor = TaikoBlockExecutor::new_with_execution_observer(
            evm,
            ctx,
            chain_spec,
            RethReceiptBuilder::default(),
            observer.clone(),
        );
        let transactions = [recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1)];
        assert!(
            executor
                .execute_block_with_committed_transactions(
                    transactions.iter().map(|tx| Recovered::new_unchecked(tx.inner(), tx.signer()))
                )
                .is_err()
        );
        let events = observer.0.lock().expect("observer lock should not be poisoned");
        let phase_end = events
            .iter()
            .position(|event| {
                matches!(event, ExecutionEvent::PhaseEnd { phase: ExecutionPhase::Transactions })
            })
            .expect("transactions phase must end before terminal validation");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ExecutionEvent::BlockStop { reason: BlockStopReason::Fatal, .. }
                ))
                .count(),
            1
        );
        assert!(matches!(
            events.last(),
            Some(ExecutionEvent::BlockStop {
                reason: BlockStopReason::Fatal,
                first_unattempted_tx_index: None,
            })
        ));
        assert!(phase_end < events.len() - 1);
        assert!(!events.iter().any(|event| matches!(
            event,
            ExecutionEvent::BlockStop {
                reason: BlockStopReason::Complete | BlockStopReason::ZkGasTruncated,
                ..
            }
        )));
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_emits_fatal_transaction_end_for_anchor_validation_error() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let observer = Arc::new(RecordingExecutionObserver::default());
        let evm = TaikoEvmFactory.create_evm_with_execution_observer(
            &mut state,
            unzen_evm_env(),
            observer.clone(),
        );
        let ctx = unzen_execution_ctx();
        let executor = TaikoBlockExecutor::new_with_execution_observer(
            evm,
            ctx,
            chain_spec,
            RethReceiptBuilder::default(),
            observer.clone(),
        );
        let transactions = [recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 99, 1)];
        assert!(
            executor
                .execute_block_with_committed_transactions(
                    transactions.iter().map(|tx| Recovered::new_unchecked(tx.inner(), tx.signer()))
                )
                .is_err(),
            "an invalid anchor transaction must remain fatal"
        );

        let events = observer.0.lock().expect("observer lock should not be poisoned");
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::TransactionEnd {
                tx_index: 0,
                disposition: TransactionDisposition::Fatal,
                observed_current_zkgas,
                committed_current_zkgas: 0,
                ..
            } if *observed_current_zkgas == TX_INTRINSIC_ZK_GAS
        )));
        assert!(matches!(
            events.last(),
            Some(ExecutionEvent::BlockStop {
                reason: BlockStopReason::Fatal,
                first_unattempted_tx_index: None,
            })
        ));
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_rejects_call_wrapper_before_dispatching_precompile() {
        let caller = BENCH_CALLER;
        let target = Address::with_last_byte(0x81);
        let bytecode = Bytecode::new_raw(Bytes::from(vec![
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x04,
            opcode::PUSH2,
            0xff,
            0xff,
            opcode::CALL,
            opcode::STOP,
        ]));
        let make_db = || {
            let mut db = db_with_contracts(&[(caller, 0)]);
            insert_contract(&mut db, target, bytecode.clone());
            db
        };
        let call_charge = UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.call *
            u64::from(UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[usize::from(opcode::CALL)]);
        let remaining = TX_INTRINSIC_ZK_GAS + call_charge - 1;
        let reserved = UNZEN_ZK_GAS_SCHEDULE.block_limit - remaining;

        let normal_chain_spec = Arc::new(unzen_chain_spec());
        let mut normal_state =
            State::builder().with_database(make_db()).with_bundle_update().build();
        let normal_evm = TaikoEvmFactory.create_evm(&mut normal_state, unzen_evm_env());
        let normal_ctx = unzen_execution_ctx();
        let mut normal = TaikoBlockExecutor::new(
            normal_evm,
            normal_ctx.clone(),
            normal_chain_spec,
            RethReceiptBuilder::default(),
        );
        normal.reserve_block_zk_gas(reserved).expect("normal reserve must fit");
        let normal_error = normal
            .execute_transaction(recovered_tx(caller, target, 0, 1))
            .expect_err("the production wrapper charge must exhaust the remaining budget");
        assert!(is_zk_gas_limit_exceeded(&normal_error));

        let observed_chain_spec = Arc::new(unzen_chain_spec());
        let mut observed_state =
            State::builder().with_database(make_db()).with_bundle_update().build();
        let observer = Arc::new(RecordingExecutionObserver::default());
        let observed_evm = TaikoEvmFactory.create_evm_with_execution_observer(
            &mut observed_state,
            unzen_evm_env(),
            observer.clone(),
        );
        let observed_ctx = unzen_execution_ctx();
        let mut observed = TaikoBlockExecutor::new_with_execution_observer(
            observed_evm,
            observed_ctx.clone(),
            observed_chain_spec,
            RethReceiptBuilder::default(),
            observer.clone(),
        );
        observed.set_observer_context(ExecutionPhase::Transactions, Some(0));
        observed.reserve_block_zk_gas(reserved).expect("observer reserve must fit");
        let observed_error = observed
            .execute_transaction(recovered_tx(caller, target, 0, 1))
            .expect_err("the observer wrapper charge must exhaust the remaining budget");
        assert!(is_zk_gas_limit_exceeded(&observed_error));
        assert_eq!(observed_ctx.finalized_block_zk_gas(), normal_ctx.finalized_block_zk_gas());

        let events = observer.0.lock().expect("observer lock should not be poisoned");
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::ChargeAttempt {
                component: alethia_reth_evm::zk_gas::observer::ChargeComponent::Opcode {
                    opcode: opcode::CALL,
                    spawned: true,
                },
                raw_gas_source: alethia_reth_evm::zk_gas::observer::RawGasSource::SpawnEstimate,
                outcome: alethia_reth_evm::zk_gas::observer::ChargeOutcome::LimitExceeded,
                ..
            }
        )));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                ExecutionEvent::OperationExecuted {
                    component:
                        alethia_reth_evm::zk_gas::observer::OperationComponent::Precompile {
                            address,
                            ..
                        },
                    ..
                } if *address == Address::with_last_byte(0x04).into_array()
            )),
            "a rejected wrapper must stop before the precompile body executes"
        );
    }

    #[cfg(all(feature = "execution-observer", feature = "prover"))]
    #[test]
    fn observer_records_precompile_work_before_native_charge_limit() {
        let caller = BENCH_CALLER;
        let wrapper = Address::with_last_byte(0x82);
        let wrapper_code = Bytecode::new_raw(Bytes::from(vec![
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x00,
            opcode::PUSH1,
            0x04,
            opcode::PUSH2,
            0xff,
            0xff,
            opcode::CALL,
            opcode::STOP,
        ]));
        let make_db = || {
            let mut db = db_with_contracts(&[(caller, 0)]);
            insert_contract(&mut db, wrapper, wrapper_code.clone());
            db
        };
        let multiplier =
            |opcode: u8| u64::from(UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers[usize::from(opcode)]);
        let arithmetic_tx_charge =
            TX_INTRINSIC_ZK_GAS + 2 * 3 * multiplier(opcode::PUSH1) + 3 * multiplier(opcode::ADD);
        let wrapper_before_precompile = TX_INTRINSIC_ZK_GAS +
            6 * 3 * multiplier(opcode::PUSH1) +
            3 * multiplier(opcode::PUSH2) +
            UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.call * multiplier(opcode::CALL);
        let precompile_charge = 15 *
            u64::from(
                UNZEN_ZK_GAS_SCHEDULE.precompile_multiplier(&Address::with_last_byte(0x04)),
            );
        let remaining = arithmetic_tx_charge + wrapper_before_precompile + precompile_charge - 1;
        let reserved = UNZEN_ZK_GAS_SCHEDULE.block_limit - remaining;
        let transactions =
            [recovered_tx(caller, BENCH_SUCCESS_TARGET, 0, 1), recovered_tx(caller, wrapper, 1, 1)];

        let normal_chain_spec = Arc::new(unzen_chain_spec());
        let mut normal_state =
            State::builder().with_database(make_db()).with_bundle_update().build();
        let normal_evm = TaikoEvmFactory.create_evm(&mut normal_state, unzen_evm_env());
        let normal_ctx = unzen_execution_ctx();
        let mut normal = TaikoBlockExecutor::new(
            normal_evm,
            normal_ctx.clone(),
            normal_chain_spec,
            RethReceiptBuilder::default(),
        );
        normal.reserve_block_zk_gas(reserved).expect("normal reserve must fit");
        let normal_outcome = normal
            .execute_block_with_committed_transactions(
                transactions.iter().map(|tx| Recovered::new_unchecked(tx.inner(), tx.signer())),
            )
            .expect("normal prover execution should truncate at the precompile charge");

        let observed_chain_spec = Arc::new(unzen_chain_spec());
        let mut observed_state =
            State::builder().with_database(make_db()).with_bundle_update().build();
        let observer = Arc::new(RecordingExecutionObserver::default());
        let observed_evm = TaikoEvmFactory.create_evm_with_execution_observer(
            &mut observed_state,
            unzen_evm_env(),
            observer.clone(),
        );
        let observed_ctx = unzen_execution_ctx();
        let mut observed = TaikoBlockExecutor::new_with_execution_observer(
            observed_evm,
            observed_ctx.clone(),
            observed_chain_spec,
            RethReceiptBuilder::default(),
            observer.clone(),
        );
        observed.reserve_block_zk_gas(reserved).expect("observer reserve must fit");
        let observed_outcome = observed
            .execute_block_with_committed_transactions(
                transactions.iter().map(|tx| Recovered::new_unchecked(tx.inner(), tx.signer())),
            )
            .expect("observer prover execution should truncate at the precompile charge");

        assert_eq!(observed_outcome.committed_transactions, normal_outcome.committed_transactions);
        assert_eq!(observed_outcome.execution_result, normal_outcome.execution_result);
        assert_eq!(observed_ctx.finalized_block_zk_gas(), normal_ctx.finalized_block_zk_gas());
        assert_eq!(observed_outcome.committed_transactions.len(), 1);

        let events = observer.0.lock().expect("observer lock should not be poisoned");
        let (operation_position, operation_id) = events
            .iter()
            .enumerate()
            .find_map(|(position, event)| match event {
                ExecutionEvent::OperationExecuted {
                    operation_id,
                    tx_index: Some(1),
                    component:
                        alethia_reth_evm::zk_gas::observer::OperationComponent::Precompile {
                            address,
                            ..
                        },
                    ..
                } if *address == Address::with_last_byte(0x04).into_array() => {
                    Some((position, *operation_id))
                }
                _ => None,
            })
            .expect("the identity precompile must execute before its native charge fails");
        let charge_position = events
            .iter()
            .enumerate()
            .find_map(|(position, event)| match event {
                ExecutionEvent::ChargeAttempt {
                    operation_id: Some(charge_id),
                    tx_index: Some(1),
                    component:
                        alethia_reth_evm::zk_gas::observer::ChargeComponent::Precompile { address },
                    outcome: alethia_reth_evm::zk_gas::observer::ChargeOutcome::LimitExceeded,
                    ..
                } if *charge_id == operation_id &&
                    *address == Address::with_last_byte(0x04).into_array() =>
                {
                    Some(position)
                }
                _ => None,
            })
            .expect("the completed precompile must link to its native limit charge");
        assert!(operation_position < charge_position);
        assert!(events.iter().any(|event| matches!(
            event,
            ExecutionEvent::TransactionEnd {
                tx_index: 1,
                disposition: TransactionDisposition::FilteredZkGasLimit,
                ..
            }
        )));
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

            TaikoBlockExecutor::new(
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
    }
}
