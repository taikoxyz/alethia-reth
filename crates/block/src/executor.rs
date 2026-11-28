//! Taiko block executor integrating fork-aware fee initialization and transaction filtering.
use alloy_consensus::{Transaction, TransactionEnvelope, TxReceipt};
#[cfg(feature = "prover")]
use alloy_consensus::{Typed2718, transaction::Recovered};
use alloy_eips::{Encodable2718, eip7685::Requests};
use alloy_evm::{
    FromRecoveredTx, FromTxWithEncoded, RecoveredTx,
    block::GasOutput,
    eth::{EthTxResult, receipt_builder::ReceiptBuilder},
};
use alloy_primitives::{Address, Bytes, Log, U256, Uint};
use reth_chainspec::EthChainSpec;
use reth_evm::{
    Evm,
    block::{
        BlockExecutionError, BlockExecutor, BlockValidationError, CommitChanges, ExecutableTx,
        InternalBlockExecutionError, StateDB, SystemCaller,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
};
use reth_execution_types::BlockExecutionResult;
use reth_revm::context::{
    Block as _,
    result::{ExecutionResult, ResultAndState},
};
use revm_database_interface::{Database, DatabaseCommit};

use crate::factory::TaikoBlockExecutionCtx;
use alethia_reth_chainspec::spec::{MissingEtnaStakingAddress, TaikoExecutorSpec};
use alethia_reth_evm::{
    alloy::{TAIKO_GOLDEN_TOUCH_ADDRESS, TaikoAnchorEvm, TaikoSystemCallEvm, TaikoZkGasEvm},
    handler::get_treasury_address,
    zk_gas::{adapter::ZK_GAS_LIMIT_ERR, meter::ZkGasOutcome},
};
use alethia_reth_primitives::{
    EtnaExtraData, decode_shasta_basefee_sharing_pctg,
    etna::{
        ETNA_BLOCK_START_CALL_GAS, ETNA_SYSTEM_CALLER, validate_etna_extra_data, validate_etna_root,
    },
};

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
        BlockExecutionError::Validation(BlockValidationError::Other(err)) => {
            err.is::<ZkGasLimitExceeded>()
        }
        _ => false,
    }
}

/// Returns `true` when `error` represents a zk gas difficulty commitment mismatch.
pub fn is_zk_gas_difficulty_mismatch(error: &BlockExecutionError) -> bool {
    match error {
        BlockExecutionError::Internal(err) => err.is_other::<ZkGasDifficultyMismatch>(),
        BlockExecutionError::Validation(BlockValidationError::Other(err)) => {
            err.is::<ZkGasDifficultyMismatch>()
        }
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
    /// Whether pre-execution already applied the once-per-block start processing: the Etna
    /// block-start staking call, the authoritative fee sharing and, before Etna, the legacy
    /// anchor context. A repeated `apply_pre_execution_changes` skips all three.
    block_start_applied: bool,
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
        // Pre-execution installs authoritative fee context directly after Etna, or through the
        // legacy anchor marker before it; replay-only derivation must stay off.
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
            block_start_applied: false,
        }
    }

    /// Wraps a zk gas failure as a block validation error from Etna on, so the Engine tree returns
    /// INVALID, while preserving the historical internal-error mapping before Etna.
    fn zk_gas_error<E>(&self, error: E) -> BlockExecutionError
    where
        E: std::error::Error + Send + Sync + 'static,
        Spec: TaikoExecutorSpec,
        Evm: reth_evm::Evm,
    {
        if self.spec.is_etna_active(self.evm.block().timestamp().to()) {
            BlockValidationError::other(error).into()
        } else {
            BlockExecutionError::other(error)
        }
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
                return Err(BlockExecutionError::other(ZkGasLimitExceeded));
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
    /// is infallible in alloy-evm 0.39, so `execute_transaction_without_commit` pre-checks the
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
    fn validate_expected_zk_gas_difficulty(&self) -> Result<(), BlockExecutionError>
    where
        Spec: TaikoExecutorSpec,
        Evm: reth_evm::Evm,
    {
        let Some(expected) = self.ctx.expected_difficulty() else { return Ok(()) };
        let got = U256::from(self.ctx.finalized_block_zk_gas());
        if got == expected {
            return Ok(());
        }

        Err(self.zk_gas_error(ZkGasDifficultyMismatch { expected, got }))
    }
}

/// Executor hooks that let a build keep filling a block after one candidate overflows the zk gas
/// budget.
///
/// Pool selection, Etna given-list builds and Etna prover candidate lists use them. Block import,
/// sealing, committed-block execution and pre-Etna candidate lists never do, so they keep stopping
/// at the first zk gas overflow.
pub trait TxSelectionExecutor {
    /// Clears the zk gas exhaustion an overflowed transaction left, so the next candidate can run.
    /// Only a build that leaves the overflowing transaction out may call this; committed-block
    /// execution must keep stopping at the first overflow. Call it only between transactions,
    /// after an overflow error: it discards the in-flight zk gas.
    fn clear_zk_gas_exhaustion(&mut self);

    /// Returns whether the block zk gas budget can still pay another transaction's intrinsic
    /// charge. Always true without a zk gas meter.
    fn can_fit_another_transaction_zk_gas(&self) -> bool;
}

impl<Evm: TaikoZkGasEvm, Spec: Clone, R: ReceiptBuilder> TxSelectionExecutor
    for TaikoBlockExecutor<'_, Evm, Spec, R>
{
    /// Drops the sticky exhaustion flag and discards the overflowed transaction's in-flight zk
    /// gas, keeping the committed block total.
    fn clear_zk_gas_exhaustion(&mut self) {
        self.zk_gas_exhausted = false;
        self.reset_current_transaction_zk_gas();
    }

    /// Asks the EVM meter whether the finalized block budget covers one more intrinsic charge.
    fn can_fit_another_transaction_zk_gas(&self) -> bool {
        self.evm.can_fit_tx_intrinsic_zk_gas()
    }
}

