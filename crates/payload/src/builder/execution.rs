//! Transaction execution helpers used by Taiko payload building.

use alloy_consensus::Transaction;
use alloy_eips::eip4844::BYTES_PER_BLOB;
use alloy_primitives::B256;
use alloy_rlp::Encodable;
use reth::{
    providers::{ChainSpecProvider, StateProviderFactory},
    revm::{cancelled::CancelOnDrop, primitives::U256},
};
use reth_errors::RethError;
use reth_ethereum::{EthPrimitives, TransactionSigned as EthTransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockValidationError},
    execute::BlockBuilder,
};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_primitives_traits::{Header as RethHeader, Recovered};
use tracing::{debug, trace, warn};

use alethia_reth_block::{
    executor::{TxSelectionExecutor, is_zk_gas_limit_exceeded},
    tx_selection::{
        DEFAULT_DA_ZLIB_GUARD_BYTES, SelectionOutcome, TxSelectionConfig, ZkOverflowMemory,
        advance_selection_state, select_and_execute_pool_transactions,
    },
};
use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_consensus::validation::{AnchorValidationContext, validate_anchor_transaction};
use alethia_reth_primitives::transaction::is_allowed_tx_type;

/// Creates an error for when a transaction's effective tip cannot be calculated.
fn missing_tip_error(base_fee: u64) -> PayloadBuilderError {
    PayloadBuilderError::Internal(RethError::msg(format!(
        "effective tip missing for executed transaction (base_fee={base_fee})"
    )))
}

/// Outcome of executing the transaction phase for a payload build.
pub(super) enum ExecutionOutcome {
    /// Execution was cancelled before completion.
    Cancelled,
    /// Execution completed successfully with accumulated fees.
    Completed(U256),
}

/// Context for selecting pool transactions under the target fork's anchor policy.
pub(super) struct PoolExecutionContext<'a> {
    /// Prebuilt anchor transaction required before Etna and forbidden after activation.
    pub(super) anchor_tx: Option<&'a Recovered<EthTransactionSigned>>,
    /// Forced transaction of an Etna mixed build, executed at position 0 before pool selection
    /// (see [`execute_forced_transaction`]). `None` for a pool build, and always `None` before
    /// Etna, where attribute normalization rejects it.
    pub(super) forced_tx: Option<&'a Recovered<EthTransactionSigned>>,
    /// The parent block header.
    pub(super) parent_header: &'a RethHeader,
    /// Digest of the parent and the block environment this payload executes in, before the
    /// anchor, if any; see [`alethia_reth_block::tx_selection::selection_state_seed`].
    pub(super) block_state_seed: B256,
    /// Timestamp for the new block.
    pub(super) block_timestamp: u64,
    /// Payload identifier for logging.
    pub(super) payload_id: String,
    /// Base fee per gas for transaction selection.
    pub(super) base_fee: u64,
    /// Block gas limit.
    pub(super) gas_limit: u64,
    /// Zk gas overflows remembered across this builder's selection calls.
    pub(super) zk_overflows: &'a ZkOverflowMemory,
    /// Cap on the summed block-body length of the selected transactions, passed to selection as
    /// [`TxSelectionConfig::max_tx_bytes_per_list`]: `Some` on Etna builds, derived from
    /// `maxBlockBytes` and the header reserve, and `None` before Etna.
    pub(super) tx_bytes_budget: Option<u64>,
}

/// How a transaction from a given list fared against the block being built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ListedTxOutcome {
    /// The transaction executed and is committed to the block.
    Included {
        /// Gas the transaction used.
        gas_used: u64,
        /// Block-body size of the transaction in bytes, its [`Encodable::length`].
        tx_bytes: u64,
    },
    /// The transaction was left out for a transaction-level reason (unsupported type, larger
    /// than the remaining byte budget, invalid against the current state, or a gas limit above
    /// the block's remaining gas); it left no state and no receipt.
    LeftOut,
    /// The transaction exceeded the block zk gas limit and was not committed.
    ZkOverflow,
}

/// Executes one transaction from a given list and classifies the result.
///
/// A transaction whose block-body size exceeds `remaining_tx_bytes` is left out without being
/// executed; `None` disables the byte check and the bound is inclusive. Transaction-level failures
/// are outcomes, never errors: only a failure that is not about the transaction itself (for
/// example a state read error) is returned as `Err`, so it fails the build.
pub(super) fn execute_listed_transaction(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives>,
    tx: &Recovered<EthTransactionSigned>,
    remaining_tx_bytes: Option<u64>,
) -> Result<ListedTxOutcome, PayloadBuilderError> {
    if !is_allowed_tx_type(tx.inner()) {
        trace!(target: "payload_builder", ?tx, "leaving out unsupported transaction type");
        return Ok(ListedTxOutcome::LeftOut);
    }

    let tx_bytes = tx.inner().length() as u64;
    if let Some(remaining) = remaining_tx_bytes &&
        tx_bytes > remaining
    {
        trace!(target: "payload_builder", ?tx, tx_bytes, remaining, "leaving out transaction larger than the remaining byte budget");
        return Ok(ListedTxOutcome::LeftOut);
    }

    match builder.execute_transaction(tx.clone()) {
        Ok(gas_output) => {
            Ok(ListedTxOutcome::Included { gas_used: gas_output.tx_gas_used(), tx_bytes })
        }
        Err(err) if is_zk_gas_limit_exceeded(&err) => {
            trace!(target: "payload_builder", ?tx, "listed transaction exceeded the zk gas limit");
            Ok(ListedTxOutcome::ZkOverflow)
        }
        Err(BlockExecutionError::Validation(
            BlockValidationError::InvalidTx { .. } |
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
        )) => {
            trace!(target: "payload_builder", ?tx, "leaving out invalid transaction");
            Ok(ListedTxOutcome::LeftOut)
        }
        Err(err) => {
            warn!(target: "payload_builder", ?tx, %err, "fatal error executing transaction");
            Err(PayloadBuilderError::evm(err))
        }
    }
}

/// Executes a given transaction list in order.
///
/// Each transaction goes through [`execute_listed_transaction`] with what is left of
/// `tx_bytes_budget` (`None` disables the byte bound): a left-out transaction is skipped, the
/// first zk overflow ends the list, and a non-transaction error fails the build. Cancellation is
/// checked before every transaction.
pub(super) fn execute_provided_transactions(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives>,
    transactions: &[Recovered<EthTransactionSigned>],
    base_fee: u64,
    tx_bytes_budget: Option<u64>,
    cancel: &CancelOnDrop,
) -> Result<ExecutionOutcome, PayloadBuilderError> {
    let mut total_fees = U256::ZERO;
    let mut remaining_tx_bytes = tx_bytes_budget;

    for tx in transactions {
        if cancel.is_cancelled() {
            return Ok(ExecutionOutcome::Cancelled);
        }

        match execute_listed_transaction(builder, tx, remaining_tx_bytes)? {
            ListedTxOutcome::Included { gas_used, tx_bytes } => {
                let miner_fee = tx
                    .effective_tip_per_gas(base_fee)
                    .ok_or_else(|| missing_tip_error(base_fee))?;
                total_fees += U256::from(miner_fee) * U256::from(gas_used);
                remaining_tx_bytes = remaining_tx_bytes.map(|left| left.saturating_sub(tx_bytes));
            }
            ListedTxOutcome::LeftOut => {}
            ListedTxOutcome::ZkOverflow => {
                debug!(target: "payload_builder", ?tx, "stopping given-list payload after zk gas exhaustion");
                break;
            }
        }
    }

    Ok(ExecutionOutcome::Completed(total_fees))
}

/// Where pool selection starts: the execution state, the budgets left to pool transactions, and
/// the fees already collected.
#[derive(Debug, Clone, Copy)]
struct PoolSelectionStart {
    /// Digest of the execution state the first pool candidate runs against.
    state_seed: B256,
    /// Gas left to pool transactions, passed to selection as
    /// [`TxSelectionConfig::gas_limit_per_list`].
    gas_limit: u64,
    /// Transaction bytes left to pool transactions, passed to selection as
    /// [`TxSelectionConfig::max_tx_bytes_per_list`]; `None` disables the bound.
    tx_bytes_budget: Option<u64>,
    /// Tips collected from transactions executed before pool selection.
    fees: U256,
}

