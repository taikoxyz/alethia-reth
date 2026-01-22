//! Transaction selection utilities for building blocks from the transaction pool.
//!
//! This module provides a unified interface for selecting and executing transactions
//! from the mempool, used by both the payload builder and the RPC pre-building endpoint.

use alloy_eips::Encodable2718;
use alloy_primitives::Address;
use flate2::{Compression, write::ZlibEncoder};
use op_alloy_flz::tx_estimated_size_fjord_bytes;
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockValidationError, InternalBlockExecutionError},
    execute::BlockBuilder,
};
use reth_primitives::Recovered;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_transaction_pool::{
    BestTransactionsAttributes, PoolTransaction, TransactionPool,
    error::InvalidPoolTransactionError,
};
use std::io::Write;
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
    /// Local accounts to prioritize.
    /// If non-empty, only transactions from these accounts are included.
    pub locals: Vec<Address>,
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

const MIN_DA_HEADROOM_BYTES: u64 = 8 * 1024;
const DA_HEADROOM_PERCENT: u64 = 2;

fn da_headroom(limit: u64) -> u64 {
    let pct = limit.saturating_mul(DA_HEADROOM_PERCENT) / 100;
    pct.max(MIN_DA_HEADROOM_BYTES)
}

#[derive(Debug, Default)]
struct ListState {
    executed: ExecutedTxList,
    da_state: TxListState,
}

#[derive(Debug, Default)]
struct TxListState {
    raw_txs: Vec<Vec<u8>>,
    est_total: u64,
    cached_real: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy)]
enum DaFit {
    FitsEstimated,
    FitsReal(usize),
    NeedsNewList,
    TooLarge,
}

impl TxListState {
    fn check_da_fit(
        &self,
        est: u64,
        candidate: &[u8],
        limit: u64,
        headroom: u64,
    ) -> Result<DaFit, BlockExecutionError> {
        let threshold = limit.saturating_sub(headroom);
        let est_after = self.est_total.saturating_add(est);
        if est_after <= threshold {
            return Ok(DaFit::FitsEstimated);
        }

        let real_size = txlist_real_size_bytes_with_candidate(&self.raw_txs, candidate)?;
        if (real_size as u64) > limit {
            return Ok(if self.raw_txs.is_empty() { DaFit::TooLarge } else { DaFit::NeedsNewList });
        }

        Ok(DaFit::FitsReal(real_size))
    }

    fn push_raw(&mut self, raw: Vec<u8>, est: u64, real_size: Option<usize>) {
        self.raw_txs.push(raw);
        self.est_total = self.est_total.saturating_add(est);
        self.cached_real = real_size.map(|size| (size, self.raw_txs.len()));
    }
}

fn txlist_real_size_bytes_with_candidate(
    raw_txs: &[Vec<u8>],
    candidate: &[u8],
) -> Result<usize, BlockExecutionError> {
    let mut slices: Vec<&[u8]> = raw_txs.iter().map(|tx| tx.as_slice()).collect();
    slices.push(candidate);
    txlist_real_size_bytes_from_slices(&slices)
}

fn txlist_real_size_bytes_from_slices(slices: &[&[u8]]) -> Result<usize, BlockExecutionError> {
    let mut rlp_encoded = Vec::new();
    alloy_rlp::encode_list::<&[u8], [u8]>(slices, &mut rlp_encoded);

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&rlp_encoded).map_err(|err| {
        BlockExecutionError::Internal(InternalBlockExecutionError::msg(format!(
            "txlist zlib encode failed: {err}"
        )))
    })?;
    let compressed = encoder.finish().map_err(|err| {
        BlockExecutionError::Internal(InternalBlockExecutionError::msg(format!(
            "txlist zlib finish failed: {err}"
        )))
    })?;

    Ok(compressed.len())
}

/// Outcome of the transaction selection process.
#[derive(Debug)]
pub enum SelectionOutcome {
    /// Selection was cancelled before completion.
    Cancelled,
    /// Selection completed successfully with the produced lists.
    Completed(Vec<ExecutedTxList>),
}

