//! Transaction selection utilities for building blocks from the transaction pool.
//!
//! This module provides a unified interface for selecting and executing transactions
//! from the mempool, used by both the payload builder and the RPC pre-building endpoint.

use alloy_eips::Encodable2718;
use alloy_primitives::Address;
use alloy_rlp::{encode_list, list_length};
use core::fmt;
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
    error::{InvalidPoolTransactionError, PoolTransactionError},
};
use std::{
    error::Error,
    fmt::{Display, Formatter},
    io::Write,
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

/// Returns the zlib compression settings used by taiko-client-rs.
fn zlib_compression() -> Compression {
    Compression::default()
}

/// Error raised when the DA size limit would be exceeded.
#[derive(Debug)]
struct DaLimitExceeded {
    /// The DA size that was calculated.
    size: u64,
    /// The DA size limit that was exceeded.
    limit: u64,
}

impl Display for DaLimitExceeded {
    /// Formats the DA limit exceeded error message.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "transaction list DA size {} exceeds limit {}", self.size, self.limit)
    }
}

impl Error for DaLimitExceeded {}

impl PoolTransactionError for DaLimitExceeded {
    /// Indicates that this error does not represent a bad transaction.
    fn is_bad_transaction(&self) -> bool {
        false
    }

    /// Allows downcasting to the concrete error type.
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Tracks the observed ratio between actual and estimated DA sizes.
#[derive(Debug, Clone)]
struct DaRatioState {
    /// Max observed actual/estimated ratio in micros.
    max_ratio_micros: u64,
    /// Whether we've sampled at the mid threshold.
    sampled_mid: bool,
    /// Whether we've sampled at the high threshold.
    sampled_high: bool,
}

impl Default for DaRatioState {
    /// Creates a default DA ratio state.
    fn default() -> Self {
        Self { max_ratio_micros: RATIO_SCALE, sampled_mid: false, sampled_high: false }
    }
}

/// Creates an internal error for when the transaction list invariant is violated.
fn lists_empty_error() -> BlockExecutionError {
    BlockExecutionError::Internal(InternalBlockExecutionError::msg(
        "tx selection invariant violated: lists is empty",
    ))
}

/// Fixed-point scale for ratio tracking.
const RATIO_SCALE: u64 = 1_000_000;
/// Midpoint percentage to sample actual/estimated ratio.
const DA_RATIO_SAMPLE_MID_PCT: u64 = 50;
/// High watermark percentage to sample actual/estimated ratio.
const DA_RATIO_SAMPLE_HIGH_PCT: u64 = 80;

/// Returns true if we should run a full zlib size check for the candidate.
fn should_check_zlib_da_size(config: &TxSelectionConfig, estimated_after: u64) -> bool {
    // Inclusive boundary: trigger once we're within guard bytes of the limit.
    config.da_size_zlib_guard_bytes > 0 &&
        estimated_after.saturating_add(config.da_size_zlib_guard_bytes) >=
            config.max_da_bytes_per_list
}

/// Returns the threshold for triggering ratio sampling at the given percentage.
fn ratio_threshold(limit: u64, percent: u64) -> u64 {
    ((limit as u128).saturating_mul(percent as u128) / 100) as u64
}

/// Returns true if we should sample the actual ratio at this estimated size.
fn should_sample_ratio(state: &DaRatioState, estimated_after: u64, limit: u64) -> bool {
    (!state.sampled_mid && estimated_after >= ratio_threshold(limit, DA_RATIO_SAMPLE_MID_PCT)) ||
        (!state.sampled_high &&
            estimated_after >= ratio_threshold(limit, DA_RATIO_SAMPLE_HIGH_PCT))
}

/// Updates the max observed ratio and sampling flags.
fn update_ratio_state(state: &mut DaRatioState, estimated_after: u64, actual: u64, limit: u64) {
    if estimated_after > 0 {
        let ratio = (actual as u128)
            .saturating_mul(RATIO_SCALE as u128)
            .saturating_div(estimated_after as u128) as u64;
        state.max_ratio_micros = state.max_ratio_micros.max(ratio);
    }

    if estimated_after >= ratio_threshold(limit, DA_RATIO_SAMPLE_MID_PCT) {
        state.sampled_mid = true;
    }
    if estimated_after >= ratio_threshold(limit, DA_RATIO_SAMPLE_HIGH_PCT) {
        state.sampled_high = true;
    }
}

/// Returns an adjusted estimate based on the max observed ratio.
fn adjusted_estimate(estimated_after: u64, state: &DaRatioState) -> u64 {
    ((estimated_after as u128).saturating_mul(state.max_ratio_micros as u128) / RATIO_SCALE as u128)
        as u64
}

/// Returns the zlib-compressed byte size for the list plus the candidate.
fn zlib_tx_list_size_bytes(list: &ExecutedTxList, candidate: &Recovered<TransactionSigned>) -> u64 {
    let mut txs: Vec<&TransactionSigned> = Vec::with_capacity(list.transactions.len() + 1);
    txs.extend(list.transactions.iter().map(|etx| etx.tx.inner()));
    txs.push(candidate.inner());

    let mut rlp_bytes =
        Vec::with_capacity(list_length::<&TransactionSigned, TransactionSigned>(&txs));
    encode_list::<&TransactionSigned, TransactionSigned>(&txs, &mut rlp_bytes);

    let mut encoder = ZlibEncoder::new(Vec::new(), zlib_compression());
    if encoder.write_all(&rlp_bytes).is_err() {
        return u64::MAX;
    }
    let compressed_len = match encoder.finish() {
        Ok(data) => data.len(),
        Err(_) => return u64::MAX,
    };
    u64::try_from(compressed_len).unwrap_or(u64::MAX)
}

/// Returns the DA size that exceeded the limit, if any (estimated or actual).
fn exceeds_da_limit(
    list: &ExecutedTxList,
    tx: &Recovered<TransactionSigned>,
    tx_da_estimated: u64,
    state: &mut DaRatioState,
    config: &TxSelectionConfig,
) -> Option<u64> {
    let limit = config.max_da_bytes_per_list;
    let estimated_after = list.total_da_bytes.saturating_add(tx_da_estimated);
    if estimated_after > limit {
        return Some(estimated_after);
    }

    if config.da_size_zlib_guard_bytes == 0 {
        return None;
    }

    let mut run_zlib = should_check_zlib_da_size(config, estimated_after);
    if !run_zlib {
        run_zlib = adjusted_estimate(estimated_after, state) > limit ||
            should_sample_ratio(state, estimated_after, limit);
    }

    if run_zlib {
        // This is intentionally late-bound and only triggered near the limit.
        let actual = zlib_tx_list_size_bytes(list, tx);
        update_ratio_state(state, estimated_after, actual, limit);
        if actual > limit {
            return Some(actual);
        }
    }

    None
}

/// Returns whether the list exceeds gas and the DA limit (if exceeded).
fn exceeds_list_limits(
    list: &ExecutedTxList,
    tx: &Recovered<TransactionSigned>,
    tx_gas_limit: u64,
    tx_da_estimated: u64,
    state: &mut DaRatioState,
    config: &TxSelectionConfig,
) -> (bool, Option<u64>) {
    let exceeds_gas = list.total_gas_used.saturating_add(tx_gas_limit) > config.gas_limit_per_list;
    let exceeds_da = exceeds_da_limit(list, tx, tx_da_estimated, state, config);
    (exceeds_gas, exceeds_da)
}

/// Wraps a DA limit error in a pool error for logging and pruning.
fn da_limit_error(size: u64, limit: u64) -> InvalidPoolTransactionError {
    InvalidPoolTransactionError::Other(Box::new(DaLimitExceeded { size, limit }))
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

    let mut lists = Vec::with_capacity(config.max_lists.max(1));
    lists.push(ExecutedTxList::default());
    // Per-list state for adaptive DA size calibration.
    let mut da_guard_states = Vec::with_capacity(config.max_lists.max(1));
    da_guard_states.push(DaRatioState::default());

    while let Some(pool_tx) = best_txs.next() {
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

        // 4. Calculate DA size upfront (needed for limit checks)
        let tx = pool_tx.to_consensus();
        let da_size = tx_estimated_size_fjord_bytes(&tx.encoded_2718());

        // 5. Early reject transactions that cannot fit in any list
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
        if da_size > config.max_da_bytes_per_list {
            best_txs.mark_invalid(&pool_tx, &da_limit_error(da_size, config.max_da_bytes_per_list));
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
                // Can't fit in any list
                if exceeds_gas {
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::ExceedsGasLimit(
                            pool_tx.gas_limit(),
                            config.gas_limit_per_list,
                        ),
                    );
                } else if let Some(size) = exceeds_da {
                    best_txs.mark_invalid(
                        &pool_tx,
                        &da_limit_error(size, config.max_da_bytes_per_list),
                    );
                }
                continue;
            }
            // Start a new list
            lists.push(ExecutedTxList::default());
            da_guard_states.push(DaRatioState::default());

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
                if exceeds_gas {
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::ExceedsGasLimit(
                            pool_tx.gas_limit(),
                            config.gas_limit_per_list,
                        ),
                    );
                } else if let Some(size) = exceeds_da {
                    best_txs.mark_invalid(
                        &pool_tx,
                        &da_limit_error(size, config.max_da_bytes_per_list),
                    );
                }
                continue;
            }
        }

        // 7. Execute transaction
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

        // 8. Record successful transaction
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
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};

    fn make_signed_legacy_tx(input: Bytes) -> TransactionSigned {
        let tx = TxLegacy {
            chain_id: Some(ChainId::from(1u64)),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input,
        };
        let signature = Signature::new(U256::from(1), U256::from(2), false);
        let signed = Signed::new_unchecked(tx, signature, B256::ZERO);
        signed.into()
    }

    fn lcg_bytes(len: usize) -> Bytes {
        let mut out = Vec::with_capacity(len);
        let mut x = 0u8;
        for _ in 0..len {
            x = x.wrapping_mul(73).wrapping_add(41);
            out.push(x);
        }
        Bytes::from(out)
    }

    #[test]
    fn zlib_guard_rejects_when_actual_exceeds_limit() {
        let list = ExecutedTxList::default();
        let mut candidate = None;

        for len in (64..=2048).step_by(64) {
            let tx = make_signed_legacy_tx(lcg_bytes(len));
            let estimated = tx_estimated_size_fjord_bytes(&tx.encoded_2718());
            let recovered = Recovered::new_unchecked(tx, Address::ZERO);
            let actual = zlib_tx_list_size_bytes(&list, &recovered);
            if actual > estimated {
                candidate = Some((recovered, estimated, actual));
                break;
            }
        }

        let (recovered, estimated, actual) =
            candidate.expect("no tx found where zlib size exceeds estimate");

        let mut state = DaRatioState::default();
        let mut config = TxSelectionConfig {
            base_fee: 0,
            gas_limit_per_list: u64::MAX,
            max_da_bytes_per_list: estimated,
            da_size_zlib_guard_bytes: 1,
            max_lists: 1,
            min_tip: 0,
            locals: vec![],
        };

        assert!(actual > config.max_da_bytes_per_list);
        assert!(exceeds_da_limit(&list, &recovered, estimated, &mut state, &config).is_some());

        config.da_size_zlib_guard_bytes = 0;
        assert!(exceeds_da_limit(&list, &recovered, estimated, &mut state, &config).is_none());
    }
}