/// Executes the forced transaction of an Etna mixed build at position 0 and returns where pool
/// selection starts after it.
///
/// `full` holds the build's full budgets and the selection state before any transaction. The
/// forced transaction runs on the real pre-block state through [`execute_listed_transaction`] with
/// `full.tx_bytes_budget`, exactly as the given-list build of that transaction alone runs it, so
/// it is included exactly when that build includes it:
/// - Included: its tip is collected, the selection state advances past it, and its gas and bytes
///   come off the pool's budgets.
/// - Left out or zk overflow: it is not at position 0 and the pool keeps `full`. After a zk
///   overflow the exhaustion is cleared, so pool selection can run. A copy the pool also holds may
///   still be selected later in the block, as an ordinary pool transaction.
///
/// Remembered zk overflows apply to pool candidates only; the forced transaction runs first on the
/// real pre-block state, so this step takes no overflow memory.
fn execute_forced_transaction(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives, Executor: TxSelectionExecutor>,
    forced_tx: &Recovered<EthTransactionSigned>,
    base_fee: u64,
    full: PoolSelectionStart,
    payload_id: &str,
) -> Result<PoolSelectionStart, PayloadBuilderError> {
    let tx_hash = *forced_tx.tx_hash();
    match execute_listed_transaction(builder, forced_tx, full.tx_bytes_budget)? {
        ListedTxOutcome::Included { gas_used, tx_bytes } => {
            debug!(target: "payload_builder", id=%payload_id, ?tx_hash, gas_used, tx_bytes, "included the forced transaction at position 0");
            let tip = forced_tx
                .effective_tip_per_gas(base_fee)
                .ok_or_else(|| missing_tip_error(base_fee))?;
            Ok(PoolSelectionStart {
                state_seed: advance_selection_state(full.state_seed, tx_hash),
                gas_limit: full.gas_limit.saturating_sub(gas_used),
                tx_bytes_budget: full.tx_bytes_budget.map(|left| left.saturating_sub(tx_bytes)),
                fees: full.fees + U256::from(tip) * U256::from(gas_used),
            })
        }
        ListedTxOutcome::LeftOut => {
            debug!(target: "payload_builder", id=%payload_id, ?tx_hash, "left out the forced transaction");
            Ok(full)
        }
        ListedTxOutcome::ZkOverflow => {
            debug!(target: "payload_builder", id=%payload_id, ?tx_hash, "left out the forced transaction after it exceeded the zk gas limit");
            builder.executor_mut().clear_zk_gas_exhaustion();
            Ok(full)
        }
    }
}

