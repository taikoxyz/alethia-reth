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

/// Executes a given transaction list in order.
///
/// Transactions larger than the remaining `tx_bytes_budget` are left out (`None` disables the
/// byte bound). Transaction validation failures are skipped, the first zk overflow ends the
/// list, and non-transaction errors fail the build. Cancellation is checked before each item.
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

        if !is_allowed_tx_type(tx.inner()) {
            trace!(target: "payload_builder", ?tx, "leaving out unsupported transaction type");
            continue;
        }
        let tx_bytes = tx.inner().length() as u64;
        if remaining_tx_bytes.is_some_and(|remaining| tx_bytes > remaining) {
            trace!(target: "payload_builder", ?tx, tx_bytes, "leaving out transaction larger than the remaining byte budget");
            continue;
        }
        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_output) => gas_output.tx_gas_used(),
            Err(err) if is_zk_gas_limit_exceeded(&err) => {
                debug!(target: "payload_builder", ?tx, "stopping given-list payload after zk gas exhaustion");
                break;
            }
            Err(BlockExecutionError::Validation(
                BlockValidationError::InvalidTx { .. } |
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
            )) => {
                trace!(target: "payload_builder", ?tx, "leaving out invalid transaction");
                continue;
            }
            Err(err) => return Err(PayloadBuilderError::evm(err)),
        };
        let tip = tx.effective_tip_per_gas(base_fee).ok_or_else(|| missing_tip_error(base_fee))?;
        total_fees += U256::from(tip) * U256::from(gas_used);
        remaining_tx_bytes = remaining_tx_bytes.map(|left| left.saturating_sub(tx_bytes));
    }

    Ok(ExecutionOutcome::Completed(total_fees))
}