#[cfg(feature = "prover")]
impl<E, Spec, R> TaikoBlockExecutor<'_, E, Spec, R>
where
    E: Evm<
            DB: StateDB + DatabaseCommit,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
        > + TaikoZkGasEvm
        + TaikoAnchorEvm
        + TaikoSystemCallEvm,
    Spec: TaikoExecutorSpec + EthChainSpec + Clone,
    R: ReceiptBuilder<
            Transaction: Transaction + Encodable2718 + Clone,
            Receipt: TxReceipt<Log = Log>,
        >,
    <R::Transaction as TransactionEnvelope>::TxType: Send + 'static,
{
    /// Executes a prover candidate block and returns the transactions that were actually committed.
    ///
    /// Invalid non-anchor transactions are skipped, as in a given-list build. Before Etna, zk gas
    /// exhaustion stops the list, as Unzen derivation requires. At Etna a transaction that
    /// overflows the zk gas budget is left out and the rest of the list still runs, matching the
    /// payload builder's given-list mode; this serves candidate lists such as a forced-inclusion
    /// singleton build. It does not validate a committed Etna block, since it drops what fails
    /// instead of failing; [`BlockExecutor::execute_block`] and reth's `Executor::execute` do not
    /// filter.
    pub fn execute_block_with_committed_transactions<'tx>(
        mut self,
        transactions: impl IntoIterator<Item = Recovered<&'tx R::Transaction>>,
    ) -> Result<CommittedBlockExecutionOutcome<R::Transaction, R::Receipt>, BlockExecutionError>
    where
        R::Transaction: 'tx,
    {
        self.apply_pre_execution_changes()?;

        let requires_legacy_anchor = !self.spec.is_etna_active(self.evm.block().timestamp().to());
        let mut committed_transactions = Vec::new();
        for (idx, tx) in transactions.into_iter().enumerate() {
            let is_anchor_transaction = requires_legacy_anchor && idx == 0;
            if !requires_legacy_anchor && tx.inner().is_eip4844() {
                continue;
            }
            if !is_anchor_transaction && tx.signer() == Address::ZERO {
                continue;
            }

            let committed_tx = Recovered::new_unchecked((*tx.inner()).clone(), tx.signer());
            if self.try_execute_filtered(tx, is_anchor_transaction)? {
                committed_transactions.push(committed_tx);
            }
            if self.zk_gas_exhausted {
                if requires_legacy_anchor {
                    break;
                }
                self.clear_zk_gas_exhaustion();
            }
        }

        let execution_result = self.apply_post_execution_changes()?;
        Ok(CommittedBlockExecutionOutcome { execution_result, committed_transactions })
    }
}

/// Shared prover-mode filter: executes a single transaction and classifies the failure.
///
/// Lives on the inherent impl so both [`Self::execute_block_with_committed_transactions`] and the
/// pre-Etna trait-level [`BlockExecutor::execute_block`] route through one place — the filtering
/// rules (zk gas truncation, invalid tx, gas-exceeds-block) cannot drift between the two prover
/// entry points.
#[cfg(feature = "prover")]
impl<E, Spec, R> TaikoBlockExecutor<'_, E, Spec, R>
where
    E: Evm<
            DB: StateDB + DatabaseCommit,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
        > + TaikoZkGasEvm
        + TaikoAnchorEvm
        + TaikoSystemCallEvm,
    Spec: TaikoExecutorSpec + EthChainSpec + Clone,
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

impl<E, Spec, R> TaikoBlockExecutor<'_, E, Spec, R>
where
    E: Evm<
            DB: StateDB + DatabaseCommit,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
        > + TaikoZkGasEvm
        + TaikoAnchorEvm
        + TaikoSystemCallEvm,
    Spec: TaikoExecutorSpec + EthChainSpec + Clone,
    R: ReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt<Log = Log>>,
    <R::Transaction as TransactionEnvelope>::TxType: Send + 'static,
{
    /// Makes the Etna block-start call: `onBlockStart(generation, missBitmap)` on the spec's
    /// `EtnaStaking` proxy, from [`ETNA_SYSTEM_CALLER`], with both arguments from this block's own
    /// `extraData` and exactly [`ETNA_BLOCK_START_CALL_GAS`] gas for the call's frame.
    ///
    /// The call counts the missed signatures and stores the candidate snapshot. Like the EIP-2935
    /// and EIP-4788 writes it is block-start system processing: it changes neither the block's
    /// gas used, its receipts nor its zk gas, and its logs reach no receipt.
    ///
    /// Its state is committed only when it succeeds. A revert or an exceptional halt, running out
    /// of gas included, drops the state and keeps the block valid. A call to an address without
    /// code succeeds and runs no code; it touches that address, so an empty account there is
    /// removed with its storage (EIP-161), and it changes no other state.
    ///
    /// # Errors
    ///
    /// [`BlockExecutionError::Internal`], never a validation error, when the spec has no
    /// `EtnaStaking` address ([`MissingEtnaStakingAddress`] names the spec's chain), when the EVM
    /// reports a database or environment fault, or when `extraData` does not decode, which
    /// pre-execution validation rules out before this call.
    fn apply_etna_block_start_call(&mut self) -> Result<(), BlockExecutionError> {
        let block_number: u64 = self.evm.block().number().to();
        let Some(extra_data) = EtnaExtraData::decode(&self.ctx.extra_data) else {
            return Err(BlockExecutionError::msg(format!(
                "Etna block {block_number} reached the block-start call with undecodable extraData"
            )));
        };
        // The spec whose lookup fails names its own chain, which can differ from the EVM's.
        let staking = self.spec.etna_staking_address().ok_or_else(|| {
            BlockExecutionError::other(MissingEtnaStakingAddress {
                chain_id: self.spec.chain().id(),
            })
        })?;

        let ResultAndState { result, state } = self
            .evm
            .transact_unmetered_system_call(
                ETNA_SYSTEM_CALLER,
                staking,
                Bytes::copy_from_slice(&extra_data.on_block_start_calldata()),
                ETNA_BLOCK_START_CALL_GAS,
            )
            .map_err(|e| {
                BlockExecutionError::Internal(InternalBlockExecutionError::Other(e.into()))
            })?;

        // The warning names the failure and sizes its data but never logs the data: a reverting
        // frame can return megabytes, and the warning repeats on every node for every execution
        // of every block while the call keeps failing.
        match &result {
            ExecutionResult::Success { .. } => self.evm.db_mut().commit(state),
            ExecutionResult::Revert { gas, output, .. } => tracing::warn!(
                target: "taiko::executor",
                block_number,
                %staking,
                gas_used = gas.tx_gas_used(),
                output_len = output.len(),
                "Etna block-start staking call reverted; its state is discarded and the block stays valid"
            ),
            ExecutionResult::Halt { reason, gas, .. } => tracing::warn!(
                target: "taiko::executor",
                block_number,
                %staking,
                ?reason,
                gas_used = gas.tx_gas_used(),
                "Etna block-start staking call halted; its state is discarded and the block stays valid"
            ),
        }
        Ok(())
    }
}

