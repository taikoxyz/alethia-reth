//! Transaction selection utilities for building blocks from the transaction pool.
//!
//! This module provides a unified interface for selecting and executing transactions
//! from the mempool, used by both the payload builder and the RPC pre-building endpoint.

use alethia_reth_primitives::transaction::is_allowed_tx_type;
use alloy_consensus::transaction::Recovered;
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256};
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockValidationError},
    execute::BlockBuilder,
};
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    BestTransactionsAttributes, PoolTransaction, TransactionPool,
    error::InvalidPoolTransactionError,
};
use tracing::trace;

use self::{
    limits::{DaRatioState, da_limit_error, exceeds_list_limits, lists_empty_error},
    overflow::ZkGasBudgetExceeded,
};
use crate::executor::{TxSelectionExecutor, is_zk_gas_limit_exceeded};

/// DA-limit checking and adaptive zlib sizing helpers.
mod limits;
/// Memory of zk gas overflows and the execution state each happened at.
mod overflow;

pub use limits::zlib_compressed_len;
pub use overflow::{ZkOverflowMemory, advance_selection_state, selection_state_seed};

/// Returns the appropriate pool error for the exceeded limit.
fn limit_exceeded_error(
    gas_limit: u64,
    exceeds_gas: bool,
    exceeds_da: Option<u64>,
    config: &TxSelectionConfig,
) -> InvalidPoolTransactionError {
    if exceeds_gas {
        InvalidPoolTransactionError::ExceedsGasLimit(gas_limit, config.gas_limit_per_list)
    } else if let Some(size) = exceeds_da {
        da_limit_error(size, config.max_da_bytes_per_list)
    } else {
        // Caller only invokes this when at least one limit is exceeded;
        // catch logic bugs in dev but avoid crashing in production.
        debug_assert!(false, "called limit_exceeded_error without an exceeded limit");
        InvalidPoolTransactionError::ExceedsGasLimit(gas_limit, config.gas_limit_per_list)
    }
}

/// Returns the pool error that skips a transaction overflowing the block zk gas budget, along
/// with its sender's later transactions.
fn zk_gas_budget_error() -> InvalidPoolTransactionError {
    InvalidPoolTransactionError::Other(Box::new(ZkGasBudgetExceeded))
}

/// Configuration for transaction selection.
#[derive(Debug, Clone)]
pub struct TxSelectionConfig {
    /// Base fee per gas for tip calculation.
    pub base_fee: u64,
    /// Maximum gas allowed per list.
    pub gas_limit_per_list: u64,
    /// Maximum DA bytes allowed per list (compressed size).
    pub max_da_bytes_per_list: u64,
    /// When non-zero, run a full RLP+zlib size check once the estimated list size is within this
    /// many bytes of the limit.
    pub da_size_zlib_guard_bytes: u64,
    /// Maximum number of transaction lists to produce.
    pub max_lists: usize,
    /// Minimum tip required for a transaction to be included.
    pub min_tip: u64,
    /// Local accounts to prioritize.
    /// If non-empty, only transactions from these accounts are included.
    pub locals: Vec<Address>,
    /// Digest of the execution state selection starts from; see [`selection_state_seed`].
    pub state_seed: B256,
}

/// A successfully executed transaction with metadata.
#[derive(Debug, Clone)]
pub struct ExecutedTx {
    /// The executed transaction.
    pub tx: Recovered<TransactionSigned>,
    /// Gas used by the transaction.
    pub gas_used: u64,
    /// Estimated DA size (compressed).
    pub da_size: u64,
}

/// A list of executed transactions with cumulative statistics.
#[derive(Debug, Clone, Default)]
pub struct ExecutedTxList {
    /// The executed transactions in this list.
    pub transactions: Vec<ExecutedTx>,
    /// Total gas used by all transactions in this list.
    pub total_gas_used: u64,
    /// Total DA bytes used by all transactions in this list.
    pub total_da_bytes: u64,
}

/// Outcome of the transaction selection process.
#[derive(Debug)]
pub enum SelectionOutcome {
    /// Selection was cancelled before completion.
    Cancelled,
    /// Selection completed successfully with the produced lists.
    Completed(Vec<ExecutedTxList>),
}

/// Default threshold for triggering the zlib guard check.
pub const DEFAULT_DA_ZLIB_GUARD_BYTES: u64 = 4 * 1024;

