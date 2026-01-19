//! Transaction selection utilities for building blocks from the transaction pool.
//!
//! This module provides a unified interface for selecting and executing transactions
//! from the mempool, used by both the payload builder and the RPC pre-building endpoint.

use alloy_eips::Encodable2718;
use alloy_primitives::Address;
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockValidationError},
    execute::BlockBuilder,
};
use reth_primitives::Recovered;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    BestTransactionsAttributes, PoolTransaction, TransactionPool,
    error::InvalidPoolTransactionError,
};
use tracing::trace;

/// Configuration for transaction selection.
#[derive(Debug, Clone)]
pub struct TxSelectionConfig {
    /// Base fee per gas for tip calculation.
    pub base_fee: u64,
    /// Maximum gas allowed per list.
    pub gas_limit_per_list: u64,
    /// Maximum DA bytes allowed per list (compressed size).
    pub max_da_bytes_per_list: u64,
    /// Maximum number of transaction lists to produce.
    pub max_lists: usize,
    /// Minimum tip required for a transaction to be included.
    pub min_tip: u64,
    /// Optional list of local accounts to prioritize.
    /// If set and non-empty, only transactions from these accounts are included.
    pub locals: Option<Vec<Address>>,
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

/// Selects and executes transactions from the pool.
///
/// This function iterates through the best transactions in the pool, applying
/// the configured filters and limits, and executes them against the provided
/// block builder.
///
/// # Arguments
///
/// * `builder` - The block builder to execute transactions against.
/// * `pool` - The transaction pool to select from.
/// * `config` - Configuration for transaction selection.
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
    is_cancelled: impl Fn() -> bool,
) -> Result<SelectionOutcome, BlockExecutionError>
where
    B: BlockBuilder<Primitives = EthPrimitives>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    let mut best_txs = pool
        .best_transactions_with_attributes(BestTransactionsAttributes::new(config.base_fee, None));

    let mut lists = vec![ExecutedTxList::default()];

    while let Some(pool_tx) = best_txs.next() {
        // 1. Check cancellation
        if is_cancelled() {
            return Ok(SelectionOutcome::Cancelled);
        }

        // 2. Filter by locals (if configured)
        if let Some(ref local_accounts) = config.locals
            && !local_accounts.is_empty()
            && !local_accounts.contains(&pool_tx.sender())
        {
            // Mark as underpriced to skip this transaction and its dependents
            best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
            continue;
        }

        // 3. Filter by min_tip
        let tip = pool_tx.effective_tip_per_gas(config.base_fee);
        if tip.is_none_or(|t| t < config.min_tip as u128) {
            trace!(target: "tx_selection", ?pool_tx, "skipping transaction with insufficient tip");
            best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
            continue;
        }

        // 4. Check gas limit for current list, start new list if needed
        let current = lists.last().expect("at least one list always exists");
        if current.total_gas_used + pool_tx.gas_limit() > config.gas_limit_per_list {
            if lists.len() >= config.max_lists {
                // Can't fit in any list, mark as exceeding gas limit
                best_txs.mark_invalid(
                    &pool_tx,
                    &InvalidPoolTransactionError::ExceedsGasLimit(
                        pool_tx.gas_limit(),
                        config.gas_limit_per_list,
                    ),
                );
                continue;
            }
            // Start a new list
            lists.push(ExecutedTxList::default());
        }

        // 5. Calculate DA size and check limit
        let tx = pool_tx.to_consensus();
        let da_size = tx_estimated_size_fjord_bytes(&tx.encoded_2718());

        let current = lists.last().expect("at least one list always exists");
        if current.total_da_bytes + da_size > config.max_da_bytes_per_list {
            if lists.len() >= config.max_lists {
                // Can't fit in any list due to DA size, mark as underpriced
                best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
                continue;
            }
            // Start a new list
            lists.push(ExecutedTxList::default());
        }

        // 6. Execute transaction
        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_used) => gas_used,
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
                        &InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::TxTypeNotSupported,
                        ),
                    );
                }
                continue;
            }
            // Fatal error - stop selection
            Err(err) => return Err(err),
        };

        // 7. Record successful transaction
        let current = lists.last_mut().expect("at least one list always exists");
        current.total_gas_used += gas_used;
        current.total_da_bytes += da_size;
        current.transactions.push(ExecutedTx { tx, gas_used, da_size });

        trace!(target: "tx_selection", gas_used, da_size, "included transaction from pool");
    }

    Ok(SelectionOutcome::Completed(lists))
}
