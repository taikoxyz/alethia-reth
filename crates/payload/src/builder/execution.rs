//! Transaction execution helpers used by Taiko payload building.

use alloy_consensus::Transaction;
use alloy_eips::eip4844::BYTES_PER_BLOB;
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
use reth_primitives_traits::{Header as RethHeader, NodePrimitives, Recovered};
use tracing::{debug, trace, warn};

use alethia_reth_block::{
    executor::is_zk_gas_limit_exceeded,
    tx_selection::{
        DEFAULT_DA_ZLIB_GUARD_BYTES, SelectionOutcome, TxSelectionConfig,
        select_and_execute_pool_transactions,
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
    /// Timestamp for the new block.
    pub(super) block_timestamp: u64,
    /// Payload identifier for logging.
    pub(super) payload_id: String,
    /// Base fee per gas for transaction selection.
    pub(super) base_fee: u64,
    /// Block gas limit.
    pub(super) gas_limit: u64,
}

/// Executes the provided transaction list in legacy mode.
///
/// Preserves legacy mode: validation errors are skipped, fatal errors abort
/// the build, and cancellation short-circuits the loop.
pub(super) fn execute_provided_transactions(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives>,
    transactions: &[Recovered<EthTransactionSigned>],
    base_fee: u64,
    cancel: &CancelOnDrop,
) -> Result<ExecutionOutcome, PayloadBuilderError> {
    let mut total_fees = U256::ZERO;

    for tx in transactions {
        if cancel.is_cancelled() {
            return Ok(ExecutionOutcome::Cancelled);
        }

        if !is_allowed_tx_type(tx.inner()) {
            trace!(target: "payload_builder", ?tx, "skipping unsupported transaction type in legacy mode");
            continue;
        }

        let recovered_tx: Recovered<<EthPrimitives as NodePrimitives>::SignedTx> = tx.clone();
        let gas_used = match builder.execute_transaction(recovered_tx) {
            Ok(gas_output) => gas_output.tx_gas_used(),
            Err(err) if is_zk_gas_limit_exceeded(&err) => {
                debug!(
                    target: "payload_builder",
                    ?tx,
                    "stopping legacy-mode payload after zk gas exhaustion"
                );
                break;
            }
            Err(BlockExecutionError::Validation(
                BlockValidationError::InvalidTx { .. } |
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
            )) => {
                trace!(target: "payload_builder", ?tx, "skipping invalid transaction in legacy mode");
                continue;
            }
            // Fatal errors should still fail the build
            Err(err) => {
                warn!(target: "payload_builder", ?tx, %err, "fatal error executing transaction");
                return Err(PayloadBuilderError::evm(err));
            }
        };

        // Add transaction fees to total
        let miner_fee =
            tx.effective_tip_per_gas(base_fee).ok_or_else(|| missing_tip_error(base_fee))?;
        total_fees += U256::from(miner_fee) * U256::from(gas_used);
    }

    Ok(ExecutionOutcome::Completed(total_fees))
}

/// Executes fork-aware pool transactions until exhaustion or cancellation.
///
/// Legacy payloads validate and execute their supplied anchor before pool selection. Etna payloads
/// require no anchor and begin ordinary selection with the caller-supplied full gas budget.
pub(super) fn execute_pool_transactions<Client, Pool>(
    builder: &mut impl BlockBuilder<Primitives = EthPrimitives>,
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
    if chain_spec.is_etna_active(ctx.block_timestamp) {
        if ctx.anchor_tx.is_some() {
            return Err(PayloadBuilderError::Internal(RethError::msg(
                "Etna pool execution must not include an anchor transaction",
            )));
        }
        debug!(target: "payload_builder", id=%ctx.payload_id, "selecting anchorless Etna transactions");
    } else {
        let anchor_tx = ctx.anchor_tx.ok_or(PayloadBuilderError::MissingPayload)?;
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
    }

    // Use the shared transaction selection logic for pool transactions.
    let config = TxSelectionConfig {
        base_fee: ctx.base_fee,
        gas_limit_per_list: ctx.gas_limit,
        max_da_bytes_per_list: BYTES_PER_BLOB as u64,
        da_size_zlib_guard_bytes: DEFAULT_DA_ZLIB_GUARD_BYTES,
        max_lists: 1,
        min_tip: 0,
        locals: vec![],
    };

    match select_and_execute_pool_transactions(builder, pool, &config, || cancel.is_cancelled()) {
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
        sync::Arc,
        task::{Context, Poll, Waker},
    };

    use super::*;
    use alloy_consensus::{
        SignableTransaction, Signed, TxEip1559, TxLegacy,
        transaction::{SignerRecoverable, TxHashable},
    };
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use reth::revm::State;
    use reth_evm::{
        EvmFactory,
        block::{BlockExecutionError, BlockExecutor, CommitChanges},
        execute::{BlockBuilder, BlockBuilderOutcome, ExecutorTx},
    };
    use reth_evm_ethereum::RethReceiptBuilder;
    use reth_primitives_traits::Recovered;
    use reth_provider::test_utils::MockEthProvider;
    use reth_storage_api::StateProvider;
    use reth_transaction_pool::{
        TransactionOrigin, TransactionPool, noop::NoopTransactionPool, test_utils::testing_pool,
    };

    use alethia_reth_block::{
        executor::{TaikoBlockExecutor, ZkGasLimitExceeded},
        testutil::{
            BENCH_LIMIT_TARGET, BENCH_SUCCESS_TARGET, ExecutorBackedBuilder, db_with_contracts,
            etna_chain_spec, etna_evm_env, etna_execution_ctx, recovered_tx, unzen_chain_spec,
            unzen_evm_env, unzen_execution_ctx,
        },
    };
    use alethia_reth_chainspec::spec::TaikoChainSpec;
    use alethia_reth_consensus::validation::{ANCHOR_V3_V4_GAS_LIMIT, ANCHOR_V4_SELECTOR};
    use alethia_reth_evm::{factory::TaikoEvmFactory, spec::TaikoSpecId};
    use alethia_reth_primitives::addresses::TAIKO_GOLDEN_TOUCH_ADDRESS;

    const BENCH_SUCCESS_CALLER: Address = Address::with_last_byte(0x30);
    const BENCH_LIMIT_CALLER: Address = Address::with_last_byte(0x31);
    const BENCH_LATE_CALLER: Address = Address::with_last_byte(0x32);
    const GOLDEN_TOUCH_PRIVATE_KEY: &str =
        "0x92954368afd3caa1f3ce3ead0069c1af414054aefe1ef9aeacc1bf426222ce38";

    fn test_anchor_transaction() -> Recovered<EthTransactionSigned> {
        let signer: PrivateKeySigner =
            GOLDEN_TOUCH_PRIVATE_KEY.parse().expect("golden touch private key should parse");
        let tx = TxEip1559 {
            chain_id: 167,
            nonce: 0,
            gas_limit: ANCHOR_V3_V4_GAS_LIMIT,
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: 0,
            to: BENCH_SUCCESS_TARGET.into(),
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

    fn test_ordinary_transaction(
        caller: Address,
        gas_limit: u64,
        gas_price: u128,
        input: Bytes,
    ) -> Recovered<EthTransactionSigned> {
        let tx = TxLegacy {
            chain_id: Some(167),
            nonce: 0,
            gas_price,
            gas_limit,
            to: TxKind::Call(BENCH_SUCCESS_TARGET),
            value: U256::ZERO,
            input,
        };
        let signature = Signature::new(U256::from(1_u64), U256::from(2_u64), false);
        let signed: EthTransactionSigned =
            Signed::new_unchecked(tx, signature, B256::with_last_byte(caller.as_slice()[19]))
                .into();
        Recovered::new_unchecked(signed, caller)
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

    struct FirstTxZkGasErrorBuilder<E> {
        inner: ExecutorBackedBuilder<E>,
        fail_next_execution: bool,
    }

    impl<E> BlockBuilder for FirstTxZkGasErrorBuilder<E>
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
            if self.fail_next_execution {
                self.fail_next_execution = false;
                return Err(BlockExecutionError::other(ZkGasLimitExceeded));
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

        let outcome = execute_provided_transactions(&mut builder, &transactions, 0, &cancel)
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
        let mut builder = FirstTxZkGasErrorBuilder {
            inner: ExecutorBackedBuilder { executor },
            fail_next_execution: true,
        };
        let anchor_tx = test_anchor_transaction();
        let client = test_client((*chain_spec).clone());
        let pool = NoopTransactionPool::default();
        let cancel = CancelOnDrop::default();
        let parent_header = RethHeader { timestamp: 0, number: 0, ..Default::default() };

        let result = execute_pool_transactions(
            &mut builder,
            &pool,
            &client,
            &PoolExecutionContext {
                anchor_tx: Some(&anchor_tx),
                parent_header: &parent_header,
                block_timestamp: 1,
                payload_id: "anchor-zk-gas".to_string(),
                base_fee: 0,
                gas_limit: 30_000_000,
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
    fn etna_pool_uses_full_gas_budget_and_matches_the_derived_list() {
        let caller = Address::with_last_byte(0x40);
        let ordinary = test_ordinary_transaction(caller, 5_000_000, 10, Bytes::new());
        let parent_header = RethHeader { timestamp: 0, number: 0, ..Default::default() };
        let cancel = CancelOnDrop::default();

        let legacy_spec = Arc::new(unzen_chain_spec());
        let mut legacy_state = State::builder()
            .with_database(db_with_contracts(&[
                (Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS), 0),
                (caller, 0),
            ]))
            .with_bundle_update()
            .build();
        let legacy_evm = TaikoEvmFactory.create_evm(&mut legacy_state, unzen_evm_env());
        let legacy_executor = TaikoBlockExecutor::new(
            legacy_evm,
            unzen_execution_ctx(),
            legacy_spec.clone(),
            RethReceiptBuilder::default(),
        );
        let mut legacy_builder = ExecutorBackedBuilder { executor: legacy_executor };
        let legacy_pool = testing_pool();
        block_on_ready(
            legacy_pool.add_consensus_transaction(ordinary.clone(), TransactionOrigin::External),
        )
        .expect("ordinary transaction should enter the legacy pool");
        let anchor = test_anchor_transaction();

        let legacy_outcome = execute_pool_transactions(
            &mut legacy_builder,
            &legacy_pool,
            &test_client((*legacy_spec).clone()),
            &PoolExecutionContext {
                anchor_tx: Some(&anchor),
                parent_header: &parent_header,
                block_timestamp: 1,
                payload_id: "legacy-reduced-budget".to_string(),
                base_fee: 0,
                gas_limit: 5_500_000_u64.saturating_sub(ANCHOR_V3_V4_GAS_LIMIT),
            },
            &cancel,
        )
        .expect("legacy pool execution should complete");
        let ExecutionOutcome::Completed(legacy_fees) = legacy_outcome else {
            panic!("legacy pool execution should not cancel")
        };
        assert_eq!(legacy_fees, U256::ZERO);
        assert_eq!(legacy_builder.executor.receipts().len(), 1, "only the anchor should execute");

        let etna_spec = Arc::new(etna_chain_spec());
        let mut pool_state = State::builder()
            .with_database(db_with_contracts(&[(caller, 0)]))
            .with_bundle_update()
            .build();
        let pool_evm = TaikoEvmFactory.create_evm(&mut pool_state, etna_evm_env());
        let pool_ctx = etna_execution_ctx(B256::with_last_byte(1));
        let pool_executor = TaikoBlockExecutor::new(
            pool_evm,
            pool_ctx.clone(),
            etna_spec.clone(),
            RethReceiptBuilder::default(),
        );
        let mut pool_builder = ExecutorBackedBuilder { executor: pool_executor };
        let etna_pool = testing_pool();
        block_on_ready(
            etna_pool.add_consensus_transaction(ordinary.clone(), TransactionOrigin::External),
        )
        .expect("ordinary transaction should enter the Etna pool");

        let pool_outcome = execute_pool_transactions(
            &mut pool_builder,
            &etna_pool,
            &test_client((*etna_spec).clone()),
            &PoolExecutionContext {
                anchor_tx: None,
                parent_header: &parent_header,
                block_timestamp: 1,
                payload_id: "etna-full-budget".to_string(),
                base_fee: 0,
                gas_limit: 5_500_000,
            },
            &cancel,
        )
        .expect("Etna pool execution should complete");

        let mut derived_state = State::builder()
            .with_database(db_with_contracts(&[(caller, 0)]))
            .with_bundle_update()
            .build();
        let derived_evm = TaikoEvmFactory.create_evm(&mut derived_state, etna_evm_env());
        let derived_ctx = etna_execution_ctx(B256::with_last_byte(1));
        let derived_executor = TaikoBlockExecutor::new(
            derived_evm,
            derived_ctx.clone(),
            etna_spec,
            RethReceiptBuilder::default(),
        );
        let mut derived_builder = ExecutorBackedBuilder { executor: derived_executor };
        let derived_outcome =
            execute_provided_transactions(&mut derived_builder, &[ordinary], 0, &cancel)
                .expect("Etna derived execution should complete");

        let ExecutionOutcome::Completed(pool_fees) = pool_outcome else {
            panic!("Etna pool execution should not cancel")
        };
        let ExecutionOutcome::Completed(derived_fees) = derived_outcome else {
            panic!("Etna derived execution should not cancel")
        };
        assert_eq!(pool_fees, derived_fees);
        assert_eq!(pool_builder.executor.receipts(), derived_builder.executor.receipts());
        assert_eq!(pool_builder.executor.receipts().len(), 1);
        assert_eq!(pool_ctx.finalized_block_zk_gas(), derived_ctx.finalized_block_zk_gas());
        assert!(pool_ctx.finalized_block_zk_gas() > 0);
    }

    #[test]
    fn etna_pool_counts_the_first_ordinary_transaction_against_the_da_budget() {
        let first_caller = Address::with_last_byte(0x41);
        let second_caller = Address::with_last_byte(0x42);
        let mut state_byte = 0x1234_5678_u32;
        let input: Vec<u8> = (0..90_000)
            .map(|_| {
                state_byte ^= state_byte << 13;
                state_byte ^= state_byte >> 17;
                state_byte ^= state_byte << 5;
                state_byte as u8
            })
            .collect();
        let first =
            test_ordinary_transaction(first_caller, 5_000_000, 20, Bytes::from(input.clone()));
        let second = test_ordinary_transaction(second_caller, 5_000_000, 10, Bytes::from(input));
        let spec = Arc::new(etna_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(first_caller, 0), (second_caller, 0)]))
            .with_bundle_update()
            .build();
        let mut da_only_env = etna_evm_env();
        da_only_env.cfg_env.spec = TaikoSpecId::SHASTA;
        let evm = TaikoEvmFactory.create_evm(&mut state, da_only_env);
        let executor = TaikoBlockExecutor::new(
            evm,
            etna_execution_ctx(B256::with_last_byte(1)),
            spec.clone(),
            RethReceiptBuilder::default(),
        );
        let mut builder = ExecutorBackedBuilder { executor };
        let pool = testing_pool();
        block_on_ready(pool.add_consensus_transaction(first, TransactionOrigin::External))
            .expect("first transaction should enter the pool");
        block_on_ready(pool.add_consensus_transaction(second, TransactionOrigin::External))
            .expect("second transaction should enter the pool");

        execute_pool_transactions(
            &mut builder,
            &pool,
            &test_client((*spec).clone()),
            &PoolExecutionContext {
                anchor_tx: None,
                parent_header: &RethHeader { timestamp: 0, number: 0, ..Default::default() },
                block_timestamp: 1,
                payload_id: "etna-da-budget".to_string(),
                base_fee: 0,
                gas_limit: 30_000_000,
            },
            &CancelOnDrop::default(),
        )
        .expect("Etna selection should complete");

        assert_eq!(builder.executor.receipts().len(), 1);
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
            let ctx = etna_execution_ctx(B256::with_last_byte(1));
            let executor = TaikoBlockExecutor::new(
                evm,
                ctx.clone(),
                spec.clone(),
                RethReceiptBuilder::default(),
            );
            let mut builder = ExecutorBackedBuilder { executor };
            builder.apply_pre_execution_changes().unwrap();
            let pool = testing_pool();
            // A single nonce chain fixes ordering: optional success, real zk exhaustion, then
            // a transaction that must remain unexecuted. The real executor emits Etna Validation.
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
                    block_timestamp: 1,
                    payload_id: format!("etna-zk-limit-{prefix_len}"),
                    base_fee: 0,
                    gas_limit: 30_000_000,
                },
                &CancelOnDrop::default(),
            )
            .expect("ordinary zk exhaustion should stop selection cleanly");
            let ExecutionOutcome::Completed(fees) = outcome else {
                panic!("zk exhaustion should not report cancellation")
            };
            assert_eq!(builder.executor.receipts().len(), prefix_len as usize);
            assert_eq!(fees, U256::from(prefix_len * 21_009 * 10));
            assert_eq!(ctx.finalized_block_zk_gas(), prefix_len * 243_111);
            drop(builder);
            use reth_revm::Database;
            let account = state.basic(caller).unwrap().unwrap();
            assert_eq!(
                account.nonce, prefix_len,
                "failed/later transactions must not advance nonce"
            );
            assert_eq!(account.balance, U256::from(10_000_000_000_u64) - fees);
        }
    }
}