/// Executes fork-aware pool transactions until exhaustion or cancellation.
///
/// Legacy payloads validate and execute their supplied anchor before pool selection. Etna payloads
/// require no anchor and begin ordinary selection with the caller-supplied full gas budget.
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
    let state_seed = if chain_spec.is_etna_active(ctx.block_timestamp) {
        // Etna attribute normalization already rejects an anchor, so selection starts directly.
        debug!(target: "payload_builder", id=%ctx.payload_id, "selecting anchorless Etna transactions");
        ctx.block_state_seed
    } else {
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

        // The anchor committed above, so selection starts from the state after it.
        advance_selection_state(ctx.block_state_seed, *anchor_tx.tx_hash())
    };

    // Use the shared transaction selection logic for pool transactions.
    let config = TxSelectionConfig {
        base_fee: ctx.base_fee,
        gas_limit_per_list: ctx.gas_limit,
        max_da_bytes_per_list: BYTES_PER_BLOB as u64,
        da_size_zlib_guard_bytes: DEFAULT_DA_ZLIB_GUARD_BYTES,
        max_lists: 1,
        min_tip: 0,
        locals: vec![],
        state_seed,
        max_tx_bytes_per_list: ctx.tx_bytes_budget,
    };

    match select_and_execute_pool_transactions(builder, pool, &config, ctx.zk_overflows, || {
        cancel.is_cancelled()
    }) {
        Ok(SelectionOutcome::Cancelled) => Ok(ExecutionOutcome::Cancelled),
        Ok(SelectionOutcome::Completed(lists)) => {
            // Calculate total fees from the executed transactions.
            let total_fees = match lists.first() {
                Some(list) => list.transactions.iter().try_fold(U256::ZERO, |acc, etx| {
                    let tip = etx
                        .tx
                        .effective_tip_per_gas(ctx.base_fee)
                        .ok_or_else(|| missing_tip_error(ctx.base_fee))?;
                    Ok::<_, PayloadBuilderError>(acc + U256::from(tip) * U256::from(etx.gas_used))
                })?,
                None => U256::ZERO,
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
        block::{BlockExecutionError, BlockExecutor, CommitChanges, InternalBlockExecutionError},
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
    fn singleton_given_list_includes_valid_transactions_and_leaves_invalid_ones_empty() {
        let funded = Address::with_last_byte(0x50);
        let unfunded = Address::with_last_byte(0x51);
        let success = recovered_tx(funded, BENCH_SUCCESS_TARGET, 0, 1);
        let success_len = success.inner().length() as u64;

        // A singleton build includes a valid transaction at the inclusive byte bound.
        for remaining in [None, Some(success_len)] {
            let mut builder = etna_builder(&[(funded, 0)]);
            let outcome = execute_provided_transactions(
                &mut builder,
                std::slice::from_ref(&success),
                0,
                remaining,
                &CancelOnDrop::default(),
            )
            .expect("a valid transaction should execute");
            let ExecutionOutcome::Completed(fees) = outcome else {
                panic!("a singleton build must complete")
            };
            assert!(fees > U256::ZERO);
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
            let outcome = execute_provided_transactions(
                &mut builder,
                std::slice::from_ref(&tx),
                0,
                remaining,
                &CancelOnDrop::default(),
            )
            .unwrap_or_else(|err| panic!("{case}: a transaction-level failure errored: {err}"));
            assert!(
                matches!(outcome, ExecutionOutcome::Completed(fees) if fees == U256::ZERO),
                "{case}"
            );
            assert!(builder.executor.receipts().is_empty(), "{case}: left a receipt");
            assert_eq!(nonce_of(&mut builder, tx.signer()), 0, "{case}: left state behind");
        }

        let mut env = etna_evm_env();
        env.block_env.basefee = 10;
        let mut builder = etna_builder_with_env(&[(funded, 0)], env);
        let outcome = execute_provided_transactions(
            &mut builder,
            std::slice::from_ref(&success),
            10,
            None,
            &CancelOnDrop::default(),
        )
        .expect("a fee below the base fee produces an empty singleton build");
        assert!(matches!(outcome, ExecutionOutcome::Completed(fees) if fees == U256::ZERO));
        assert!(builder.executor.receipts().is_empty());
        assert_eq!(nonce_of(&mut builder, funded), 0);

        let mut builder = etna_builder(&[(funded, 0)]);
        let limit = recovered_tx(funded, BENCH_LIMIT_TARGET, 0, 1);
        let outcome = execute_provided_transactions(
            &mut builder,
            &[limit],
            0,
            None,
            &CancelOnDrop::default(),
        )
        .expect("a zk overflow produces a completed empty singleton build");
        assert!(matches!(outcome, ExecutionOutcome::Completed(fees) if fees == U256::ZERO));
        assert!(builder.executor.receipts().is_empty());
        assert_eq!(nonce_of(&mut builder, funded), 0);
    }

    #[test]
    fn singleton_given_list_keeps_non_transaction_errors() {
        let caller = Address::with_last_byte(0x52);
        let tx = recovered_tx(caller, BENCH_SUCCESS_TARGET, 0, 1);
        let builder_failing_with = |next_error| FirstTxErrorBuilder {
            inner: etna_builder(&[(caller, 0)]),
            next_error: Some(next_error),
        };

        // The pre-Etna wrapping of a zk overflow is still an overflow.
        let mut builder = builder_failing_with(BlockExecutionError::other(ZkGasLimitExceeded));
        let outcome = execute_provided_transactions(
            &mut builder,
            std::slice::from_ref(&tx),
            0,
            None,
            &CancelOnDrop::default(),
        )
        .expect("a legacy-wrapped zk overflow produces an empty singleton build");
        assert!(matches!(outcome, ExecutionOutcome::Completed(fees) if fees == U256::ZERO));

        // A failure that is not about the transaction is never turned into a left-out
        // transaction, whichever error shape carries it: an internal error, a fatal EVM error
        // (for example a database error), a non-fatal EVM error that is not a transaction
        // validation error, or another validation error.
        let failures = [
            ("internal", BlockExecutionError::msg("state read failed")),
            (
                "fatal evm",
                BlockExecutionError::Internal(InternalBlockExecutionError::EVM {
                    hash: *tx.tx_hash(),
                    error: "state read failed".into(),
                }),
            ),
            (
                "non-fatal evm",
                BlockExecutionError::Validation(BlockValidationError::EVM {
                    hash: *tx.tx_hash(),
                    error: "state read failed".into(),
                }),
            ),
            (
                "other validation",
                BlockExecutionError::Validation(BlockValidationError::msg("state read failed")),
            ),
        ];
        for (case, failure) in failures {
            let mut builder = builder_failing_with(failure);
            let err = execute_provided_transactions(
                &mut builder,
                std::slice::from_ref(&tx),
                0,
                None,
                &CancelOnDrop::default(),
            )
            .err()
            .expect("a non-transaction failure must fail the build");
            assert!(err.to_string().contains("state read failed"), "{case}: {err}");
        }
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
        let pool = pool_with(txs.clone());
        let [a_len, b_len, c_len] = txs.each_ref().map(|tx| pooled_len(&pool, tx));
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

    /// Parent header of the Etna pool-build tests; Etna selection does not read it.
    static ETNA_PARENT: LazyLock<RethHeader> = LazyLock::new(RethHeader::default);

    /// Transaction byte budget of the Etna build tests that do not exercise the byte bound.
    const ETNA_TX_BYTES_BUDGET: Option<u64> = Some(100_000);

    /// Returns the context of an Etna pool build with a zero base fee and the 30M gas limit of
    /// the Etna test environment,
    /// [`ETNA_TX_BYTES_BUDGET`], `zk_overflows` and `block_state_seed`.
    fn etna_build_ctx<'a>(
        zk_overflows: &'a ZkOverflowMemory,
        block_state_seed: B256,
    ) -> PoolExecutionContext<'a> {
        PoolExecutionContext {
            anchor_tx: None,
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

        let total_fees =
            run_etna_build(&mut builder, &pool, &etna_build_ctx(&overflows, block_state_seed));

        // `l` overflowed and was left out; `s`, from another sender, was still selected.
        let receipts = builder.executor.receipts().to_vec();
        assert_eq!(receipts.len(), 1);
        assert_eq!(total_fees, U256::from(10 * receipts[0].cumulative_gas_used));
        assert_eq!(nonce_of(&mut builder, l_caller), 0);
        assert_eq!(nonce_of(&mut builder, s_caller), 1);
        assert_eq!(overflows.state(&l_hash), Some(block_state_seed));
    }
}