/// Selects and executes transactions from the pool.
///
/// This function iterates through the best transactions in the pool, applying
/// the configured filters and limits, and executes them against the provided
/// block builder.
///
/// # Zk gas overflows
///
/// A transaction that overflows the block zk gas budget is left out, together with its sender's
/// later transactions, and selection moves on to the next candidate. `overflows` remembers each
/// such transaction with the execution state it ran against. That state starts at
/// [`TxSelectionConfig::state_seed`] and advances with every transaction this call commits.
///
/// - A remembered transaction is skipped without running while the state is the one it overflowed
///   at.
/// - At any other state it runs again. Re-runs never stop selection, but once the gas limits of
///   re-runs that overflowed again in this call add up to `gas_limit_per_list`, remembered
///   transactions are skipped instead. A re-run that fits is forgotten.
/// - Selection stops once the gas limits of first-time overflows in this call add up to
///   `gas_limit_per_list`.
/// - Selection stops as soon as the block zk gas budget cannot pay another transaction's intrinsic
///   charge.
///
/// Entries for transactions that are no longer in the pool are dropped before selection starts.
///
/// # Arguments
///
/// * `builder` - The block builder to execute transactions against.
/// * `pool` - The transaction pool to select from.
/// * `config` - Configuration for transaction selection.
/// * `overflows` - Transactions that overflowed the zk gas budget, with the state each overflowed
///   at; updated as selection runs.
/// * `is_cancelled` - A function that returns true if selection should be cancelled.
///
/// # Returns
///
/// * `Ok(SelectionOutcome::Cancelled)` - If cancelled during selection.
/// * `Ok(SelectionOutcome::Completed(lists))` - If selection completed successfully.
/// * `Err(err)` - If a fatal execution error occurred.
pub fn select_and_execute_pool_transactions<B, Pool>(
    builder: &mut B,
    pool: &Pool,
    config: &TxSelectionConfig,
    overflows: &ZkOverflowMemory,
    is_cancelled: impl Fn() -> bool,
) -> Result<SelectionOutcome, BlockExecutionError>
where
    B: BlockBuilder<Primitives = EthPrimitives>,
    B::Executor: TxSelectionExecutor,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    // Each zk gas overflow budget below sums transaction gas limits up to this cap.
    let overflow_gas_cap = config.gas_limit_per_list;
    overflows.retain(|tx_hash| pool.contains(tx_hash));
    // Digest of the execution state the next candidate runs against.
    let mut state = config.state_seed;
    // Gas limits of transactions that overflowed for the first time in this call.
    let mut first_time_overflow_gas = 0u64;
    // Gas limits of remembered transactions that ran again and overflowed again in this call.
    let mut rerun_overflow_gas = 0u64;

    let mut best_txs = pool
        .best_transactions_with_attributes(BestTransactionsAttributes::new(config.base_fee, None));

    let mut lists = vec![ExecutedTxList::default()];
    // Per-list state for adaptive DA size calibration.
    let mut da_guard_states = vec![DaRatioState::default()];

    while let Some(pool_tx) = best_txs.next() {
        // 1. Check cancellation
        if is_cancelled() {
            return Ok(SelectionOutcome::Cancelled);
        }

        // Stop once no transaction can pay its intrinsic zk gas charge.
        if !builder.executor().can_fit_another_transaction_zk_gas() {
            trace!(target: "tx_selection", "stopping selection: the block zk gas budget cannot pay another transaction's intrinsic charge");
            break;
        }

        // 2. Filter by locals (if configured)
        if !config.locals.is_empty() && !config.locals.contains(&pool_tx.sender()) {
            // Mark as underpriced to skip this transaction and its dependents
            best_txs.mark_invalid(&pool_tx, InvalidPoolTransactionError::Underpriced);
            continue;
        }

        // 3. Filter by min_tip
        let tip = pool_tx.effective_tip_per_gas(config.base_fee);
        if tip.is_none_or(|t| t < config.min_tip as u128) {
            trace!(target: "tx_selection", ?pool_tx, "skipping transaction with insufficient tip");
            best_txs.mark_invalid(&pool_tx, InvalidPoolTransactionError::Underpriced);
            continue;
        }

        // 4. Calculate DA size upfront (needed for limit checks)
        let tx = pool_tx.to_consensus();
        if !is_allowed_tx_type(tx.inner()) {
            best_txs.mark_invalid(
                &pool_tx,
                InvalidPoolTransactionError::Consensus(InvalidTransactionError::TxTypeNotSupported),
            );
            continue;
        }
        let da_size = tx_estimated_size_fjord_bytes(&tx.encoded_2718());

        // 5. Early reject transactions that cannot fit in any list
        if pool_tx.gas_limit() > config.gas_limit_per_list {
            best_txs.mark_invalid(
                &pool_tx,
                InvalidPoolTransactionError::ExceedsGasLimit(
                    pool_tx.gas_limit(),
                    config.gas_limit_per_list,
                ),
            );
            continue;
        }
        if da_size > config.max_da_bytes_per_list {
            best_txs.mark_invalid(&pool_tx, da_limit_error(da_size, config.max_da_bytes_per_list));
            continue;
        }

        // Skip a remembered zk gas overflow at the state it overflowed at, or once the re-run
        // allowance is used up.
        let tx_hash = *pool_tx.hash();
        let remembered = overflows.state(&tx_hash);
        if remembered == Some(state) {
            trace!(target: "tx_selection", ?tx_hash, "skipping transaction that overflowed the zk gas budget at this state");
            best_txs.mark_invalid(&pool_tx, zk_gas_budget_error());
            continue;
        }
        if remembered.is_some() && rerun_overflow_gas >= overflow_gas_cap {
            trace!(target: "tx_selection", ?tx_hash, rerun_overflow_gas, "skipping remembered zk gas overflow: re-run allowance used up");
            best_txs.mark_invalid(&pool_tx, zk_gas_budget_error());
            continue;
        }

        // 6. Check if transaction fits in current list; if not, try starting a new one
        let current_index = lists.len().saturating_sub(1);
        let current = lists.get(current_index).ok_or_else(lists_empty_error)?;
        let (exceeds_gas, exceeds_da) = exceeds_list_limits(
            current,
            &tx,
            pool_tx.gas_limit(),
            da_size,
            &mut da_guard_states[current_index],
            config,
        );

        if exceeds_gas || exceeds_da.is_some() {
            if lists.len() >= config.max_lists {
                let err =
                    limit_exceeded_error(pool_tx.gas_limit(), exceeds_gas, exceeds_da, config);
                best_txs.mark_invalid(&pool_tx, err);
                continue;
            }
            // Start a new list
            lists.push(ExecutedTxList::default());
            da_guard_states.push(DaRatioState::default());

            // Re-check against the fresh empty list (needed for the zlib guard edge case
            // where a single tx's actual compressed size exceeds the limit).
            let current_index = lists.len().saturating_sub(1);
            let current = lists.get(current_index).ok_or_else(lists_empty_error)?;
            let (exceeds_gas, exceeds_da) = exceeds_list_limits(
                current,
                &tx,
                pool_tx.gas_limit(),
                da_size,
                &mut da_guard_states[current_index],
                config,
            );
            if exceeds_gas || exceeds_da.is_some() {
                let err =
                    limit_exceeded_error(pool_tx.gas_limit(), exceeds_gas, exceeds_da, config);
                best_txs.mark_invalid(&pool_tx, err);
                continue;
            }
        }

        // 7. Execute transaction
        if remembered.is_some() {
            trace!(target: "tx_selection", ?tx_hash, "re-running transaction that overflowed the zk gas budget at another state");
        }
        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_output) => gas_output.tx_gas_used(),
            Err(err) if is_zk_gas_limit_exceeded(&err) => {
                // Clear the exhaustion this candidate left so the next one can run, and remember
                // the overflow at this state.
                builder.executor_mut().clear_zk_gas_exhaustion();
                overflows.record(tx_hash, state);
                best_txs.mark_invalid(&pool_tx, zk_gas_budget_error());
                if remembered.is_some() {
                    rerun_overflow_gas = rerun_overflow_gas.saturating_add(pool_tx.gas_limit());
                    trace!(target: "tx_selection", ?tx_hash, rerun_overflow_gas, "skipping transaction that overflowed the zk gas budget again");
                } else {
                    first_time_overflow_gas =
                        first_time_overflow_gas.saturating_add(pool_tx.gas_limit());
                    if first_time_overflow_gas >= overflow_gas_cap {
                        trace!(target: "tx_selection", ?tx_hash, first_time_overflow_gas, "stopping selection: first-time zk gas overflows reached the cap");
                        break;
                    }
                    trace!(target: "tx_selection", ?tx_hash, first_time_overflow_gas, "skipping transaction that overflowed the zk gas budget");
                }
                continue;
            }
            Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                error, ..
            })) => {
                if error.is_nonce_too_low() {
                    // Nonce too low - just skip, don't mark invalid
                    // (could be a race condition, transaction might be valid later)
                    trace!(target: "tx_selection", %error, ?tx, "skipping nonce too low transaction");
                } else {
                    // Other validation error - mark invalid to skip dependents
                    trace!(target: "tx_selection", %error, ?tx, "skipping invalid transaction and its descendants");
                    best_txs.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::TxTypeNotSupported,
                        ),
                    );
                }
                continue;
            }
            // Fatal error - stop selection
            Err(err) => return Err(err),
        };

        // 8. Record successful transaction
        state = advance_selection_state(state, tx_hash);
        if remembered.is_some() {
            overflows.forget(&tx_hash);
        }
        let current = lists.last_mut().ok_or_else(lists_empty_error)?;
        current.total_gas_used += gas_used;
        current.total_da_bytes += da_size;
        current.transactions.push(ExecutedTx { tx, gas_used, da_size });

        trace!(target: "tx_selection", gas_used, da_size, "included transaction from pool");
    }

    Ok(SelectionOutcome::Completed(lists))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::transaction::Recovered;
    use alloy_evm::EvmFactory;
    use alloy_primitives::{Address, B256, Bytes, TxHash};
    use reth_ethereum_primitives::TransactionSigned;
    use reth_evm::block::BlockExecutor;
    use reth_evm_ethereum::RethReceiptBuilder;
    use reth_revm::State;
    use reth_transaction_pool::{
        TransactionOrigin, TransactionPool,
        test_utils::{TestPool, testing_pool},
    };

    use super::{
        ExecutedTxList, SelectionOutcome, TxSelectionConfig, ZkOverflowMemory,
        advance_selection_state, select_and_execute_pool_transactions,
    };
    use crate::{
        executor::TaikoBlockExecutor,
        testutil::{
            BENCH_LIMIT_TARGET, BENCH_NEAR_LIMIT_TARGET, BENCH_SUCCESS_TARGET, BENCH_SWITCH_TARGET,
            ExecutorBackedBuilder, db_with_contracts, recovered_tx, recovered_tx_with_input,
            unzen_chain_spec, unzen_evm_env, unzen_execution_ctx,
        },
    };
    use alethia_reth_evm::{
        factory::TaikoEvmFactory,
        zk_gas::unzen::{BLOCK_ZK_GAS_LIMIT, TX_INTRINSIC_ZK_GAS},
    };

    const BENCH_INVALID_CALLER: Address = Address::with_last_byte(0x30);
    const BENCH_INCLUDED_CALLER: Address = Address::with_last_byte(0x31);
    const BENCH_LIMIT_CALLER: Address = Address::with_last_byte(0x32);
    const BENCH_LATE_CALLER: Address = Address::with_last_byte(0x33);
    const BENCH_SUCCESS_CALLER: Address = Address::with_last_byte(0x34);
    const BENCH_SWITCH_CALLER: Address = Address::with_last_byte(0x35);
    const BENCH_SETTER_CALLER: Address = Address::with_last_byte(0x36);
    /// First address byte of the callers that send zk gas overflowing transactions.
    const FIRST_LIMIT_CALLER: u8 = 0x40;

    /// Returns a selection config with a 30M gas limit per list. Every test transaction has a 5M
    /// gas limit, so six first-time overflows reach the cap.
    fn overflow_test_config(max_lists: usize, state_seed: B256) -> TxSelectionConfig {
        TxSelectionConfig {
            base_fee: 0,
            gas_limit_per_list: 30_000_000,
            max_da_bytes_per_list: 1_000_000,
            da_size_zlib_guard_bytes: 0,
            max_lists,
            min_tip: 0,
            locals: vec![],
            state_seed,
        }
    }

    /// Returns one zk gas overflowing transaction per tip, each from its own caller.
    fn overflowing_txs(tips: impl IntoIterator<Item = u64>) -> Vec<Recovered<TransactionSigned>> {
        tips.into_iter()
            .zip(FIRST_LIMIT_CALLER..)
            .map(|(tip, caller)| {
                recovered_tx(Address::with_last_byte(caller), BENCH_LIMIT_TARGET, 0, tip)
            })
            .collect()
    }

    /// Returns the hashes of `txs`, in order.
    fn hashes<'a>(txs: impl IntoIterator<Item = &'a Recovered<TransactionSigned>>) -> Vec<TxHash> {
        txs.into_iter().map(|tx| *tx.tx_hash()).collect()
    }

    /// Returns the funded accounts, at nonce 0, for the senders of `txs`.
    fn senders<'a>(
        txs: impl IntoIterator<Item = &'a Recovered<TransactionSigned>>,
    ) -> Vec<(Address, u64)> {
        txs.into_iter().map(|tx| (tx.signer(), 0)).collect()
    }

    /// Adds `txs` to `pool` as external transactions.
    fn add_to_pool(pool: &TestPool, txs: impl IntoIterator<Item = Recovered<TransactionSigned>>) {
        let rt = tokio::runtime::Builder::new_current_thread().build().expect("test runtime");
        for tx in txs {
            rt.block_on(pool.add_consensus_transaction(tx, TransactionOrigin::External))
                .expect("test tx should enter the pool");
        }
    }

    /// Runs one selection over `pool` on a fresh Unzen builder with `accounts` funded, after
    /// reserving `reserved_zk_gas` of the block zk gas budget. Returns the produced lists and the
    /// number of committed receipts.
    fn select_on_fresh_builder(
        accounts: &[(Address, u64)],
        reserved_zk_gas: u64,
        pool: &TestPool,
        config: &TxSelectionConfig,
        overflows: &ZkOverflowMemory,
    ) -> (Vec<ExecutedTxList>, usize) {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(accounts))
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
        builder.executor.reserve_block_zk_gas(reserved_zk_gas).expect("test reserve should fit");

        let outcome =
            select_and_execute_pool_transactions(&mut builder, pool, config, overflows, || false)
                .expect("selection should complete cleanly");
        let SelectionOutcome::Completed(lists) = outcome else {
            panic!("selection should not cancel")
        };
        (lists, builder.executor.receipts().len())
    }

    /// Returns the hashes of the transactions in `list`, in order.
    fn list_hashes(list: &ExecutedTxList) -> Vec<TxHash> {
        list.transactions.iter().map(|etx| *etx.tx.tx_hash()).collect()
    }

    fn select_near_limit_transaction(reserved_zk_gas: u64) -> (SelectionOutcome, u64) {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state = State::builder()
            .with_database(db_with_contracts(&[(BENCH_INCLUDED_CALLER, 0)]))
            .with_bundle_update()
            .build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let ctx = unzen_execution_ctx();
        let executor =
            TaikoBlockExecutor::new(evm, ctx.clone(), chain_spec, RethReceiptBuilder::default());
        let mut builder = ExecutorBackedBuilder { executor };
        builder.executor.reserve_block_zk_gas(reserved_zk_gas).expect("test reserve should fit");
        let pool = testing_pool();
        let rt = tokio::runtime::Builder::new_current_thread().build().expect("test runtime");
        rt.block_on(pool.add_consensus_transaction(
            recovered_tx(BENCH_INCLUDED_CALLER, BENCH_NEAR_LIMIT_TARGET, 0, 10),
            TransactionOrigin::External,
        ))
        .expect("near-limit tx should enter the pool");

        let outcome = select_and_execute_pool_transactions(
            &mut builder,
            &pool,
            &TxSelectionConfig {
                base_fee: 0,
                gas_limit_per_list: 30_000_000,
                max_da_bytes_per_list: 1_000_000,
                da_size_zlib_guard_bytes: 0,
                max_lists: 1,
                min_tip: 0,
                locals: vec![],
                state_seed: B256::ZERO,
            },
            &ZkOverflowMemory::default(),
            || false,
        )
        .expect("near-limit selection should complete cleanly");

        (outcome, ctx.finalized_block_zk_gas())
    }

    #[test]
    fn tx_selection_does_not_preallocate_the_caller_supplied_list_limit() {
        let chain_spec = Arc::new(unzen_chain_spec());
        let mut state =
            State::builder().with_database(db_with_contracts(&[])).with_bundle_update().build();
        let evm = TaikoEvmFactory.create_evm(&mut state, unzen_evm_env());
        let executor = TaikoBlockExecutor::new(
            evm,
            unzen_execution_ctx(),
            chain_spec,
            RethReceiptBuilder::default(),
        );
        let mut builder = ExecutorBackedBuilder { executor };
        let pool = testing_pool();

        let outcome = select_and_execute_pool_transactions(
            &mut builder,
            &pool,
            &TxSelectionConfig {
                base_fee: 0,
                gas_limit_per_list: 1,
                max_da_bytes_per_list: 1,
                da_size_zlib_guard_bytes: 0,
                max_lists: usize::MAX,
                min_tip: 0,
                locals: vec![],
                state_seed: B256::ZERO,
            },
            &ZkOverflowMemory::default(),
            || false,
        )
        .expect("an empty pool should complete without allocating the maximum list count");

        let SelectionOutcome::Completed(lists) = outcome else {
            panic!("selection should not cancel")
        };
        assert_eq!(lists.len(), 1);
        assert!(lists[0].transactions.is_empty());
    }

    #[test]
    fn tx_selection_respects_reserved_anchor_zk_gas() {
        let (without_reserve, without_reserve_zk_gas) = select_near_limit_transaction(0);
        let SelectionOutcome::Completed(without_reserve) = without_reserve else {
            panic!("selection should not cancel")
        };
        assert_eq!(without_reserve[0].transactions.len(), 1);
        assert!(without_reserve_zk_gas > 98_000_000);
        assert!(without_reserve_zk_gas <= 100_000_000);

        let (with_reserve, with_reserve_zk_gas) = select_near_limit_transaction(2_000_000);
        let SelectionOutcome::Completed(with_reserve) = with_reserve else {
            panic!("selection should not cancel")
        };
        assert!(with_reserve[0].transactions.is_empty());
        assert_eq!(with_reserve_zk_gas, 2_000_000);
    }

    #[test]
    fn tx_selection_skips_zk_gas_overflow_and_keeps_selecting() {
        let invalid = recovered_tx(BENCH_INVALID_CALLER, BENCH_SUCCESS_TARGET, 0, 40);
        let included = recovered_tx(BENCH_INCLUDED_CALLER, BENCH_SUCCESS_TARGET, 0, 30);
        let limit = recovered_tx(BENCH_LIMIT_CALLER, BENCH_LIMIT_TARGET, 0, 20);
        let limit_next = recovered_tx(BENCH_LIMIT_CALLER, BENCH_SUCCESS_TARGET, 1, 20);
        let late = recovered_tx(BENCH_LATE_CALLER, BENCH_SUCCESS_TARGET, 0, 10);
        let [included_hash, limit_hash, limit_next_hash, late_hash] =
            [&included, &limit, &limit_next, &late].map(|tx| *tx.tx_hash());
        let pool = testing_pool();
        add_to_pool(&pool, [invalid, included, limit, limit_next, late]);
        let config = overflow_test_config(1, B256::ZERO);
        let overflows = ZkOverflowMemory::default();

        let (lists, receipts) = select_on_fresh_builder(
            &[
                (BENCH_INVALID_CALLER, 1),
                (BENCH_INCLUDED_CALLER, 0),
                (BENCH_LIMIT_CALLER, 0),
                (BENCH_LATE_CALLER, 0),
            ],
            0,
            &pool,
            &config,
            &overflows,
        );

        assert_eq!(lists.len(), 1);
        assert_eq!(list_hashes(&lists[0]), vec![included_hash, late_hash]);
        assert!(!list_hashes(&lists[0]).contains(&limit_next_hash));
        assert_eq!(receipts, 2);
        // `included` committed before the overflow, so the overflow is remembered at that state.
        assert_eq!(
            overflows.state(&limit_hash),
            Some(advance_selection_state(config.state_seed, included_hash))
        );
    }

    #[test]
    fn tx_selection_stops_at_first_time_overflow_cap_and_skips_them_at_the_same_state() {
        let limit_txs = overflowing_txs((55..=60).rev());
        let limit_hashes = hashes(&limit_txs);
        let success = recovered_tx(BENCH_SUCCESS_CALLER, BENCH_SUCCESS_TARGET, 0, 10);
        let success_hash = *success.tx_hash();
        let accounts = senders(limit_txs.iter().chain([&success]));
        let pool = testing_pool();
        add_to_pool(&pool, limit_txs.into_iter().chain([success]));
        let seed = B256::with_last_byte(7);
        let overflows = ZkOverflowMemory::default();

        // Six first-time overflows reach the cap before the success tx gets to run.
        let (lists, receipts) = select_on_fresh_builder(
            &accounts,
            0,
            &pool,
            &overflow_test_config(1, seed),
            &overflows,
        );
        assert_eq!(lists.len(), 1);
        assert!(lists[0].transactions.is_empty());
        assert_eq!(receipts, 0);
        assert_eq!(overflows.len(), 6);
        for hash in &limit_hashes {
            assert_eq!(overflows.state(hash), Some(seed));
        }

        // At the same state the remembered overflows are left out again and stay remembered,
        // without stopping selection, so the success tx is included.
        let (lists, receipts) = select_on_fresh_builder(
            &accounts,
            0,
            &pool,
            &overflow_test_config(2, seed),
            &overflows,
        );
        assert_eq!(lists.len(), 1);
        assert_eq!(list_hashes(&lists[0]), vec![success_hash]);
        assert_eq!(receipts, 1);
        assert_eq!(overflows.len(), 6);
    }

    #[test]
    fn tx_selection_skips_remembered_overflow_at_the_same_state_without_opening_a_list() {
        let success = recovered_tx(BENCH_SUCCESS_CALLER, BENCH_SUCCESS_TARGET, 0, 20);
        let success_hash = *success.tx_hash();
        let limit = recovered_tx(BENCH_LIMIT_CALLER, BENCH_LIMIT_TARGET, 0, 10);
        let limit_hash = *limit.tx_hash();
        let accounts = senders([&success, &limit]);
        let pool = testing_pool();
        add_to_pool(&pool, [success, limit]);
        let seed = B256::with_last_byte(4);
        let after_success = advance_selection_state(seed, success_hash);
        let overflows = ZkOverflowMemory::default();
        overflows.record(limit_hash, after_success);
        // Each list fits one 5M gas limit transaction: after the success tx, the gas it used plus
        // the remembered tx's 5M gas limit is over the per-list limit.
        let config =
            TxSelectionConfig { gas_limit_per_list: 5_000_000, ..overflow_test_config(2, seed) };

        let (lists, receipts) = select_on_fresh_builder(&accounts, 0, &pool, &config, &overflows);

        // The remembered tx comes after the success tx, at the state it overflowed at. It does
        // not fit the current list, but it is skipped before the list checks, so no second list
        // is opened for it.
        assert_eq!(lists.len(), 1);
        assert_eq!(list_hashes(&lists[0]), vec![success_hash]);
        assert_eq!(receipts, 1);
        assert_eq!(overflows.state(&limit_hash), Some(after_success));
    }

    #[test]
    fn tx_selection_skips_at_the_same_state_without_using_the_rerun_allowance() {
        let limit_txs = overflowing_txs((55..=60).rev());
        let limit_hashes = hashes(&limit_txs);
        let refit = recovered_tx(BENCH_SUCCESS_CALLER, BENCH_SUCCESS_TARGET, 0, 50);
        let refit_hash = *refit.tx_hash();
        let accounts = senders(limit_txs.iter().chain([&refit]));
        let pool = testing_pool();
        add_to_pool(&pool, limit_txs.into_iter().chain([refit]));
        let seed = B256::with_last_byte(2);
        let overflows = ZkOverflowMemory::default();
        for hash in &limit_hashes {
            overflows.record(*hash, seed);
        }
        overflows.record(refit_hash, B256::with_last_byte(1));

        let (lists, receipts) = select_on_fresh_builder(
            &accounts,
            0,
            &pool,
            &overflow_test_config(1, seed),
            &overflows,
        );

        // The six skipped at the same state leave the whole allowance for the remembered tx from
        // another state, which now fits and is forgotten.
        assert_eq!(lists.len(), 1);
        assert_eq!(list_hashes(&lists[0]), vec![refit_hash]);
        assert_eq!(receipts, 1);
        assert_eq!(overflows.state(&refit_hash), None);
        for hash in &limit_hashes {
            assert_eq!(overflows.state(hash), Some(seed));
        }
    }

    #[test]
    fn tx_selection_reruns_remembered_overflows_within_allowance_without_stopping() {
        let limit_txs = overflowing_txs((54..=60).rev());
        let limit_hashes = hashes(&limit_txs);
        let success = recovered_tx(BENCH_SUCCESS_CALLER, BENCH_SUCCESS_TARGET, 0, 10);
        let success_hash = *success.tx_hash();
        let accounts = senders(limit_txs.iter().chain([&success]));
        let pool = testing_pool();
        add_to_pool(&pool, limit_txs.into_iter().chain([success]));
        let earlier = B256::with_last_byte(1);
        let seed = B256::with_last_byte(2);
        let overflows = ZkOverflowMemory::default();
        for hash in &limit_hashes {
            overflows.record(*hash, earlier);
        }

        let (lists, receipts) = select_on_fresh_builder(
            &accounts,
            0,
            &pool,
            &overflow_test_config(1, seed),
            &overflows,
        );

        // Six re-runs overflow again and use up the allowance, but selection keeps going.
        assert_eq!(lists.len(), 1);
        assert_eq!(list_hashes(&lists[0]), vec![success_hash]);
        assert_eq!(receipts, 1);
        for hash in &limit_hashes[..6] {
            assert_eq!(overflows.state(hash), Some(seed));
        }
        // The seventh is skipped without running once the allowance is used up.
        assert_eq!(overflows.state(&limit_hashes[6]), Some(earlier));
    }

    #[test]
    fn tx_selection_reruns_overflow_that_now_fits() {
        let switch = recovered_tx(BENCH_SWITCH_CALLER, BENCH_SWITCH_TARGET, 0, 15);
        let switch_hash = *switch.tx_hash();
        let accounts = [(BENCH_SWITCH_CALLER, 0), (BENCH_SETTER_CALLER, 0)];
        let pool = testing_pool();
        add_to_pool(&pool, [switch]);
        let config = overflow_test_config(1, B256::with_last_byte(3));
        let overflows = ZkOverflowMemory::default();

        // The switch is unset, so the tx overflows and is remembered.
        let (lists, receipts) = select_on_fresh_builder(&accounts, 0, &pool, &config, &overflows);
        assert_eq!(lists.len(), 1);
        assert!(lists[0].transactions.is_empty());
        assert_eq!(receipts, 0);
        assert_eq!(overflows.state(&switch_hash), Some(config.state_seed));

        // A higher-tip setter turns the switch on first. The remembered tx then runs at another
        // state, fits, and is forgotten.
        let setter = recovered_tx_with_input(
            BENCH_SETTER_CALLER,
            BENCH_SWITCH_TARGET,
            0,
            20,
            Bytes::from_static(&[1]),
        );
        let setter_hash = *setter.tx_hash();
        add_to_pool(&pool, [setter]);
        let (lists, receipts) = select_on_fresh_builder(&accounts, 0, &pool, &config, &overflows);
        assert_eq!(lists.len(), 1);
        assert_eq!(list_hashes(&lists[0]), vec![setter_hash, switch_hash]);
        assert_eq!(receipts, 2);
        assert_eq!(overflows.state(&switch_hash), None);

        // Control: without the remembered overflow the same pool selects the same list.
        let (lists, _) =
            select_on_fresh_builder(&accounts, 0, &pool, &config, &ZkOverflowMemory::default());
        assert_eq!(lists.len(), 1);
        assert_eq!(list_hashes(&lists[0]), vec![setter_hash, switch_hash]);
    }

    #[test]
    fn tx_selection_stops_when_no_transaction_can_pay_the_intrinsic_zk_gas() {
        let pool = testing_pool();
        add_to_pool(&pool, [recovered_tx(BENCH_SUCCESS_CALLER, BENCH_SUCCESS_TARGET, 0, 10)]);
        let overflows = ZkOverflowMemory::default();

        let (lists, receipts) = select_on_fresh_builder(
            &[(BENCH_SUCCESS_CALLER, 0)],
            BLOCK_ZK_GAS_LIMIT - TX_INTRINSIC_ZK_GAS + 1,
            &pool,
            &overflow_test_config(1, B256::ZERO),
            &overflows,
        );

        assert_eq!(lists.len(), 1);
        assert!(lists[0].transactions.is_empty());
        assert_eq!(receipts, 0);
        // Selection stops before running the tx, so nothing is remembered as an overflow.
        assert!(overflows.is_empty());
    }

    #[test]
    fn tx_selection_prunes_overflows_no_longer_in_the_pool() {
        let pool = testing_pool();
        add_to_pool(&pool, [recovered_tx(BENCH_SUCCESS_CALLER, BENCH_SUCCESS_TARGET, 0, 10)]);
        let gone = TxHash::repeat_byte(0xee);
        let overflows = ZkOverflowMemory::default();
        overflows.record(gone, B256::ZERO);

        select_on_fresh_builder(
            &[(BENCH_SUCCESS_CALLER, 0)],
            0,
            &pool,
            &overflow_test_config(1, B256::ZERO),
            &overflows,
        );

        assert_eq!(overflows.state(&gone), None);
    }
}
