//! Adaptive DA-size limit helpers for transaction selection.

use alloy_consensus::transaction::Recovered;
use alloy_rlp::{encode_list, list_length};
use core::fmt;
use flate2::{Compression, write::ZlibEncoder};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::block::{BlockExecutionError, InternalBlockExecutionError};
use reth_transaction_pool::error::{InvalidPoolTransactionError, PoolTransactionError};
use std::{
    error::Error,
    fmt::{Display, Formatter},
    io::Write,
};

use super::{ExecutedTxList, TxSelectionConfig};

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
pub(super) struct DaRatioState {
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

/// Fixed-point scale for ratio tracking.
const RATIO_SCALE: u64 = 1_000_000;
/// Midpoint percentage to sample actual/estimated ratio.
const DA_RATIO_SAMPLE_MID_PCT: u64 = 50;
/// High watermark percentage to sample actual/estimated ratio.
const DA_RATIO_SAMPLE_HIGH_PCT: u64 = 80;

/// Returns the zlib compression settings used by taiko-client-rs.
fn zlib_compression() -> Compression {
    Compression::default()
}

/// Creates an internal error for when the transaction list invariant is violated.
pub(super) fn lists_empty_error() -> BlockExecutionError {
    BlockExecutionError::Internal(InternalBlockExecutionError::msg(
        "tx selection invariant violated: lists is empty",
    ))
}

/// Returns true if we should run a full zlib size check for the candidate.
fn should_check_zlib_da_size(config: &TxSelectionConfig, estimated_after: u64) -> bool {
    // Inclusive boundary: trigger once we're within guard bytes of the limit.
    config.da_size_zlib_guard_bytes > 0
        && estimated_after.saturating_add(config.da_size_zlib_guard_bytes)
            >= config.max_da_bytes_per_list
}

/// Returns the threshold for triggering ratio sampling at the given percentage.
fn ratio_threshold(limit: u64, percent: u64) -> u64 {
    ((limit as u128).saturating_mul(percent as u128) / 100) as u64
}

/// Returns true if we should sample the actual ratio at this estimated size.
fn should_sample_ratio(state: &DaRatioState, estimated_after: u64, limit: u64) -> bool {
    (!state.sampled_mid && estimated_after >= ratio_threshold(limit, DA_RATIO_SAMPLE_MID_PCT))
        || (!state.sampled_high
            && estimated_after >= ratio_threshold(limit, DA_RATIO_SAMPLE_HIGH_PCT))
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
        run_zlib = adjusted_estimate(estimated_after, state) > limit
            || should_sample_ratio(state, estimated_after, limit);
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
pub(super) fn exceeds_list_limits(
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
pub(super) fn da_limit_error(size: u64, limit: u64) -> InvalidPoolTransactionError {
    InvalidPoolTransactionError::Other(Box::new(DaLimitExceeded { size, limit }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, ChainId, Signature, TxKind, U256};
    use op_alloy_flz::tx_estimated_size_fjord_bytes;

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

    #[test]
    fn zlib_guard_threshold_is_inclusive() {
        let config = TxSelectionConfig {
            base_fee: 0,
            gas_limit_per_list: 100,
            max_da_bytes_per_list: 1_000,
            da_size_zlib_guard_bytes: 100,
            max_lists: 1,
            min_tip: 0,
            locals: vec![],
        };

        assert!(!should_check_zlib_da_size(&config, 899));
        assert!(should_check_zlib_da_size(&config, 900));
    }

    #[test]
    fn adjusted_estimate_tracks_observed_ratio() {
        let mut state = DaRatioState::default();
        assert_eq!(adjusted_estimate(500, &state), 500);

        // Sample ratio 1.5x at 50% occupancy and ensure future adjusted estimates scale.
        update_ratio_state(&mut state, 500, 750, 1_000);
        assert_eq!(adjusted_estimate(400, &state), 600);
    }
}