/// Creates an internal error for when the transaction list invariant is violated.
fn lists_empty_error() -> BlockExecutionError {
    BlockExecutionError::Internal(InternalBlockExecutionError::msg(
        "tx selection invariant violated: lists is empty",
    ))
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

    let mut lists = vec![ListState::default()];
    let headroom = da_headroom(config.max_da_bytes_per_list);

    'select: while let Some(pool_tx) = best_txs.next() {
        // 1. Check cancellation
        if is_cancelled() {
            return Ok(SelectionOutcome::Cancelled);
        }

        // 2. Filter by locals (if configured)
        if !config.locals.is_empty() && !config.locals.contains(&pool_tx.sender()) {
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

        // 4. Early reject transactions that cannot fit in any list
        if pool_tx.gas_limit() > config.gas_limit_per_list {
            best_txs.mark_invalid(
                &pool_tx,
                &InvalidPoolTransactionError::ExceedsGasLimit(
                    pool_tx.gas_limit(),
                    config.gas_limit_per_list,
                ),
            );
            continue;
        }

        // 5. Calculate DA size upfront (needed for limit checks)
        let tx = pool_tx.to_consensus();
        let tx_bytes = tx.encoded_2718();
        let da_size = tx_estimated_size_fjord_bytes(&tx_bytes);

        loop {
            // 6. Check if transaction fits in current list; if not, try starting a new one
            let current = lists.last_mut().ok_or_else(lists_empty_error)?;
            let exceeds_gas =
                current.executed.total_gas_used + pool_tx.gas_limit() > config.gas_limit_per_list;
            if exceeds_gas {
                if lists.len() >= config.max_lists {
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::ExceedsGasLimit(
                            pool_tx.gas_limit(),
                            config.gas_limit_per_list,
                        ),
                    );
                    continue 'select;
                }
                // Start a new list and retry the same tx
                lists.push(ListState::default());
                continue;
            }

            let da_fit = current.da_state.check_da_fit(
                da_size,
                &tx_bytes,
                config.max_da_bytes_per_list,
                headroom,
            )?;
            match da_fit {
                DaFit::NeedsNewList => {
                    if lists.len() >= config.max_lists {
                        best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
                        continue 'select;
                    }
                    lists.push(ListState::default());
                    continue;
                }
                DaFit::TooLarge => {
                    best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Underpriced);
                    continue 'select;
                }
                DaFit::FitsEstimated | DaFit::FitsReal(_) => {}
            }

            // 7. Execute transaction
            let gas_used = match builder.execute_transaction(tx.clone()) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
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
                    continue 'select;
                }
                // Fatal error - stop selection
                Err(err) => return Err(err),
            };

            // 8. Record successful transaction
            let current = lists.last_mut().ok_or_else(lists_empty_error)?;
            current.executed.total_gas_used += gas_used;
            match da_fit {
                DaFit::FitsEstimated => {
                    current.executed.total_da_bytes =
                        current.executed.total_da_bytes.saturating_add(da_size);
                    current.da_state.push_raw(tx_bytes, da_size, None);
                }
                DaFit::FitsReal(real_size) => {
                    current.executed.total_da_bytes = real_size as u64;
                    current.da_state.push_raw(tx_bytes, da_size, Some(real_size));
                }
                DaFit::NeedsNewList | DaFit::TooLarge => {}
            }
            current.executed.transactions.push(ExecutedTx { tx, gas_used, da_size });

            trace!(target: "tx_selection", gas_used, da_size, "included transaction from pool");
            break;
        }
    }

    Ok(SelectionOutcome::Completed(lists.into_iter().map(|list| list.executed).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn da_size_check_rejects_single_tx_over_limit() {
        let tx = vec![0u8; 256];
        let size = txlist_real_size_bytes_with_candidate(&[], &tx).expect("size");
        let limit = (size as u64).saturating_sub(1);
        let headroom = limit;
        let list = TxListState::default();
        let decision = list.check_da_fit(1, &tx, limit, headroom).expect("fit check");
        assert!(matches!(decision, DaFit::TooLarge));
    }

    #[test]
    fn da_size_check_requests_new_list_when_overflowing() {
        let tx_a = vec![0u8; 128];
        let tx_b = vec![1u8; 128];
        let size_ab = txlist_real_size_bytes_with_candidate(&[tx_a.clone()], &tx_b).expect("size");
        let limit = (size_ab as u64).saturating_sub(1);
        let headroom = limit;
        let mut list = TxListState::default();
        list.push_raw(tx_a, 1, None);
        let decision = list.check_da_fit(1, &tx_b, limit, headroom).expect("fit check");
        assert!(matches!(decision, DaFit::NeedsNewList));
    }
}