/// Executes fork-aware pool transactions until exhaustion or cancellation.
///
/// Legacy payloads validate and execute their supplied anchor before pool selection. Etna payloads
/// require no anchor. An Etna mixed build first runs its forced transaction
/// ([`execute_forced_transaction`]); pool selection then gets the gas and transaction bytes it
/// left, or the full budgets ([`PoolExecutionContext::gas_limit`] and
/// [`PoolExecutionContext::tx_bytes_budget`]) when it was left out or there is none.
pub(super) fn execute_pool_transactions<Client, Pool>(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives, Executor: TxSelectionExecutor>,
    pool: &Pool,
    client: &Client,
    ctx: &PoolExecutionContext<'_>,
    cancel: &CancelOnDrop,
) -> Result<ExecutionOutcome, PayloadBuilderError>
where
    Client: StateProviderFactory
        + ChainSpecProvider<ChainSpec = TaikoChainSpec>
        + reth_provider::BlockReader,
    Pool: reth_transaction_pool::TransactionPool<
            Transaction: reth_transaction_pool::PoolTransaction<
                Consensus = reth_ethereum::TransactionSigned,
            >,
        >,
{
    let chain_spec = client.chain_spec();
    let full = PoolSelectionStart {
        state_seed: ctx.block_state_seed,
        gas_limit: ctx.gas_limit,
        tx_bytes_budget: ctx.tx_bytes_budget,
        fees: U256::ZERO,
    };
    let start = if chain_spec.is_etna_active(ctx.block_timestamp) {
        // Etna attribute normalization already rejects an anchor, so only a mixed build's forced
        // transaction runs before selection.
        debug!(target: "payload_builder", id=%ctx.payload_id, mixed = ctx.forced_tx.is_some(), "selecting anchorless Etna transactions");
        match ctx.forced_tx {
            Some(forced_tx) => {
                execute_forced_transaction(builder, forced_tx, ctx.base_fee, full, &ctx.payload_id)?
            }
            None => full,
        }
    } else {
        debug_assert!(ctx.forced_tx.is_none(), "forced transactions are Etna only");
        let anchor_tx = ctx.anchor_tx.ok_or_else(|| {
            warn!(target: "payload_builder", id=%ctx.payload_id, "missing prebuilt anchor transaction in new mode");
            PayloadBuilderError::MissingPayload
        })?;
        debug!(target: "payload_builder", id=%ctx.payload_id, "injecting anchor transaction");

        validate_anchor_transaction(
            anchor_tx.inner(),
            chain_spec.as_ref(),
            AnchorValidationContext {
                timestamp: ctx.block_timestamp,
                block_number: ctx.parent_header.number + 1,
                base_fee_per_gas: ctx.base_fee,
            },
        )
        .map_err(PayloadBuilderError::other)?;

        // The legacy anchor does not contribute to the pool DA-size calculation.
        match builder.execute_transaction(anchor_tx.clone()) {
            Ok(gas_output) => {
                debug!(target: "payload_builder", id=%ctx.payload_id, gas_used = gas_output.tx_gas_used(), "anchor transaction executed successfully");
            }
            Err(err) if is_zk_gas_limit_exceeded(&err) => {
                debug!(
                    target: "payload_builder",
                    id=%ctx.payload_id,
                    "stopping new-mode payload after anchor hit the zk gas limit"
                );
                return Err(PayloadBuilderError::evm(err));
            }
            Err(err) => {
                warn!(target: "payload_builder", id=%ctx.payload_id, %err, "failed to execute anchor transaction");
                return Err(PayloadBuilderError::evm(err));
            }
        }

        // The anchor committed above, so selection starts from the state after it. The legacy
        // anchor's gas is already reserved out of `ctx.gas_limit`.
        PoolSelectionStart {
            state_seed: advance_selection_state(ctx.block_state_seed, *anchor_tx.tx_hash()),
            ..full
        }
    };

    // Use the shared transaction selection logic for pool transactions. The gas limit is the gas
    // left after any forced transaction, so selection's own gas check, not the executor's block
    // gas error, rejects a pool transaction that no longer fits.
    let config = TxSelectionConfig {
        base_fee: ctx.base_fee,
        gas_limit_per_list: start.gas_limit,
        max_da_bytes_per_list: BYTES_PER_BLOB as u64,
        da_size_zlib_guard_bytes: DEFAULT_DA_ZLIB_GUARD_BYTES,
        max_lists: 1,
        min_tip: 0,
        locals: vec![],
        state_seed: start.state_seed,
        max_tx_bytes_per_list: start.tx_bytes_budget,
    };

    match select_and_execute_pool_transactions(builder, pool, &config, ctx.zk_overflows, || {
        cancel.is_cancelled()
    }) {
        Ok(SelectionOutcome::Cancelled) => Ok(ExecutionOutcome::Cancelled),
        Ok(SelectionOutcome::Completed(lists)) => {
            // Add the fees of the executed pool transactions to those collected before selection.
            let total_fees = match lists.first() {
                Some(list) => list.transactions.iter().try_fold(start.fees, |acc, etx| {
                    let tip = etx
                        .tx
                        .effective_tip_per_gas(ctx.base_fee)
                        .ok_or_else(|| missing_tip_error(ctx.base_fee))?;
                    Ok::<_, PayloadBuilderError>(acc + U256::from(tip) * U256::from(etx.gas_used))
                })?,
                None => start.fees,
            };
            Ok(ExecutionOutcome::Completed(total_fees))
        }
        Err(err) => Err(PayloadBuilderError::evm(err)),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::{Arc, LazyLock},
        task::{Context, Poll, Waker},
    };

    use super::*;
    use alloy_consensus::{
        SignableTransaction, Signed, TxEip1559, TxEip4844, TxLegacy,
        transaction::{SignerRecoverable, TxHashable},
    };
    use alloy_primitives::{Address, B256, Bytes, Signature};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use reth::revm::State;
    use reth_evm::{
        Evm, EvmFactory,
        block::{BlockExecutionError, BlockExecutor, CommitChanges},
        execute::{BlockBuilder, BlockBuilderOutcome, ExecutorTx},
    };
    use reth_evm_ethereum::RethReceiptBuilder;
    use reth_primitives_traits::Recovered;
    use reth_provider::test_utils::MockEthProvider;
    use reth_revm::{Database, state::Bytecode};
    use reth_storage_api::StateProvider;
    use reth_transaction_pool::{
        TransactionOrigin, TransactionPool,
        noop::NoopTransactionPool,
        test_utils::{TestPool, testing_pool},
    };

    use alethia_reth_block::{
        config::TaikoNextBlockEnvAttributes,
        executor::{TaikoBlockExecutor, ZkGasLimitExceeded},
        testutil::{
            BENCH_LIMIT_TARGET, BENCH_SUCCESS_TARGET, ExecutorBackedBuilder, db_with_contracts,
            etna_chain_spec, etna_evm_env, etna_execution_ctx, insert_contract, recovered_tx,
            recovered_tx_with_input, unzen_chain_spec, unzen_evm_env, unzen_execution_ctx,
        },
        tx_selection::selection_state_seed,
    };
    use alethia_reth_chainspec::spec::TaikoChainSpec;
    use alethia_reth_consensus::validation::{ANCHOR_V3_V4_GAS_LIMIT, ANCHOR_V4_SELECTOR};
    use alethia_reth_evm::{env::TaikoEvmEnv, factory::TaikoEvmFactory};
    use alethia_reth_primitives::addresses::TAIKO_GOLDEN_TOUCH_ADDRESS;

    const BENCH_SUCCESS_CALLER: Address = Address::with_last_byte(0x30);
    const BENCH_LIMIT_CALLER: Address = Address::with_last_byte(0x31);
    const BENCH_LATE_CALLER: Address = Address::with_last_byte(0x32);
    const GOLDEN_TOUCH_PRIVATE_KEY: &str =
        "0x92954368afd3caa1f3ce3ead0069c1af414054aefe1ef9aeacc1bf426222ce38";

    fn test_anchor_transaction(to: Address) -> Recovered<EthTransactionSigned> {
        let signer: PrivateKeySigner =
            GOLDEN_TOUCH_PRIVATE_KEY.parse().expect("golden touch private key should parse");
        let tx = TxEip1559 {
            chain_id: 167,
            nonce: 0,
            gas_limit: ANCHOR_V3_V4_GAS_LIMIT,
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: 0,
            to: to.into(),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::copy_from_slice(ANCHOR_V4_SELECTOR),
        };
        let sig_hash = tx.signature_hash();
        let signature = signer.sign_hash_sync(&sig_hash).expect("anchor tx should sign");
        let tx_hash = tx.tx_hash(&signature);
        let signed: EthTransactionSigned = Signed::new_unchecked(tx, signature, tx_hash).into();

        signed.try_into_recovered().expect("fixture anchor transaction should be recoverable")
    }

    fn test_client(chain_spec: TaikoChainSpec) -> MockEthProvider<EthPrimitives, TaikoChainSpec> {
        MockEthProvider::default().with_chain_spec(chain_spec)
    }

    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    /// Builder that fails the next transaction execution with `next_error`, then delegates.
    struct FirstTxErrorBuilder<E> {
        inner: ExecutorBackedBuilder<E>,
        next_error: Option<BlockExecutionError>,
    }

    impl<E> BlockBuilder for FirstTxErrorBuilder<E>
    where
        E: BlockExecutor<
                Transaction = reth_ethereum::TransactionSigned,
                Receipt = reth_ethereum::Receipt,
            >,
    {
        type Primitives = EthPrimitives;
        type Executor = E;

        fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
            self.inner.apply_pre_execution_changes()
        }

        fn execute_transaction_with_commit_condition(
            &mut self,
            tx: impl ExecutorTx<Self::Executor>,
            f: impl FnOnce(&<Self::Executor as BlockExecutor>::Result) -> CommitChanges,
        ) -> Result<Option<reth_evm::block::GasOutput>, BlockExecutionError> {
            if let Some(err) = self.next_error.take() {
                return Err(err);
            }

            self.inner.execute_transaction_with_commit_condition(tx, f)
        }

        fn finish(
            self,
            state_provider: impl StateProvider,
            state_root_precomputed: Option<(B256, reth_trie_common::updates::TrieUpdates)>,
        ) -> Result<BlockBuilderOutcome<Self::Primitives>, BlockExecutionError> {
            self.inner.finish(state_provider, state_root_precomputed)
        }

        fn executor_mut(&mut self) -> &mut Self::Executor {
            self.inner.executor_mut()
        }

        fn executor(&self) -> &Self::Executor {
            self.inner.executor()
        }

        fn into_executor(self) -> Self::Executor {
            self.inner.into_executor()
        }
    }

    #[test]
    fn missing_tip_error_includes_base_fee_context() {
        let err = missing_tip_error(1234);
        let message = err.to_string();
        assert!(
            message.contains("base_fee=1234"),
            "expected error message to include base fee context, got: {message}"
        );
    }

    #[test]
    fn missing_tip_error_mentions_effective_tip() {
        let err = missing_tip_error(1);
        assert!(err.to_string().contains("effective tip missing"));
    }

    #[test]
    fn execute_provided_transactions_stops_on_zk_gas_error() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[
                (BENCH_SUCCESS_CALLER, 0),
                (BENCH_LIMIT_CALLER, 0),
                (BENCH_LATE_CALLER, 0),
            ]))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let executor = TaikoBlockExecutor::new(
            evm,
            unzen_execution_ctx(),
            chain_spec,
            RethReceiptBuilder::default(),
        );
        let mut builder = ExecutorBackedBuilder { executor };
        let cancel = CancelOnDrop::default();
        let transactions = vec![
            recovered_tx(BENCH_SUCCESS_CALLER, BENCH_SUCCESS_TARGET, 0, 1),
            recovered_tx(BENCH_LIMIT_CALLER, BENCH_LIMIT_TARGET, 0, 1),
            recovered_tx(BENCH_LATE_CALLER, BENCH_SUCCESS_TARGET, 0, 1),
        ];

        let outcome = execute_provided_transactions(&mut builder, &transactions, 0, None, &cancel)
            .expect("zk gas exhaustion should stop cleanly");

        match outcome {
            ExecutionOutcome::Completed(total_fees) => {
                assert!(total_fees > U256::ZERO, "fees from the committed prefix should remain");
            }
            ExecutionOutcome::Cancelled => panic!("selection should not cancel"),
        }
        assert_eq!(builder.executor.receipts().len(), 1);
    }

    #[test]
    fn execute_pool_transactions_errors_when_legacy_anchor_hits_zk_gas_limit() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS), 0)]))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let executor = TaikoBlockExecutor::new(
            evm,
            unzen_execution_ctx(),
            chain_spec.clone(),
            RethReceiptBuilder::default(),
        );
        let mut builder = FirstTxErrorBuilder {
            inner: ExecutorBackedBuilder { executor },
            next_error: Some(BlockExecutionError::other(ZkGasLimitExceeded)),
        };
        let anchor_tx = test_anchor_transaction(BENCH_LIMIT_TARGET);
        let client = test_client((*chain_spec).clone());
        let pool = NoopTransactionPool::default();
        let cancel = CancelOnDrop::default();
        let parent_header = RethHeader { timestamp: 0, number: 0, ..Default::default() };
        let overflows = ZkOverflowMemory::default();

        let result = execute_pool_transactions(
            &mut builder,
            &pool,
            &client,
            &PoolExecutionContext {
                anchor_tx: Some(&anchor_tx),
                forced_tx: None,
                parent_header: &parent_header,
                block_state_seed: B256::ZERO,
                block_timestamp: 1,
                payload_id: "anchor-zk-gas".to_string(),
                base_fee: 0,
                gas_limit: 30_000_000,
                zk_overflows: &overflows,
                tx_bytes_budget: None,
            },
            &cancel,
        );

        let err = match result {
            Ok(ExecutionOutcome::Cancelled) => panic!("anchor execution should not cancel"),
            Ok(ExecutionOutcome::Completed(_)) => {
                panic!("anchor zk gas exhaustion should fail payload building")
            }
            Err(err) => err,
        };

        let PayloadBuilderError::EvmExecutionError(inner) = err else {
            panic!("anchor zk gas exhaustion should surface as an evm execution error");
        };
        let execution_err = inner
            .downcast_ref::<BlockExecutionError>()
            .expect("payload evm error should retain the block execution error");
        assert!(is_zk_gas_limit_exceeded(execution_err));
    }

    #[test]
    fn new_mode_keeps_selecting_after_a_pool_zk_gas_overflow() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[
                (Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS), 0),
                (BENCH_LIMIT_CALLER, 0),
                (BENCH_SUCCESS_CALLER, 0),
            ]))
            .with_bundle_update()
            .build();
        let mut evm_env = unzen_evm_env();
        evm_env.block_env.beneficiary = Address::with_last_byte(0x77);
        let beneficiary = evm_env.block_env.beneficiary;
        let evm = TaikoEvmFactory.create_evm(&mut state, evm_env);
        let executor = TaikoBlockExecutor::new(
            evm,
            unzen_execution_ctx(),
            chain_spec.clone(),
            RethReceiptBuilder::default(),
        );
        let mut builder = ExecutorBackedBuilder { executor };
        let anchor_tx = test_anchor_transaction(BENCH_SUCCESS_TARGET);
        let limit = recovered_tx(BENCH_LIMIT_CALLER, BENCH_LIMIT_TARGET, 0, 20);
        let success = recovered_tx(BENCH_SUCCESS_CALLER, BENCH_SUCCESS_TARGET, 0, 10);
        let [limit_hash, success_hash] = [&limit, &success].map(|tx| *tx.tx_hash());
        let pool = testing_pool();
        let rt = tokio::runtime::Builder::new_current_thread().build().expect("test runtime");
        for tx in [limit, success] {
            rt.block_on(pool.add_consensus_transaction(tx, TransactionOrigin::External))
                .expect("test tx should enter the pool");
        }
        let client = test_client((*chain_spec).clone());
        let cancel = CancelOnDrop::default();
        let parent_header = RethHeader { timestamp: 0, number: 0, ..Default::default() };
        let block_state_seed = selection_state_seed(
            B256::repeat_byte(0x66),
            &test_env_attributes(1, beneficiary),
            &[],
        );
        let gas_limit = 29_000_000;
        let overflows = ZkOverflowMemory::default();

        let outcome = execute_pool_transactions(
            &mut builder,
            &pool,
            &client,
            &PoolExecutionContext {
                anchor_tx: Some(&anchor_tx),
                forced_tx: None,
                parent_header: &parent_header,
                block_state_seed,
                block_timestamp: 1,
                payload_id: "pool-zk-gas-overflow".to_string(),
                base_fee: 0,
                gas_limit,
                zk_overflows: &overflows,
                tx_bytes_budget: None,
            },
            &cancel,
        )
        .expect("a pool zk gas overflow should not fail payload building");

        let ExecutionOutcome::Completed(total_fees) = outcome else {
            panic!("selection should not cancel")
        };
        // The anchor plus the success tx; the higher-tip limit tx overflowed and was left out.
        let receipts = builder.executor.receipts();
        assert_eq!(receipts.len(), 2);
        assert!(receipts[1].success);
        // Only the tip-10 success tx paid fees, so the second receipt is its receipt.
        let success_gas_used = receipts[1].cumulative_gas_used - receipts[0].cumulative_gas_used;
        assert_eq!(total_fees, U256::from(10 * success_gas_used));
        // The overflow is remembered at the state right after the anchor.
        assert_eq!(
            overflows.state(&limit_hash),
            Some(advance_selection_state(block_state_seed, *anchor_tx.tx_hash()))
        );
        assert_eq!(overflows.state(&success_hash), None);
    }

    /// Hashes in a loop while `TIMESTAMP == 1` and stops at once at any other timestamp.
    const BENCH_TIMESTAMP_TARGET: Address = Address::with_last_byte(0x40);

    /// Returns the block environment of a new-mode test payload at `timestamp`.
    fn test_env_attributes(timestamp: u64, beneficiary: Address) -> TaikoNextBlockEnvAttributes {
        TaikoNextBlockEnvAttributes {
            timestamp,
            suggested_fee_recipient: beneficiary,
            prev_randao: B256::ZERO,
            gas_limit: 30_000_000,
            extra_data: Bytes::new(),
            base_fee_per_gas: 0,
            parent_beacon_block_root: Some(B256::ZERO),
        }
    }

    /// Builds a new-mode payload at `timestamp` from `pool`, with the same parent and anchor every
    /// time, and returns how many transactions it committed, the anchor included.
    fn committed_in_new_mode_at(
        chain_spec: &Arc<TaikoChainSpec>,
        pool: &TestPool,
        anchor_tx: &Recovered<EthTransactionSigned>,
        timestamp: u64,
        overflows: &ZkOverflowMemory,
    ) -> usize {
        let mut db = db_with_contracts(&[
            (Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS), 0),
            (BENCH_LIMIT_CALLER, 0),
        ]);
        insert_contract(
            &mut db,
            BENCH_TIMESTAMP_TARGET,
            Bytecode::new_raw(Bytes::from_static(&alloy_primitives::hex!(
                "42600114600857005b6201000060002050600856"
            ))),
        );
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut evm_env = unzen_evm_env();
        evm_env.block_env.timestamp = U256::from(timestamp);
        let evm = TaikoEvmFactory.create_evm(&mut state, evm_env);
        let executor = TaikoBlockExecutor::new(
            evm,
            unzen_execution_ctx(),
            chain_spec.clone(),
            RethReceiptBuilder::default(),
        );
        let mut builder = ExecutorBackedBuilder { executor };
        let parent_header = RethHeader { timestamp: 0, number: 0, ..Default::default() };
        let env = test_env_attributes(timestamp, Address::ZERO);

        let outcome = execute_pool_transactions(
            &mut builder,
            pool,
            &test_client((**chain_spec).clone()),
            &PoolExecutionContext {
                anchor_tx: Some(anchor_tx),
                forced_tx: None,
                parent_header: &parent_header,
                block_state_seed: selection_state_seed(B256::repeat_byte(0x66), &env, &[]),
                block_timestamp: timestamp,
                payload_id: format!("timestamp-{timestamp}"),
                base_fee: 0,
                gas_limit: 29_000_000,
                zk_overflows: overflows,
                tx_bytes_budget: None,
            },
            &CancelOnDrop::default(),
        )
        .expect("new-mode selection should complete");
        assert!(matches!(outcome, ExecutionOutcome::Completed(_)));
        builder.executor.receipts().len()
    }

    #[test]
    fn new_mode_reruns_a_remembered_overflow_once_the_timestamp_changes() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let anchor_tx = test_anchor_transaction(BENCH_SUCCESS_TARGET);
        let switch = recovered_tx(BENCH_LIMIT_CALLER, BENCH_TIMESTAMP_TARGET, 0, 10);
        let switch_hash = *switch.tx_hash();
        let pool = testing_pool();
        let rt = tokio::runtime::Builder::new_current_thread().build().expect("test runtime");
        rt.block_on(pool.add_consensus_transaction(switch, TransactionOrigin::External))
            .expect("test tx should enter the pool");

        // At timestamp 1 the transaction hashes in a loop and overflows: only the anchor commits.
        let remembered = ZkOverflowMemory::default();
        assert_eq!(committed_in_new_mode_at(&chain_spec, &pool, &anchor_tx, 1, &remembered), 1);
        assert!(remembered.state(&switch_hash).is_some());

        // At timestamp 2 it stops at once and fits, with a fresh memory and with the one that
        // remembers its overflow at timestamp 1.
        let fresh = ZkOverflowMemory::default();
        assert_eq!(committed_in_new_mode_at(&chain_spec, &pool, &anchor_tx, 2, &fresh), 2);
        assert_eq!(committed_in_new_mode_at(&chain_spec, &pool, &anchor_tx, 2, &remembered), 2);
        assert_eq!(remembered.state(&switch_hash), None);
    }

    #[test]
    fn etna_pool_zk_gas_exhaustion_preserves_empty_and_completed_prefixes() {
        for prefix_len in [0, 1] {
            let caller = Address::with_last_byte(0x44);
            let spec = Arc::new(etna_chain_spec());
            let mut state = State::builder()
                .with_database(db_with_contracts(&[(caller, 0)]))
                .with_bundle_update()
                .build();
            let evm = TaikoEvmFactory.create_evm(&mut state, etna_evm_env());
            let executor = TaikoBlockExecutor::new(
                evm,
                etna_execution_ctx(B256::with_last_byte(1)),
                spec.clone(),
                RethReceiptBuilder::default(),
            );
            let mut builder = ExecutorBackedBuilder { executor };
            builder.apply_pre_execution_changes().unwrap();
            let pool = testing_pool();
            // A single nonce chain fixes ordering: optional success, a real zk overflow, then
            // the same sender's next nonce, which must stay unexecuted. The real executor reports
            // the overflow as an Etna validation error.
            for nonce in 0..prefix_len + 2 {
                let target =
                    if nonce == prefix_len { BENCH_LIMIT_TARGET } else { BENCH_SUCCESS_TARGET };
                let tx = Recovered::new_unchecked(
                    Signed::new_unhashed(
                        TxLegacy {
                            chain_id: Some(167),
                            nonce,
                            gas_price: 10,
                            gas_limit: 5_000_000,
                            to: target.into(),
                            ..Default::default()
                        },
                        Signature::new(U256::from(1), U256::from(2), false),
                    )
                    .into(),
                    caller,
                );
                block_on_ready(pool.add_consensus_transaction(tx, TransactionOrigin::External))
                    .expect("transaction should enter the pool");
            }
            let outcome = execute_pool_transactions(
                &mut builder,
                &pool,
                &test_client((*spec).clone()),
                &PoolExecutionContext {
                    anchor_tx: None,
                    forced_tx: None,
                    parent_header: &RethHeader { timestamp: 0, number: 0, ..Default::default() },
                    block_state_seed: B256::ZERO,
                    block_timestamp: 1,
                    payload_id: format!("etna-zk-limit-{prefix_len}"),
                    base_fee: 0,
                    gas_limit: 30_000_000,
                    zk_overflows: &ZkOverflowMemory::default(),
                    tx_bytes_budget: None,
                },
                &CancelOnDrop::default(),
            )
            .expect("a pool zk overflow should not fail the build");
            let ExecutionOutcome::Completed(_) = outcome else {
                panic!("selection should not cancel")
            };
            // A zk overflow leaves out that sender's later nonces; selection does not fail.
            assert_eq!(builder.executor.receipts().len(), prefix_len as usize);
        }
    }

    /// Returns an Etna builder over the standard test contracts and `accounts`, with the
    /// pre-execution changes applied.
    fn etna_builder(
        accounts: &[(Address, u64)],
    ) -> ExecutorBackedBuilder<
        impl BlockExecutor<Transaction = EthTransactionSigned, Receipt = reth_ethereum::Receipt>
        + TxSelectionExecutor
        + use<>,
    > {
        etna_builder_with_env(accounts, etna_evm_env())
    }

    /// Returns an Etna builder in `env` over the standard test contracts and `accounts`, with the
    /// pre-execution changes applied.
    fn etna_builder_with_env(
        accounts: &[(Address, u64)],
        env: TaikoEvmEnv,
    ) -> ExecutorBackedBuilder<
        impl BlockExecutor<Transaction = EthTransactionSigned, Receipt = reth_ethereum::Receipt>
        + TxSelectionExecutor
        + use<>,
    > {
        let state = State::builder()
            .with_database(db_with_contracts(accounts))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(state, env);
        let executor = TaikoBlockExecutor::new(
            evm,
            etna_execution_ctx(B256::with_last_byte(1)),
            Arc::new(etna_chain_spec()),
            RethReceiptBuilder::default(),
        );
        let mut builder = ExecutorBackedBuilder { executor };
        builder.apply_pre_execution_changes().expect("Etna pre-execution changes should apply");
        builder
    }

    /// Returns the nonce `address` has in the builder's state.
    fn nonce_of(builder: &mut ExecutorBackedBuilder<impl BlockExecutor>, address: Address) -> u64 {
        builder
            .executor
            .evm_mut()
            .db_mut()
            .basic(address)
            .expect("test state reads should not fail")
            .map_or(0, |account| account.nonce)
    }

    /// Builds a nonce-0 legacy transaction from `caller` to the success contract with `gas_limit`.
    fn legacy_tx_with_gas_limit(
        caller: Address,
        gas_limit: u64,
    ) -> Recovered<EthTransactionSigned> {
        Recovered::new_unchecked(
            Signed::new_unhashed(
                TxLegacy {
                    chain_id: Some(167),
                    nonce: 0,
                    gas_price: 1,
                    gas_limit,
                    to: BENCH_SUCCESS_TARGET.into(),
                    ..Default::default()
                },
                Signature::new(U256::from(1), U256::from(2), false),
            )
            .into(),
            caller,
        )
    }

    #[test]
    fn execute_listed_transaction_classifies_each_outcome() {
        let funded = Address::with_last_byte(0x50);
        let unfunded = Address::with_last_byte(0x51);
        let success = recovered_tx(funded, BENCH_SUCCESS_TARGET, 0, 1);
        let success_len = success.inner().length() as u64;

        // An executed transaction reports its gas and its block-body size; the byte bound is
        // inclusive.
        for remaining in [None, Some(success_len)] {
            let mut builder = etna_builder(&[(funded, 0)]);
            let outcome = execute_listed_transaction(&mut builder, &success, remaining)
                .expect("a valid transaction should execute");
            let ListedTxOutcome::Included { gas_used, tx_bytes } = outcome else {
                panic!("a valid transaction should be included, got {outcome:?}")
            };
            assert!(gas_used > 0);
            assert_eq!(tx_bytes, success_len);
            assert_eq!(builder.executor.receipts().len(), 1);
            assert_eq!(nonce_of(&mut builder, funded), 1);
        }

        let blob = Recovered::new_unchecked(
            Signed::new_unhashed(
                TxEip4844 {
                    chain_id: 167,
                    gas_limit: 5_000_000,
                    max_fee_per_gas: 1,
                    to: BENCH_SUCCESS_TARGET,
                    blob_versioned_hashes: vec![B256::with_last_byte(1)],
                    max_fee_per_blob_gas: 1,
                    ..Default::default()
                },
                Signature::new(U256::from(1), U256::from(2), false),
            )
            .into(),
            funded,
        );
        let cases = [
            ("nonce ahead of the account", recovered_tx(funded, BENCH_SUCCESS_TARGET, 1, 1), None),
            ("unfunded caller", recovered_tx(unfunded, BENCH_SUCCESS_TARGET, 0, 1), None),
            ("gas limit below intrinsic gas", legacy_tx_with_gas_limit(funded, 20_000), None),
            ("gas limit above the block gas", legacy_tx_with_gas_limit(funded, 30_000_001), None),
            ("unsupported blob type", blob, None),
            ("larger than the remaining bytes", success.clone(), Some(success_len - 1)),
        ];
        for (case, tx, remaining) in cases {
            let mut builder = etna_builder(&[(funded, 0)]);
            let outcome = execute_listed_transaction(&mut builder, &tx, remaining)
                .unwrap_or_else(|err| panic!("{case}: a transaction-level failure errored: {err}"));
            assert_eq!(outcome, ListedTxOutcome::LeftOut, "{case}");
            assert!(builder.executor.receipts().is_empty(), "{case}: left a receipt");
            assert_eq!(nonce_of(&mut builder, funded), 0, "{case}: left state behind");
        }

        let mut builder = etna_builder(&[(funded, 0)]);
        let limit = recovered_tx(funded, BENCH_LIMIT_TARGET, 0, 1);
        let outcome = execute_listed_transaction(&mut builder, &limit, None)
            .expect("a zk overflow is a transaction-level outcome");
        assert_eq!(outcome, ListedTxOutcome::ZkOverflow);
        assert!(builder.executor.receipts().is_empty());
        assert_eq!(nonce_of(&mut builder, funded), 0);
    }

    #[test]
    fn execute_listed_transaction_keeps_non_transaction_errors() {
        let caller = Address::with_last_byte(0x52);
        let tx = recovered_tx(caller, BENCH_SUCCESS_TARGET, 0, 1);
        let builder_failing_with = |next_error| FirstTxErrorBuilder {
            inner: etna_builder(&[(caller, 0)]),
            next_error: Some(next_error),
        };

        // The pre-Etna wrapping of a zk overflow is still an overflow.
        let mut builder = builder_failing_with(BlockExecutionError::other(ZkGasLimitExceeded));
        let outcome = execute_listed_transaction(&mut builder, &tx, None)
            .expect("a legacy-wrapped zk overflow is a transaction-level outcome");
        assert_eq!(outcome, ListedTxOutcome::ZkOverflow);

        // An infrastructure failure is not turned into a left-out transaction.
        let mut builder = builder_failing_with(BlockExecutionError::msg("state read failed"));
        let err = execute_listed_transaction(&mut builder, &tx, None)
            .expect_err("an infrastructure failure must fail the build");
        assert!(err.to_string().contains("state read failed"), "{err}");
    }

    #[test]
    fn etna_given_list_skips_transactions_beyond_the_byte_budget() {
        let [a_caller, b_caller, c_caller] = [0x53, 0x54, 0x55].map(Address::with_last_byte);
        let large_input = Bytes::from(vec![0_u8; 2_961]);
        let a = recovered_tx_with_input(a_caller, BENCH_SUCCESS_TARGET, 0, 1, large_input.clone());
        let b = recovered_tx_with_input(b_caller, BENCH_SUCCESS_TARGET, 0, 1, large_input);
        let c = recovered_tx(c_caller, BENCH_SUCCESS_TARGET, 0, 1);
        let [a_len, b_len, c_len] = [&a, &b, &c].map(|tx| tx.inner().length() as u64);
        assert!(a_len == 3_000 && b_len == 3_000 && c_len <= 100, "{a_len} {b_len} {c_len}");
        let mut builder = etna_builder(&[(a_caller, 0), (b_caller, 0), (c_caller, 0)]);

        let outcome = execute_provided_transactions(
            &mut builder,
            &[a, b, c],
            0,
            Some(a_len + c_len),
            &CancelOnDrop::default(),
        )
        .expect("a transaction beyond the byte budget should be left out, not fail the build");

        let ExecutionOutcome::Completed(total_fees) = outcome else {
            panic!("the given-list build should not cancel")
        };
        // a then c, in list order: a's calldata makes its receipt the costlier one.
        let receipts = builder.executor.receipts();
        assert_eq!(receipts.len(), 2);
        let a_gas = receipts[0].cumulative_gas_used;
        let c_gas = receipts[1].cumulative_gas_used - a_gas;
        assert!(a_gas > c_gas, "{a_gas} {c_gas}");
        assert_eq!(total_fees, U256::from(a_gas + c_gas));
        assert_eq!(nonce_of(&mut builder, b_caller), 0);
    }

    #[test]
    fn etna_pool_selection_respects_the_tx_bytes_budget() {
        let [a_caller, b_caller, c_caller] = [0x56, 0x57, 0x58].map(Address::with_last_byte);
        let tx = |caller, tip, input_len| {
            recovered_tx_with_input(
                caller,
                BENCH_SUCCESS_TARGET,
                0,
                tip,
                Bytes::from(vec![0_u8; input_len]),
            )
        };
        let txs = [tx(a_caller, 30, 3_000), tx(b_caller, 20, 3_000), tx(c_caller, 10, 100)];
        let pool = testing_pool();
        for tx in txs.clone() {
            block_on_ready(pool.add_consensus_transaction(tx, TransactionOrigin::External))
                .expect("transaction should enter the pool");
        }
        // The test pool re-signs what it stores, so the lengths are taken from the copies it hands
        // to selection.
        let [a_len, b_len, c_len] = txs.each_ref().map(|tx| {
            let pooled = pool.get(tx.tx_hash()).expect("test tx should be in the pool");
            pooled.to_consensus().inner().length() as u64
        });
        assert!(a_len == b_len && a_len > 3_000 && c_len < 300, "{a_len} {b_len} {c_len}");
        let accounts = [(a_caller, 0), (b_caller, 0), (c_caller, 0)];

        let select = |tx_bytes_budget| {
            let mut builder = etna_builder(&accounts);
            let outcome = execute_pool_transactions(
                &mut builder,
                &pool,
                &test_client(etna_chain_spec()),
                &PoolExecutionContext {
                    anchor_tx: None,
                    forced_tx: None,
                    parent_header: &RethHeader { timestamp: 0, number: 0, ..Default::default() },
                    block_state_seed: B256::ZERO,
                    block_timestamp: 1,
                    payload_id: "etna-tx-bytes-budget".to_string(),
                    base_fee: 0,
                    gas_limit: 30_000_000,
                    zk_overflows: &ZkOverflowMemory::default(),
                    tx_bytes_budget,
                },
                &CancelOnDrop::default(),
            )
            .expect("a transaction beyond the byte budget should be left out, not fail the build");
            let ExecutionOutcome::Completed(total_fees) = outcome else {
                panic!("pool selection should not cancel")
            };
            let receipts = builder.executor.receipts().to_vec();
            let b_nonce = nonce_of(&mut builder, b_caller);
            (receipts, total_fees, b_nonce)
        };

        // Without a budget all three fit.
        let (receipts, _, b_nonce) = select(None);
        assert_eq!(receipts.len(), 3);
        assert_eq!(b_nonce, 1);

        // `b` does not fit after `a`, so it is left out and `c`, which fits, is still selected.
        let (receipts, total_fees, b_nonce) = select(Some(a_len + c_len));
        assert_eq!(receipts.len(), 2);
        let a_gas = receipts[0].cumulative_gas_used;
        let c_gas = receipts[1].cumulative_gas_used - a_gas;
        assert_eq!(total_fees, U256::from(30 * a_gas + 10 * c_gas));
        assert_eq!(b_nonce, 0);
    }

    /// Parent header of the Etna pool and mixed-build tests; Etna selection does not read it.
    static ETNA_PARENT: LazyLock<RethHeader> = LazyLock::new(RethHeader::default);

    /// Transaction byte budget of the Etna build tests that do not exercise the byte bound.
    const ETNA_TX_BYTES_BUDGET: Option<u64> = Some(100_000);

    /// Returns the context of an Etna build: a mixed build of `forced_tx` when it is `Some` and a
    /// pool build otherwise, with a zero base fee, the 30M gas limit of the Etna test environment,
    /// [`ETNA_TX_BYTES_BUDGET`], `zk_overflows` and `block_state_seed`.
    fn etna_build_ctx<'a>(
        forced_tx: Option<&'a Recovered<EthTransactionSigned>>,
        zk_overflows: &'a ZkOverflowMemory,
        block_state_seed: B256,
    ) -> PoolExecutionContext<'a> {
        PoolExecutionContext {
            anchor_tx: None,
            forced_tx,
            parent_header: &ETNA_PARENT,
            block_state_seed,
            block_timestamp: 1,
            payload_id: "etna-build".to_string(),
            base_fee: 0,
            gas_limit: 30_000_000,
            zk_overflows,
            tx_bytes_budget: ETNA_TX_BYTES_BUDGET,
        }
    }

    /// Runs the Etna build `ctx` describes over `pool` on `builder` and returns the fees it
    /// collected; the build must complete.
    fn run_etna_build(
        builder: &mut ExecutorBackedBuilder<
            impl BlockExecutor<Transaction = EthTransactionSigned, Receipt = reth_ethereum::Receipt>
            + TxSelectionExecutor,
        >,
        pool: &TestPool,
        ctx: &PoolExecutionContext<'_>,
    ) -> U256 {
        let outcome = execute_pool_transactions(
            builder,
            pool,
            &test_client(etna_chain_spec()),
            ctx,
            &CancelOnDrop::default(),
        )
        .expect("the Etna build should complete");
        let ExecutionOutcome::Completed(total_fees) = outcome else {
            panic!("the Etna build should not cancel")
        };
        total_fees
    }

    /// Returns a test pool holding `txs` as external transactions.
    fn pool_with(txs: impl IntoIterator<Item = Recovered<EthTransactionSigned>>) -> TestPool {
        let pool = testing_pool();
        for tx in txs {
            block_on_ready(pool.add_consensus_transaction(tx, TransactionOrigin::External))
                .expect("transaction should enter the pool");
        }
        pool
    }

    /// Returns the block-body length of the copy of `tx` that `pool` hands to selection; the test
    /// pool re-signs what it stores.
    fn pooled_len(pool: &TestPool, tx: &Recovered<EthTransactionSigned>) -> u64 {
        let pooled = pool.get(tx.tx_hash()).expect("test tx should be in the pool");
        pooled.to_consensus().inner().length() as u64
    }

    #[test]
    fn etna_pool_keeps_selecting_other_senders_after_a_zk_gas_overflow() {
        let [l_caller, s_caller] = [0x60, 0x61].map(Address::with_last_byte);
        // `l` pays the higher tip, so selection tries it first.
        let l = recovered_tx(l_caller, BENCH_LIMIT_TARGET, 0, 20);
        let s = recovered_tx(s_caller, BENCH_SUCCESS_TARGET, 0, 10);
        let l_hash = *l.tx_hash();
        let pool = pool_with([l, s]);
        let block_state_seed = B256::repeat_byte(0x66);
        let overflows = ZkOverflowMemory::default();
        let mut builder = etna_builder(&[(l_caller, 0), (s_caller, 0)]);

        let total_fees = run_etna_build(
            &mut builder,
            &pool,
            &etna_build_ctx(None, &overflows, block_state_seed),
        );

        // `l` overflowed and was left out; `s`, from another sender, was still selected.
        let receipts = builder.executor.receipts().to_vec();
        assert_eq!(receipts.len(), 1);
        assert_eq!(total_fees, U256::from(10 * receipts[0].cumulative_gas_used));
        assert_eq!(nonce_of(&mut builder, l_caller), 0);
        assert_eq!(nonce_of(&mut builder, s_caller), 1);
        assert_eq!(overflows.state(&l_hash), Some(block_state_seed));
    }

    #[test]
    fn mixed_build_includes_the_forced_transaction_exactly_when_its_singleton_build_does() {
        let [fi_caller, unfunded, p_caller] = [0x62, 0x63, 0x64].map(Address::with_last_byte);
        let accounts = [(fi_caller, 0), (p_caller, 0)];
        // The forced transaction's calldata makes its gas differ from `p`'s.
        let forced = |caller, to, nonce| {
            recovered_tx_with_input(caller, to, nonce, 1, Bytes::from_static(&[1; 32]))
        };
        let cases = [
            ("fits", 0, forced(fi_caller, BENCH_SUCCESS_TARGET, 0), true),
            ("nonce gap", 0, forced(fi_caller, BENCH_SUCCESS_TARGET, 1), false),
            ("unfunded sender", 0, forced(unfunded, BENCH_SUCCESS_TARGET, 0), false),
            ("fee below the base fee", 10, forced(fi_caller, BENCH_SUCCESS_TARGET, 0), false),
            ("zk overflow", 0, forced(fi_caller, BENCH_LIMIT_TARGET, 0), false),
        ];
        for (case, base_fee, fi, expect_included) in cases {
            let mut env = etna_evm_env();
            env.block_env.basefee = base_fee;
            // `p` pays a tip of 10 above the base fee.
            let pool = pool_with([recovered_tx(p_caller, BENCH_SUCCESS_TARGET, 0, base_fee + 10)]);

            let mut mixed = etna_builder_with_env(&accounts, env.clone());
            let overflows = ZkOverflowMemory::default();
            let mut ctx = etna_build_ctx(Some(&fi), &overflows, B256::ZERO);
            ctx.base_fee = base_fee;
            run_etna_build(&mut mixed, &pool, &ctx);

            // The given-list build of the forced transaction alone, on the same pre-state.
            let mut singleton = etna_builder_with_env(&accounts, env);
            execute_provided_transactions(
                &mut singleton,
                std::slice::from_ref(&fi),
                base_fee,
                ETNA_TX_BYTES_BUDGET,
                &CancelOnDrop::default(),
            )
            .unwrap_or_else(|err| panic!("{case}: the singleton build errored: {err}"));
            let singleton_receipts = singleton.executor.receipts().to_vec();
            assert_eq!(singleton_receipts.len() == 1, expect_included, "{case}: singleton");

            let included = nonce_of(&mut mixed, fi.signer()) == 1;
            assert_eq!(included, singleton_receipts.len() == 1, "{case}: mixed vs singleton");
            assert_eq!(nonce_of(&mut mixed, p_caller), 1, "{case}: pool transaction");
            let mixed_receipts = mixed.executor.receipts();
            assert_eq!(mixed_receipts.len(), 1 + usize::from(included), "{case}");
            if included {
                // Receipt 0 is the forced transaction's, on the same pre-state: its gas matches the
                // singleton build's and differs from `p`'s.
                let fi_gas = singleton_receipts[0].cumulative_gas_used;
                let p_gas =
                    mixed_receipts[1].cumulative_gas_used - mixed_receipts[0].cumulative_gas_used;
                assert_eq!(mixed_receipts[0].cumulative_gas_used, fi_gas, "{case}");
                assert_ne!(fi_gas, p_gas, "{case}");
            }
        }
    }

    #[test]
    fn mixed_build_never_skips_the_forced_transaction_for_a_remembered_overflow() {
        let fi_caller = Address::with_last_byte(0x65);
        // The test pool takes no fee cap below 7 wei.
        let fi = recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 0, 10);
        let fi_hash = *fi.tx_hash();
        let block_state_seed = B256::repeat_byte(0x66);
        // The forced transaction is remembered as overflowing at the very state it now runs on.
        let overflows = ZkOverflowMemory::default();
        overflows.record(fi_hash, block_state_seed);
        // The pool holds it too, so pool selection keeps the memory entry.
        let pool = pool_with([fi.clone()]);
        let mut builder = etna_builder(&[(fi_caller, 0)]);

        run_etna_build(
            &mut builder,
            &pool,
            &etna_build_ctx(Some(&fi), &overflows, block_state_seed),
        );

        // It runs and fits; its pooled copy is not included again.
        assert_eq!(builder.executor.receipts().len(), 1);
        assert_eq!(nonce_of(&mut builder, fi_caller), 1);
        assert_eq!(overflows.state(&fi_hash), Some(block_state_seed));
    }

    #[test]
    fn mixed_build_leaves_out_a_failing_forced_transaction_and_still_fills_from_the_pool() {
        let [fi_caller, l_caller, p_caller] = [0x66, 0x67, 0x73].map(Address::with_last_byte);
        // `l` pays the higher tip, so it is the first pool candidate.
        let l = recovered_tx(l_caller, BENCH_LIMIT_TARGET, 0, 20);
        let l_hash = *l.tx_hash();
        let pool = pool_with([l, recovered_tx(p_caller, BENCH_SUCCESS_TARGET, 0, 10)]);
        let block_state_seed = B256::repeat_byte(0x66);
        let cases = [
            ("zk overflow", recovered_tx(fi_caller, BENCH_LIMIT_TARGET, 0, 1)),
            ("nonce gap", recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 1, 1)),
        ];
        for (case, fi) in cases {
            let overflows = ZkOverflowMemory::default();
            let mut builder = etna_builder(&[(fi_caller, 0), (l_caller, 0), (p_caller, 0)]);

            // The step that runs the forced transaction never receives the overflow memory.
            let total_fees = run_etna_build(
                &mut builder,
                &pool,
                &etna_build_ctx(Some(&fi), &overflows, block_state_seed),
            );

            // The forced transaction is left out, and any zk gas exhaustion it caused is cleared,
            // so `p` still runs and is the block's only transaction.
            let receipts = builder.executor.receipts().to_vec();
            assert_eq!(receipts.len(), 1, "{case}");
            assert_eq!(total_fees, U256::from(10 * receipts[0].cumulative_gas_used), "{case}");
            assert_eq!(nonce_of(&mut builder, fi_caller), 0, "{case}");
            assert_eq!(nonce_of(&mut builder, p_caller), 1, "{case}");
            // Pool selection starts from the state before any transaction: `l` overflowed there.
            assert_eq!(overflows.state(&l_hash), Some(block_state_seed), "{case}");
        }
    }

    #[test]
    fn mixed_build_may_select_a_pooled_copy_of_a_left_out_forced_transaction_later() {
        let fi_caller = Address::with_last_byte(0x74);
        // The forced transaction is one nonce ahead of its sender, so it is left out of position
        // 0. The pool holds a copy of it and the sender's transaction at the current nonce, and
        // the copy's calldata makes its gas differ from that transaction's.
        let current = recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 0, 10);
        let fi = recovered_tx_with_input(
            fi_caller,
            BENCH_SUCCESS_TARGET,
            1,
            10,
            Bytes::from_static(&[1; 32]),
        );
        let pool = pool_with([current, fi.clone()]);
        let overflows = ZkOverflowMemory::default();
        let mut builder = etna_builder(&[(fi_caller, 0)]);

        run_etna_build(&mut builder, &pool, &etna_build_ctx(Some(&fi), &overflows, B256::ZERO));

        // The pool runs the current nonce at position 0 and then the copy at position 1, as an
        // ordinary pool transaction.
        let receipts = builder.executor.receipts();
        assert_eq!(receipts.len(), 2);
        let first_gas = receipts[0].cumulative_gas_used;
        assert!(first_gas < receipts[1].cumulative_gas_used - first_gas, "{receipts:?}");
        assert_eq!(nonce_of(&mut builder, fi_caller), 2);
    }

    #[test]
    fn mixed_build_seeds_pool_selection_after_the_forced_transaction() {
        let [fi_caller, l_caller, p_caller] = [0x68, 0x69, 0x6a].map(Address::with_last_byte);
        let fi = recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 0, 1);
        // `l` pays the higher tip, so it is the first pool candidate after the forced transaction.
        let l = recovered_tx(l_caller, BENCH_LIMIT_TARGET, 0, 20);
        let p = recovered_tx(p_caller, BENCH_SUCCESS_TARGET, 0, 10);
        let l_hash = *l.tx_hash();
        let pool = pool_with([l, p]);
        let block_state_seed = B256::repeat_byte(0x66);
        let overflows = ZkOverflowMemory::default();
        let mut builder = etna_builder(&[(fi_caller, 0), (l_caller, 0), (p_caller, 0)]);

        run_etna_build(
            &mut builder,
            &pool,
            &etna_build_ctx(Some(&fi), &overflows, block_state_seed),
        );

        // The forced transaction and `p`; `l` overflowed at the state right after the forced
        // transaction.
        assert_eq!(builder.executor.receipts().len(), 2);
        assert_eq!(
            overflows.state(&l_hash),
            Some(advance_selection_state(block_state_seed, *fi.tx_hash()))
        );
    }

    #[test]
    fn mixed_build_gives_pool_selection_only_the_gas_left_after_the_forced_transaction() {
        let [fi_caller, p_caller] = [0x6b, 0x6c].map(Address::with_last_byte);
        // Both transactions have a 5M gas limit, so `p` cannot follow the forced transaction in a
        // 5.01M gas block.
        let fi = recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 0, 1);
        let pool = pool_with([recovered_tx(p_caller, BENCH_SUCCESS_TARGET, 0, 10)]);
        let mut env = etna_evm_env();
        env.block_env.gas_limit = 5_010_000;
        let mut builder = etna_builder_with_env(&[(fi_caller, 0), (p_caller, 0)], env);
        let overflows = ZkOverflowMemory::default();
        let mut ctx = etna_build_ctx(Some(&fi), &overflows, B256::ZERO);
        ctx.gas_limit = 5_010_000;

        // Selection's own gas check leaves `p` out; the executor's block gas error would be fatal.
        run_etna_build(&mut builder, &pool, &ctx);

        assert_eq!(builder.executor.receipts().len(), 1);
        assert_eq!(nonce_of(&mut builder, fi_caller), 1);
        assert_eq!(nonce_of(&mut builder, p_caller), 0);
    }

    #[test]
    fn mixed_build_gives_pool_selection_only_the_bytes_left_after_the_forced_transaction() {
        let [fi_caller, p_caller] = [0x6d, 0x6e].map(Address::with_last_byte);
        let fi = recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 0, 1);
        let p = recovered_tx(p_caller, BENCH_SUCCESS_TARGET, 0, 10);
        let pool = pool_with([p.clone()]);
        let fi_len = fi.inner().length() as u64;
        let p_len = pooled_len(&pool, &p);

        // The bound is inclusive: both fit in exactly their summed length, and `p` does not fit
        // in one byte less.
        for (budget, p_included) in [(fi_len + p_len, true), (fi_len + p_len - 1, false)] {
            let mut builder = etna_builder(&[(fi_caller, 0), (p_caller, 0)]);
            let overflows = ZkOverflowMemory::default();
            let mut ctx = etna_build_ctx(Some(&fi), &overflows, B256::ZERO);
            ctx.tx_bytes_budget = Some(budget);

            run_etna_build(&mut builder, &pool, &ctx);

            assert_eq!(nonce_of(&mut builder, fi_caller), 1, "budget {budget}");
            assert_eq!(nonce_of(&mut builder, p_caller), u64::from(p_included), "budget {budget}");
        }
    }

    #[test]
    fn mixed_build_gives_pool_selection_the_full_budgets_when_the_forced_transaction_is_left_out() {
        let [fi_caller, p_caller] = [0x6f, 0x70].map(Address::with_last_byte);
        let p = recovered_tx(p_caller, BENCH_SUCCESS_TARGET, 0, 10);
        let pool = pool_with([p.clone()]);
        let p_len = pooled_len(&pool, &p);
        let large_input = Bytes::from(vec![0_u8; p_len as usize]);
        let cases = [
            ("nonce gap", recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 1, 1)),
            ("zk overflow", recovered_tx(fi_caller, BENCH_LIMIT_TARGET, 0, 1)),
            (
                "larger than the byte budget",
                recovered_tx_with_input(fi_caller, BENCH_SUCCESS_TARGET, 0, 1, large_input),
            ),
        ];
        // The first two fit the byte budget, so they are left out only after running.
        assert!(cases[..2].iter().all(|(_, fi)| fi.inner().length() as u64 <= p_len));
        for (case, fi) in cases {
            // Every transaction has a 5M gas limit and `p` alone fills the byte budget, so `p`
            // fits only if the forced transaction leaves the gas and the bytes in full.
            let mut env = etna_evm_env();
            env.block_env.gas_limit = 5_010_000;
            let mut builder = etna_builder_with_env(&[(fi_caller, 0), (p_caller, 0)], env);
            let overflows = ZkOverflowMemory::default();
            let mut ctx = etna_build_ctx(Some(&fi), &overflows, B256::ZERO);
            ctx.gas_limit = 5_010_000;
            ctx.tx_bytes_budget = Some(p_len);

            run_etna_build(&mut builder, &pool, &ctx);

            assert_eq!(builder.executor.receipts().len(), 1, "{case}");
            assert_eq!(nonce_of(&mut builder, fi_caller), 0, "{case}");
            assert_eq!(nonce_of(&mut builder, p_caller), 1, "{case}");
        }
    }

    #[test]
    fn mixed_build_with_an_empty_pool_builds_only_the_forced_transaction() {
        let fi_caller = Address::with_last_byte(0x71);
        let fi = recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 0, 7);
        let overflows = ZkOverflowMemory::default();
        let mut builder = etna_builder(&[(fi_caller, 0)]);

        let total_fees = run_etna_build(
            &mut builder,
            &testing_pool(),
            &etna_build_ctx(Some(&fi), &overflows, B256::ZERO),
        );

        let receipts = builder.executor.receipts();
        assert_eq!(receipts.len(), 1);
        assert!(receipts[0].success);
        assert_eq!(total_fees, U256::from(7 * receipts[0].cumulative_gas_used));
    }

    #[test]
    fn mixed_build_keeps_a_pooled_copy_of_the_forced_transaction_out_and_its_next_nonce_in() {
        let fi_caller = Address::with_last_byte(0x72);
        let fi = recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 0, 10);
        let next = recovered_tx(fi_caller, BENCH_SUCCESS_TARGET, 1, 10);
        let pool = pool_with([fi.clone(), next]);
        let overflows = ZkOverflowMemory::default();
        let mut builder = etna_builder(&[(fi_caller, 0)]);

        run_etna_build(&mut builder, &pool, &etna_build_ctx(Some(&fi), &overflows, B256::ZERO));

        // The forced transaction once, at position 0, then its sender's next nonce.
        assert_eq!(builder.executor.receipts().len(), 2);
        assert_eq!(nonce_of(&mut builder, fi_caller), 2);
    }
}