impl<E, Spec, R> BlockExecutor for TaikoBlockExecutor<'_, E, Spec, R>
where
    E: Evm<
            DB: StateDB + DatabaseCommit,
            Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
        > + TaikoZkGasEvm
        + TaikoAnchorEvm
        + TaikoSystemCallEvm,
    Spec: TaikoExecutorSpec + EthChainSpec + Clone,
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

    /// Validates the root and `extraData`, runs the EIP-2935 then EIP-4788 system writes, and
    /// applies the once-per-block start processing.
    ///
    /// In a non-genesis Etna block that processing first makes the block-start staking call
    /// ([`Self::apply_etna_block_start_call`]), then installs the header's fee sharing. Before
    /// Etna, the legacy marker supplies the fee sharing and the golden-touch nonce for anchor
    /// execution. A repeated call on the same executor repeats the system writes but never the
    /// once-per-block processing. Every build, import, prover and witness path runs this method.
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        let is_etna_active = self.spec.is_etna_active(self.evm.block().timestamp().to());
        let block_number = self.evm.block().number().to();
        validate_etna_root(is_etna_active, block_number, self.ctx.parent_beacon_block_root)
            .map_err(BlockExecutionError::other)?;
        validate_etna_extra_data(is_etna_active, block_number, &self.ctx.extra_data)
            .map_err(BlockExecutionError::other)?;
        self.system_caller.apply_blockhashes_contract_call(self.ctx.parent_hash, &mut self.evm)?;
        self.system_caller
            .apply_beacon_root_contract_call(self.ctx.parent_beacon_block_root, &mut self.evm)?;

        // Apply the block-start call and the authoritative fee sharing once per block. The genesis
        // has no call: its empty extraData carries no arguments.
        if !self.block_start_applied {
            if is_etna_active && block_number != 0 {
                self.apply_etna_block_start_call()?;
            }

            // Decode the base fee share percentage from the block's extra data.
            let base_fee_share_pgtg =
                if self.spec.is_shasta_active(self.evm.block().timestamp().to()) {
                    decode_shasta_basefee_sharing_pctg(self.ctx.extra_data.as_ref()) as u64
                } else if self.spec.is_ontake_active_at_block(self.evm.block().number().to()) {
                    decode_post_ontake_extra_data(self.ctx.extra_data.clone())
                } else {
                    0
                };

            if is_etna_active {
                self.evm.set_block_fee_context(base_fee_share_pgtg);
            } else {
                let account_info = self
                    .evm
                    .db_mut()
                    .basic(Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS))
                    .map_err(|e| {
                        BlockExecutionError::Internal(InternalBlockExecutionError::Other(e.into()))
                    })?;
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
            }

            self.block_start_applied = true;
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
            return Err(self.zk_gas_error(ZkGasLimitExceeded));
        }

        let (tx_env, tx) = tx.into_parts();

        // Prover inputs encode an unrecoverable signer as the zero address. An Etna block is a
        // committed list that executes in full, so such a transaction, like a blob transaction
        // that consensus rejects on import, fails it on every entry point; candidate-list
        // derivation filters both before execution. Prover builds (including node tests) also
        // reject an Etna `eth_simulateV1` call without `from`, which reth fills with the zero
        // address; production builds do not enable this feature.
        #[cfg(feature = "prover")]
        if self.spec.is_etna_active(self.evm.block().timestamp().to()) {
            if *tx.signer() == Address::ZERO {
                return Err(BlockExecutionError::Validation(BlockValidationError::msg(format!(
                    "Etna transaction {} has no recoverable signer",
                    tx.tx().trie_hash()
                ))));
            }
            if tx.tx().is_eip4844() {
                return Err(BlockExecutionError::Validation(BlockValidationError::msg(format!(
                    "Etna transaction {} is a blob transaction",
                    tx.tx().trie_hash()
                ))));
            }
        }

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
        if let Err(ZkGasOutcome::LimitExceeded) = self.evm.charge_tx_intrinsic_zk_gas() {
            self.zk_gas_exhausted = true;
            self.reset_current_transaction_zk_gas();
            return Err(self.zk_gas_error(ZkGasLimitExceeded));
        }

        let result = match self.evm.transact(tx_env) {
            Ok(result) => result,
            Err(err) if err.to_string() == ZK_GAS_LIMIT_ERR => {
                self.zk_gas_exhausted = true;
                self.reset_current_transaction_zk_gas();
                return Err(self.zk_gas_error(ZkGasLimitExceeded));
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
            return Err(self.zk_gas_error(ZkGasLimitExceeded));
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
    ///
    /// Before Etna, prover builds keep the legacy filtering here: invalid non-anchor transactions
    /// are skipped and zk gas exhaustion stops the block. An Etna block is a committed list, so
    /// every listed transaction executes as on node import and any failure fails the block.
    /// Candidate-list derivation at Etna, such as a forced-inclusion singleton build, uses
    /// [`Self::execute_block_with_committed_transactions`].
    #[cfg(feature = "prover")]
    fn execute_block(
        mut self,
        transactions: impl IntoIterator<Item = impl ExecutableTx<Self>>,
    ) -> Result<BlockExecutionResult<Self::Receipt>, BlockExecutionError>
    where
        Self: Sized,
    {
        self.apply_pre_execution_changes()?;

        if self.spec.is_etna_active(self.evm.block().timestamp().to()) {
            for tx in transactions {
                self.execute_transaction(tx)?;
            }
            return self.apply_post_execution_changes();
        }

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

    use alloy_consensus::{Signed, TxLegacy, transaction::Recovered};
    use alloy_eips::{eip2935::HISTORY_STORAGE_ADDRESS, eip4788::BEACON_ROOTS_ADDRESS};
    use alloy_evm::EvmFactory;
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U64, U256};
    use reth_ethereum_primitives::{Receipt, TransactionSigned};
    use reth_evm::{ConfigureEvm, block::BlockExecutor};
    use reth_evm_ethereum::RethReceiptBuilder;
    use reth_primitives_traits::SignedTransaction;
    use reth_revm::{
        State,
        db::{CacheDB, EmptyDB, InMemoryDB, states::bundle_state::BundleRetention},
        state::{AccountInfo, Bytecode, bytecode::opcode},
    };

    use alethia_reth_evm::{
        alloy::decode_anchor_system_call_data,
        env::TaikoEvmEnv,
        factory::TaikoEvmFactory,
        spec::TaikoSpecId,
        zk_gas::unzen::{BLOCK_ZK_GAS_LIMIT, TX_INTRINSIC_ZK_GAS},
    };

    use super::*;
    use crate::{
        config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes},
        testutil::{
            BENCH_LIMIT_TARGET, BENCH_SUCCESS_TARGET, FailingStorageDb, db_with_contracts,
            etna_chain_spec, etna_evm_env, etna_execution_ctx, etna_test_staking_address,
            insert_contract, insert_system_contracts, recovered_tx, slot_recorder_bytecode,
            unzen_chain_spec, unzen_evm_env, unzen_execution_ctx,
        },
    };
    use alethia_reth_chainspec::spec::TaikoChainSpec;
    const BENCH_CALLER: Address = Address::with_last_byte(0x30);

    /// Beacon root the block-start call tests commit to.
    const BLOCK_START_ROOT: B256 = B256::repeat_byte(0x42);

    /// `PUSH1 1 PUSH1 0 SSTORE STOP`: sets storage slot 0 to 1.
    const SET_SLOT_ZERO: [u8; 6] =
        [opcode::PUSH1, 1, opcode::PUSH1, 0, opcode::SSTORE, opcode::STOP];

    /// Executes one Etna block over `db` under [`etna_chain_spec`] and [`etna_evm_env`]:
    /// pre-execution, then each of `txs`, then `finish()`, each of which must succeed. Returns the
    /// state, which records bundle transitions, the block result and the block's finalized zk gas.
    fn run_etna_block(
        db: InMemoryDB,
        ctx: TaikoBlockExecutionCtx<'_>,
        txs: &[Recovered<TransactionSigned>],
    ) -> (State<InMemoryDB>, BlockExecutionResult<Receipt>, u64) {
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
        let zk_gas = ctx.clone();
        let mut executor = TaikoBlockExecutor::new(
            evm,
            ctx,
            Arc::new(etna_chain_spec()),
            RethReceiptBuilder::default(),
        );
        executor.apply_pre_execution_changes().expect("pre-execution must succeed");
        for tx in txs {
            executor.execute_transaction(tx.clone()).expect("the transaction must execute");
        }
        let (evm, result) = executor.finish().expect("the block must finish");
        drop(evm);
        (state, result, zk_gas.finalized_block_zk_gas())
    }

    /// Returns the code of a `CALL` to `target` whose 32-byte calldata is the word `word` pushes.
    /// It leaves the call's success flag on the stack and the returned word at memory 32.
    fn call_with_word(word: &[u8], target: Address) -> Vec<u8> {
        let mut code = word.to_vec();
        // memory[0..32] = word; CALL(gas, target, 0, 0, 32, 32, 32).
        code.extend_from_slice(&[
            opcode::PUSH1,
            0,
            opcode::MSTORE,
            opcode::PUSH1,
            32,
            opcode::PUSH1,
            32,
            opcode::PUSH1,
            32,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::PUSH20,
        ]);
        code.extend_from_slice(target.as_slice());
        code.extend_from_slice(&[opcode::GAS, opcode::CALL]);
        code
    }

    /// Returns `address` as a 32-byte word, the way `CALLER` and `ORIGIN` push it.
    fn address_word(address: Address) -> U256 {
        U256::from_be_slice(address.as_slice())
    }

    /// One recorded tracing event: each field's name and its `Debug` text, `message` included.
    type RecordedEvent = Vec<(&'static str, String)>;

    /// A tracing subscriber that records every event emitted while it is the thread's default.
    #[derive(Clone, Default)]
    struct EventRecorder(Arc<std::sync::Mutex<Vec<RecordedEvent>>>);

    impl EventRecorder {
        /// Removes every recorded event and returns those whose message contains `needle`.
        fn take_matching(&self, needle: &str) -> Vec<RecordedEvent> {
            let events = std::mem::take(&mut *self.0.lock().expect("recorder lock"));
            events
                .into_iter()
                .filter(|event| {
                    event_field(event, "message").is_some_and(|message| message.contains(needle))
                })
                .collect()
        }
    }

    /// Returns the `Debug` text of `event`'s field `name`, if it has one.
    fn event_field<'a>(event: &'a RecordedEvent, name: &str) -> Option<&'a str> {
        event.iter().find(|(field, _)| *field == name).map(|(_, value)| value.as_str())
    }

    impl tracing::Subscriber for EventRecorder {
        /// Records every event.
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        /// Spans are not recorded; every span gets the same id.
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        /// Spans are not recorded.
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        /// Spans are not recorded.
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        /// Records the event's fields.
        fn event(&self, event: &tracing::Event<'_>) {
            /// Collects each visited field's name and `Debug` text.
            struct Fields(RecordedEvent);
            impl tracing::field::Visit for Fields {
                /// Records the field's `Debug` text.
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push((field.name(), format!("{value:?}")));
                }
            }
            let mut fields = Fields(Vec::new());
            event.record(&mut fields);
            self.0.lock().expect("recorder lock").push(fields.0);
        }

        /// Spans are not recorded.
        fn enter(&self, _: &tracing::span::Id) {}

        /// Spans are not recorded.
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn etna_block_start_call_follows_both_system_writes_with_its_own_arguments() {
        let staking = etna_test_staking_address();
        let parent_hash = B256::repeat_byte(0x77);
        let mut db = db_with_contracts(&[]);
        insert_system_contracts(&mut db);
        insert_contract(
            &mut db,
            staking,
            slot_recorder_bytecode(&[
                &[opcode::GAS],
                &[opcode::CALLER],
                &[opcode::ORIGIN],
                &[opcode::CALLDATASIZE],
                &[opcode::PUSH1, 0, opcode::CALLDATALOAD, opcode::PUSH1, 224, opcode::SHR],
                &[opcode::PUSH1, 4, opcode::CALLDATALOAD],
                &[opcode::PUSH1, 36, opcode::CALLDATALOAD],
                &call_with_word(&[opcode::TIMESTAMP], BEACON_ROOTS_ADDRESS),
                &[opcode::PUSH1, 32, opcode::MLOAD],
                &call_with_word(
                    &[opcode::PUSH1, 1, opcode::NUMBER, opcode::SUB],
                    HISTORY_STORAGE_ADDRESS,
                ),
                &[opcode::PUSH1, 32, opcode::MLOAD],
            ]),
        );
        let mut ctx = etna_execution_ctx(BLOCK_START_ROOT);
        ctx.parent_hash = parent_hash;
        ctx.extra_data =
            Bytes::copy_from_slice(&EtnaExtraData::new(1, 258, 5).expect("fields fit").encode());

        let (mut state, _, _) = run_etna_block(db, ctx, &[]);

        // Slot 0 is the whole 30,000,000 less the first GAS opcode's own 2. Slots 7 and 9 are the
        // success flags of reading back the beacon root and the parent hash this block's two
        // system writes stored, so both writes ran before the call.
        let recorded: Vec<U256> = (0..11u64)
            .map(|slot| state.storage(staking, U256::from(slot)).expect("db read"))
            .collect();
        assert_eq!(
            recorded,
            [
                U256::from(29_999_998u64),
                address_word(ETNA_SYSTEM_CALLER),
                address_word(ETNA_SYSTEM_CALLER),
                U256::from(68u64),
                U256::from(0xea98_7592u64),
                U256::from(1u64),
                U256::from(5u64),
                U256::from(1u64),
                U256::from_be_bytes(BLOCK_START_ROOT.0),
                U256::from(1u64),
                U256::from_be_bytes(parent_hash.0),
            ]
        );
        assert_eq!(state.basic(ETNA_SYSTEM_CALLER).expect("db read"), None);
    }

    #[test]
    fn etna_block_start_call_is_outside_gas_used_receipts_and_zk_gas() {
        let staking = etna_test_staking_address();
        let sink = Address::with_last_byte(0x55);
        let caller = Address::with_last_byte(0x31);
        // Sets slot 0, calls the sink, emits a log with topic 1, then hashes 64 KiB 300 times,
        // which a transaction's zk gas meter would stop.
        let mut code = vec![opcode::PUSH1, 1, opcode::PUSH1, 0, opcode::SSTORE];
        code.extend_from_slice(&[
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::PUSH20,
        ]);
        code.extend_from_slice(sink.as_slice());
        code.extend_from_slice(&[
            opcode::GAS,
            opcode::CALL,
            opcode::POP,
            opcode::PUSH1,
            1,
            opcode::PUSH1,
            0,
            opcode::PUSH1,
            0,
            opcode::LOG1,
            opcode::PUSH2,
            0x01,
            0x2c, // 300 iterations
        ]);
        let loop_start = u8::try_from(code.len()).expect("the loop starts below offset 256");
        code.extend_from_slice(&[
            opcode::JUMPDEST,
            opcode::PUSH3,
            0x01,
            0x00,
            0x00,
            opcode::PUSH1,
            0,
            opcode::KECCAK256,
            opcode::POP,
            opcode::PUSH1,
            1,
            opcode::SWAP1,
            opcode::SUB,
            opcode::DUP1,
            opcode::PUSH1,
            loop_start,
            opcode::JUMPI,
            opcode::POP,
            opcode::STOP,
        ]);
        let mock = Bytecode::new_raw(code.into());
        let db = || {
            let mut db = db_with_contracts(&[(caller, 0)]);
            insert_contract(&mut db, sink, Bytecode::new_raw(SET_SLOT_ZERO.into()));
            db
        };
        let mut with_mock = db();
        insert_contract(&mut with_mock, staking, mock.clone());
        let tx = recovered_tx(caller, BENCH_SUCCESS_TARGET, 0, 1);

        let (mut state_a, a, zk_gas_a) = run_etna_block(
            with_mock,
            etna_execution_ctx(BLOCK_START_ROOT),
            std::slice::from_ref(&tx),
        );
        let (_, b, zk_gas_b) = run_etna_block(db(), etna_execution_ctx(BLOCK_START_ROOT), &[tx]);

        assert_eq!(a.gas_used, b.gas_used);
        assert_eq!(a.receipts, b.receipts);
        assert_eq!(a.receipts.len(), 1);
        assert!(a.receipts.iter().all(|receipt| receipt.logs.is_empty()));
        assert_eq!(zk_gas_a, zk_gas_b);
        assert_eq!(state_a.storage(staking, U256::ZERO).expect("db read"), U256::from(1u64));
        assert_eq!(state_a.storage(sink, U256::ZERO).expect("db read"), U256::from(1u64));

        // The same code run as a transaction exceeds the block zk gas limit.
        let target = Address::with_last_byte(0x56);
        let mut db = db();
        insert_contract(&mut db, target, mock);
        let mut state = State::builder().with_database(db).build();
        let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
        let mut executor = TaikoBlockExecutor::new(
            evm,
            etna_execution_ctx(BLOCK_START_ROOT),
            Arc::new(etna_chain_spec()),
            RethReceiptBuilder::default(),
        );
        let err = executor
            .execute_transaction(recovered_tx(caller, target, 0, 1))
            .expect_err("the mock is zk metered as a transaction");
        assert!(is_zk_gas_limit_exceeded(&err), "{err:?}");
    }

    /// A failed call drops its state and keeps the block valid. Its one warning reports the failure
    /// kind, the gas used and the length of the returned data, never the data itself.
    #[test]
    fn failed_etna_block_start_call_leaves_the_state_and_the_block_valid() {
        let staking = etna_test_staking_address();
        let caller = Address::with_last_byte(0x31);
        let tx = recovered_tx(caller, BENCH_SUCCESS_TARGET, 0, 1);
        // (case, code after the store, the warning's verb, a field the warning reports, its value)
        let failures: [(&str, &[u8], &str, &str, &str); 3] = [
            // REVERT(0, 0x100000): reverts with 1 MiB of data, which the warning reports only by
            // its length.
            (
                "revert",
                &[opcode::PUSH3, 0x10, 0, 0, opcode::PUSH1, 0, opcode::REVERT],
                "reverted",
                "output_len",
                "1048576",
            ),
            ("invalid opcode", &[opcode::INVALID], "halted", "reason", "InvalidFEOpcode"),
            // Loops at offset 5 until the 30,000,000 gas run out.
            (
                "out of gas",
                &[opcode::JUMPDEST, opcode::PUSH1, 5, opcode::JUMP],
                "halted",
                "gas_used",
                "30000000",
            ),
        ];
        let recorder = EventRecorder::default();
        for (name, failure, verb, field, value) in failures {
            let mut code = vec![opcode::PUSH1, 1, opcode::PUSH1, 0, opcode::SSTORE];
            code.extend_from_slice(failure);
            let mut db = db_with_contracts(&[(caller, 0)]);
            insert_contract(&mut db, staking, Bytecode::new_raw(code.into()));

            let run = || {
                run_etna_block(db, etna_execution_ctx(BLOCK_START_ROOT), std::slice::from_ref(&tx))
            };
            let (mut state, result, _) = tracing::subscriber::with_default(recorder.clone(), run);

            assert_eq!(result.receipts.len(), 1, "{name}");
            assert_eq!(state.storage(staking, U256::ZERO).expect("db read"), U256::ZERO, "{name}");
            let warnings = recorder.take_matching("block-start staking call");
            assert_eq!(warnings.len(), 1, "{name}: {warnings:?}");
            let warning = &warnings[0];
            assert!(event_field(warning, "message").is_some_and(|m| m.contains(verb)), "{name}");
            assert!(event_field(warning, field).is_some_and(|v| v.contains(value)), "{warning:?}");
            assert!(event_field(warning, "gas_used").is_some(), "{name}: {warning:?}");
            let logged: usize = warning.iter().map(|(_, text)| text.len()).sum();
            assert!(logged < 512, "{name}: the warning logs {logged} bytes");
        }

        let (mut state, result, _) = tracing::subscriber::with_default(recorder.clone(), || {
            run_etna_block(
                db_with_contracts(&[(caller, 0)]),
                etna_execution_ctx(BLOCK_START_ROOT),
                &[tx],
            )
        });
        assert_eq!(result.receipts.len(), 1);
        assert_eq!(state.basic(staking).expect("db read"), None);
        assert!(recorder.take_matching("block-start staking call").is_empty());
    }

    /// Vectors 3.2 13.aa and 13.ab (CONS-3.2-13): when the staking address holds an empty account
    /// (zero nonce and balance, no code) with storage, which only a genesis can hold, the call runs
    /// no code but touches that address, so EIP-161 removes the account with its storage. The
    /// block stays valid, and the call changes no other state.
    #[test]
    fn etna_block_start_call_removes_an_empty_staking_account_with_its_storage() {
        let staking = etna_test_staking_address();
        let caller = Address::with_last_byte(0x31);
        let slot = U256::from(7u64);
        let tx = recovered_tx(caller, BENCH_SUCCESS_TARGET, 0, 1);
        let mut with_account = db_with_contracts(&[(caller, 0)]);
        with_account.insert_account_info(staking, AccountInfo::default());
        with_account.insert_account_storage(staking, slot, U256::from(9u64)).expect("db write");
        assert_eq!(with_account.basic(staking).expect("db read"), Some(AccountInfo::default()));

        let (mut state, result, zk_gas) = run_etna_block(
            with_account,
            etna_execution_ctx(BLOCK_START_ROOT),
            std::slice::from_ref(&tx),
        );
        // The same block over the same state without the account.
        let (mut baseline_state, baseline, baseline_zk_gas) = run_etna_block(
            db_with_contracts(&[(caller, 0)]),
            etna_execution_ctx(BLOCK_START_ROOT),
            &[tx],
        );

        // 13.aa: the block is valid, and the account and its storage are gone (13.ab rejects an EL
        // that keeps either).
        assert_eq!(result.receipts.len(), 1);
        assert_eq!(state.basic(staking).expect("db read"), None);
        assert_eq!(state.storage(staking, slot).expect("db read"), U256::ZERO);
        state.merge_transitions(BundleRetention::Reverts);
        let mut bundle = state.take_bundle();
        let removed = bundle.state.remove(&staking).expect("the call changes the staking address");
        assert!(removed.was_destroyed(), "{removed:?}");
        assert_eq!(removed.info, None);
        assert_eq!(removed.storage_slot(slot), Some(U256::ZERO));

        // 13.aa: apart from that removal, the block's state, gas, receipts and zk gas are those of
        // the same block over a state without the account.
        baseline_state.merge_transitions(BundleRetention::Reverts);
        let baseline_bundle = baseline_state.take_bundle();
        assert_eq!(bundle.state, baseline_bundle.state);
        assert_eq!(bundle.contracts, baseline_bundle.contracts);
        assert_eq!(result.gas_used, baseline.gas_used);
        assert_eq!(result.receipts, baseline.receipts);
        assert_eq!(zk_gas, baseline_zk_gas);
    }

    #[test]
    fn etna_block_start_call_runs_once_and_only_in_non_genesis_etna_blocks() {
        let staking = etna_test_staking_address();
        // Increments storage slot 0.
        let counter = Bytecode::new_raw(Bytes::from_static(&[
            opcode::PUSH1,
            0,
            opcode::SLOAD,
            opcode::PUSH1,
            1,
            opcode::ADD,
            opcode::PUSH1,
            0,
            opcode::SSTORE,
            opcode::STOP,
        ]));
        let db = || {
            let mut db = db_with_contracts(&[]);
            insert_contract(&mut db, staking, counter.clone());
            db
        };
        let run = |spec: TaikoChainSpec,
                   env: TaikoEvmEnv,
                   ctx: TaikoBlockExecutionCtx<'_>,
                   times: usize| {
            let mut state = State::builder().with_database(db()).build();
            let evm = TaikoEvmFactory.create_evm(&mut state, env);
            let mut executor =
                TaikoBlockExecutor::new(evm, ctx, Arc::new(spec), RethReceiptBuilder::default());
            for _ in 0..times {
                executor.apply_pre_execution_changes().expect("pre-execution must succeed");
            }
            let (evm, _) = executor.finish().expect("the block must finish");
            drop(evm);
            state.storage(staking, U256::ZERO).expect("db read")
        };

        let twice = run(etna_chain_spec(), etna_evm_env(), etna_execution_ctx(BLOCK_START_ROOT), 2);
        assert_eq!(twice, U256::from(1u64));

        let pre_etna = run(unzen_chain_spec(), unzen_evm_env(), unzen_execution_ctx(), 1);
        assert_eq!(pre_etna, U256::ZERO);

        let mut genesis_env = etna_evm_env();
        genesis_env.block_env.number = U256::ZERO;
        let mut genesis_ctx = etna_execution_ctx(B256::ZERO);
        genesis_ctx.extra_data = Bytes::new();
        let genesis = run(etna_chain_spec(), genesis_env, genesis_ctx, 1);
        assert_eq!(genesis, U256::ZERO);
    }

    #[test]
    fn etna_block_start_call_effects_survive_a_discarded_first_transaction() {
        let staking = etna_test_staking_address();
        let caller = Address::with_last_byte(0x31);
        let mut db = db_with_contracts(&[(caller, 0)]);
        insert_contract(&mut db, staking, Bytecode::new_raw(SET_SLOT_ZERO.into()));
        let mut state = State::builder().with_database(db).build();
        let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
        let mut executor = TaikoBlockExecutor::new(
            evm,
            etna_execution_ctx(BLOCK_START_ROOT),
            Arc::new(etna_chain_spec()),
            RethReceiptBuilder::default(),
        );
        executor.apply_pre_execution_changes().expect("pre-execution must succeed");

        let err = executor
            .execute_transaction(recovered_tx(caller, BENCH_LIMIT_TARGET, 0, 1))
            .expect_err("the first transaction overflows the zk gas budget");
        assert!(is_zk_gas_limit_exceeded(&err), "{err:?}");
        executor.clear_zk_gas_exhaustion();
        let (evm, result) = executor.finish().expect("the block must finish");
        drop(evm);

        assert!(result.receipts.is_empty());
        assert_eq!(state.storage(staking, U256::ZERO).expect("db read"), U256::from(1u64));
    }

    #[test]
    fn etna_block_start_call_leaves_the_first_transaction_cold() {
        let staking = etna_test_staking_address();
        let caller = Address::with_last_byte(0x31);
        let probe = Address::with_last_byte(0xC0);
        // Stores the gas difference around `PUSH20 staking BALANCE POP GAS`, which is the BALANCE
        // cost plus 7: 107 for a warm address and 2,607 for a cold one.
        let mut balance_cost = vec![opcode::GAS, opcode::PUSH20];
        balance_cost.extend_from_slice(staking.as_slice());
        balance_cost.extend_from_slice(&[
            opcode::BALANCE,
            opcode::POP,
            opcode::GAS,
            opcode::SWAP1,
            opcode::SUB,
        ]);
        let mut db = db_with_contracts(&[(caller, 0)]);
        insert_contract(&mut db, staking, Bytecode::new_raw(SET_SLOT_ZERO.into()));
        insert_contract(&mut db, probe, slot_recorder_bytecode(&[&balance_cost]));

        let (mut state, _, _) = run_etna_block(
            db,
            etna_execution_ctx(BLOCK_START_ROOT),
            &[recovered_tx(caller, probe, 0, 1)],
        );

        assert_eq!(state.storage(staking, U256::ZERO).expect("db read"), U256::from(1u64));
        assert_eq!(state.storage(probe, U256::ZERO).expect("db read"), U256::from(2_607u64));
    }

    #[test]
    fn etna_block_start_call_faults_are_internal_errors() {
        let staking = etna_test_staking_address();
        // Sets slot 1, then reads slot 0, the read the database fails.
        let mut inner = db_with_contracts(&[]);
        insert_contract(
            &mut inner,
            staking,
            Bytecode::new_raw(Bytes::from_static(&[
                opcode::PUSH1,
                1,
                opcode::PUSH1,
                1,
                opcode::SSTORE,
                opcode::PUSH1,
                0,
                opcode::SLOAD,
                opcode::STOP,
            ])),
        );
        let mut state = State::builder()
            .with_database(FailingStorageDb { inner, failing: (staking, U256::ZERO) })
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
        let mut executor = TaikoBlockExecutor::new(
            evm,
            etna_execution_ctx(BLOCK_START_ROOT),
            Arc::new(etna_chain_spec()),
            RethReceiptBuilder::default(),
        );
        let err = executor
            .apply_pre_execution_changes()
            .expect_err("a database fault must fail pre-execution");
        assert!(matches!(err, BlockExecutionError::Internal(_)), "{err:?}");
        drop(executor);
        assert_eq!(state.storage(staking, U256::from(1u64)).expect("db read"), U256::ZERO);

        // The address comes from the spec, so the error names the spec's chain, not the EVM
        // environment's chain id 167.
        let mut spec = etna_chain_spec();
        spec.inner.chain = 167_000.into();
        let mut state = State::builder().with_database(db_with_contracts(&[])).build();
        let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
        let mut executor = TaikoBlockExecutor::new(
            evm,
            etna_execution_ctx(BLOCK_START_ROOT),
            Arc::new(spec),
            RethReceiptBuilder::default(),
        );
        let err = executor
            .apply_pre_execution_changes()
            .expect_err("a missing staking address must fail pre-execution");
        assert!(matches!(err, BlockExecutionError::Internal(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("EtnaStaking") && text.contains("chain 167000"), "{text}");
    }

    #[test]
    fn etna_pre_execution_rejects_missing_root_and_malformed_extra_data() {
        let root = B256::with_last_byte(7);
        let cases: [(Option<B256>, Option<Bytes>, &str); 6] = [
            (None, None, "beacon"),
            (Some(B256::ZERO), None, "beacon"),
            (Some(root), Some(Bytes::new()), "extraData"),
            (Some(root), Some(Bytes::from(vec![0; 7])), "extraData"),
            (Some(root), Some(Bytes::from(vec![100; 13])), "extraData"),
            (Some(root), Some(Bytes::from(vec![25; 29])), "basefeeSharingPctg"),
        ];
        for (parent_beacon_block_root, extra_data, expected) in cases {
            let mut state = State::builder().with_database(db_with_contracts(&[])).build();
            let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
            let mut ctx = etna_execution_ctx(root);
            ctx.parent_beacon_block_root = parent_beacon_block_root;
            if let Some(extra_data) = extra_data {
                ctx.extra_data = extra_data;
            }
            let mut executor = TaikoBlockExecutor::new(
                evm,
                ctx,
                Arc::new(etna_chain_spec()),
                RethReceiptBuilder::default(),
            );
            let err = executor.apply_pre_execution_changes().expect_err(expected);
            assert!(err.to_string().contains(expected), "{expected}: {err}");
        }
    }

    #[test]
    fn etna_pre_execution_shares_fees_without_anchor_exemption() {
        // At Etna, pre-execution installs the header fee share and never the legacy anchor marker,
        // so a golden-touch call to the treasury pays fees like any other transaction. The header
        // share (100) differs from the environment's (0), so the beneficiary's credit pins it.
        let golden = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
        let treasury = get_treasury_address(167);
        let mut state = State::builder().with_database(db_with_contracts(&[(golden, 0)])).build();
        let mut env = etna_evm_env();
        env.block_env.basefee = 10;
        env.block_env.beneficiary = Address::with_last_byte(0xBB);
        let evm = TaikoEvmFactory.create_evm(&mut state, env);
        let ctx = etna_execution_ctx(B256::with_last_byte(7));
        let mut executor = TaikoBlockExecutor::new(
            evm,
            ctx,
            Arc::new(etna_chain_spec()),
            RethReceiptBuilder::default(),
        );
        executor.apply_pre_execution_changes().unwrap();
        let result = executor
            .execute_transaction_without_commit(recovered_tx(golden, treasury, 0, 10))
            .unwrap();
        assert_eq!(result.result.result.tx_gas_used(), 21_000);
        assert_eq!(
            result.result.state[&golden].info.balance,
            U256::from(10_000_000_000u64 - 210_000)
        );
        assert_eq!(result.result.state[&treasury].info.balance, U256::ZERO);
        assert_eq!(
            result.result.state[&Address::with_last_byte(0xBB)].info.balance,
            U256::from(210_000)
        );
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
    fn clear_zk_gas_exhaustion_lets_the_next_transaction_run() {
        let caller_a = Address::with_last_byte(0x31);
        let caller_b = Address::with_last_byte(0x32);
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(caller_a, 0), (caller_b, 0)]))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let ctx = unzen_execution_ctx();
        let mut executor =
            TaikoBlockExecutor::new(evm, ctx.clone(), chain_spec, RethReceiptBuilder::default());

        let err = executor
            .execute_transaction(recovered_tx(caller_a, BENCH_LIMIT_TARGET, 0, 1))
            .expect_err("the overflowing transaction should exceed the zk gas limit");
        assert!(is_zk_gas_limit_exceeded(&err));
        assert_eq!(ctx.finalized_block_zk_gas(), 0);

        let sticky_err = executor
            .execute_transaction(recovered_tx(caller_b, BENCH_SUCCESS_TARGET, 0, 1))
            .expect_err("zk gas exhaustion should stay sticky until cleared");
        assert!(is_zk_gas_limit_exceeded(&sticky_err));

        executor.clear_zk_gas_exhaustion();
        executor
            .execute_transaction(recovered_tx(caller_b, BENCH_SUCCESS_TARGET, 0, 1))
            .expect("the next transaction should run once exhaustion is cleared");
        assert_eq!(executor.receipts().len(), 1);
        assert!(ctx.finalized_block_zk_gas() >= TX_INTRINSIC_ZK_GAS);
        assert!(ctx.finalized_block_zk_gas() < BLOCK_ZK_GAS_LIMIT);
    }

    #[test]
    fn overflowed_transaction_leaves_no_state_after_clearing_exhaustion() {
        let caller_a = Address::with_last_byte(0x31);
        let caller_b = Address::with_last_byte(0x32);
        let writer = Address::with_last_byte(0x25);
        let mut db = db_with_contracts(&[(caller_a, 0), (caller_b, 0)]);
        // Sets storage slot 0 to 1, then runs the 64 KiB KECCAK256 loop, so the transaction
        // overflows the zk gas budget after it has written storage.
        insert_contract(
            &mut db,
            writer,
            Bytecode::new_raw(Bytes::from(vec![
                opcode::PUSH1,
                0x01,
                opcode::PUSH1,
                0x00,
                opcode::SSTORE,   // slot 0 = 1
                opcode::JUMPDEST, // loop entry at offset 5
                opcode::PUSH3,
                0x01,
                0x00,
                0x00, // len 0x010000 (64 KiB)
                opcode::PUSH1,
                0x00, // memory offset 0
                opcode::KECCAK256,
                opcode::POP, // discard the digest
                opcode::PUSH1,
                0x05,         // jump destination 5
                opcode::JUMP, // loop
            ])),
        );
        let funded_balance =
            db.basic(caller_a).expect("db read").expect("caller a is funded").balance;
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let ctx = unzen_execution_ctx();
        let mut executor =
            TaikoBlockExecutor::new(evm, ctx.clone(), chain_spec, RethReceiptBuilder::default());

        let err = executor
            .execute_transaction(recovered_tx(caller_a, writer, 0, 1))
            .expect_err("the writer transaction should exceed the zk gas limit");
        assert!(is_zk_gas_limit_exceeded(&err));
        assert_eq!(ctx.finalized_block_zk_gas(), 0);

        executor.clear_zk_gas_exhaustion();
        executor
            .execute_transaction(recovered_tx(caller_b, BENCH_SUCCESS_TARGET, 0, 1))
            .expect("the next transaction should run once exhaustion is cleared");
        let (evm, _) = executor.finish().expect("executor should finish");
        drop(evm);

        // The overflowed transaction left nothing behind: no nonce bump, no fee charge and no
        // storage write. Only the next transaction's changes are in the state.
        let a = state.basic(caller_a).expect("db read").expect("caller a exists");
        assert_eq!(a.nonce, 0);
        assert_eq!(a.balance, funded_balance);
        assert_eq!(state.storage(writer, U256::ZERO).expect("db read"), U256::ZERO);
        assert_eq!(state.basic(caller_b).expect("db read").expect("caller b exists").nonce, 1);
        let finalized = ctx.finalized_block_zk_gas();
        assert!(finalized >= TX_INTRINSIC_ZK_GAS);
        assert!(finalized < BLOCK_ZK_GAS_LIMIT);

        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        assert!(bundle.account(&caller_a).is_none());
        assert!(bundle.account(&writer).is_none());
        assert!(bundle.account(&caller_b).is_some());
    }

    #[test]
    fn can_fit_another_transaction_zk_gas_tracks_the_intrinsic() {
        let chain_spec = Arc::new(unzen_chain_spec());

        {
            let mut state =
                State::builder().with_database(db_with_contracts(&[])).with_bundle_update().build();
            let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
            let mut executor = TaikoBlockExecutor::new(
                evm,
                unzen_execution_ctx(),
                chain_spec.clone(),
                RethReceiptBuilder::default(),
            );
            executor
                .reserve_block_zk_gas(BLOCK_ZK_GAS_LIMIT - TX_INTRINSIC_ZK_GAS)
                .expect("reserve should fit");
            assert!(executor.can_fit_another_transaction_zk_gas());
        }

        {
            let mut state =
                State::builder().with_database(db_with_contracts(&[])).with_bundle_update().build();
            let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
            let mut executor = TaikoBlockExecutor::new(
                evm,
                unzen_execution_ctx(),
                chain_spec.clone(),
                RethReceiptBuilder::default(),
            );
            executor
                .reserve_block_zk_gas(BLOCK_ZK_GAS_LIMIT - TX_INTRINSIC_ZK_GAS + 1)
                .expect("reserve should fit");
            assert!(!executor.can_fit_another_transaction_zk_gas());
        }

        {
            let mut state =
                State::builder().with_database(db_with_contracts(&[])).with_bundle_update().build();
            let mut env = unzen_evm_env();
            env.cfg_env.spec = TaikoSpecId::SHASTA;
            let evm = TaikoEvmFactory.create_evm(&mut state, env);
            let mut executor = TaikoBlockExecutor::new(
                evm,
                unzen_execution_ctx(),
                chain_spec,
                RethReceiptBuilder::default(),
            );
            executor
                .reserve_block_zk_gas(BLOCK_ZK_GAS_LIMIT - TX_INTRINSIC_ZK_GAS + 1)
                .expect("pre-Unzen reserve should be a no-op");
            assert!(executor.can_fit_another_transaction_zk_gas());
        }
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

    #[test]
    fn etna_executor_charges_transaction_intrinsic_once() {
        let mut db = db_with_contracts(&[(BENCH_CALLER, 0)]);
        insert_contract(
            &mut db,
            BENCH_SUCCESS_TARGET,
            Bytecode::new_raw(Bytes::from(vec![opcode::STOP])),
        );
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
        let ctx = etna_execution_ctx(B256::with_last_byte(1));
        let mut executor = TaikoBlockExecutor::new(
            evm,
            ctx.clone(),
            Arc::new(etna_chain_spec()),
            RethReceiptBuilder::default(),
        );
        executor.apply_pre_execution_changes().expect("block-start system calls");
        executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_SUCCESS_TARGET, 0, 1))
            .unwrap();
        assert_eq!(ctx.finalized_block_zk_gas(), 243_000);
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
        // Before Etna, zk-gas failures keep the internal mapping (JSON-RPC -32603), not INVALID.
        assert!(matches!(err, BlockExecutionError::Internal(_)), "{err:?}");
        assert!(is_zk_gas_difficulty_mismatch(&err));
        assert!(err.to_string().contains("difficulty"));
    }

    #[test]
    fn etna_zk_exhaustion_is_validation_and_remains_recoverable_for_derivation() {
        let mut state =
            State::builder().with_database(db_with_contracts(&[(BENCH_CALLER, 0)])).build();
        let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
        let ctx = etna_execution_ctx(B256::with_last_byte(1));
        let mut executor = TaikoBlockExecutor::new(
            evm,
            ctx,
            Arc::new(etna_chain_spec()),
            RethReceiptBuilder::default(),
        );
        executor.apply_pre_execution_changes().unwrap();
        let err = executor
            .execute_transaction(recovered_tx(BENCH_CALLER, BENCH_LIMIT_TARGET, 0, 1))
            .unwrap_err();
        assert!(matches!(err, BlockExecutionError::Validation(_)), "{err:?}");
        assert!(is_zk_gas_limit_exceeded(&err));
        assert!(is_recoverable_non_anchor_tx_error(&err));
    }

    #[test]
    fn etna_difficulty_mismatch_is_a_nonrecoverable_validation_error() {
        let mut state = State::builder().with_database(db_with_contracts(&[])).build();
        let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
        let mut ctx = etna_execution_ctx(B256::with_last_byte(1));
        ctx.expected_difficulty = Some(U256::from(1));
        let executor = TaikoBlockExecutor::new(
            evm,
            ctx,
            Arc::new(etna_chain_spec()),
            RethReceiptBuilder::default(),
        );
        let err = executor.validate_expected_zk_gas_difficulty().unwrap_err();
        assert!(matches!(err, BlockExecutionError::Validation(_)), "{err:?}");
        assert!(is_zk_gas_difficulty_mismatch(&err));
        assert!(!is_recoverable_non_anchor_tx_error(&err));
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
