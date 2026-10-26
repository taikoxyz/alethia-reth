//! Invariants introduced by the opt-in Etna hardfork.

use crate::extra_data::ETNA_EXTRA_DATA_LEN;
use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_rlp::{Encodable, length_of_length};
use core::fmt;
use reth_ethereum_primitives::TransactionSigned;

/// Smallest `maxBlockBytes` build attribute accepted on an Etna payload job.
///
/// A sanity floor on the driver-supplied block bound: a smaller value leaves little room beyond
/// the header, so it is rejected as an invalid parameter before any job starts.
pub const ETNA_MIN_MAX_BLOCK_BYTES: u64 = 1_024;

/// JSON-RPC error code returned when a payload job failed because the parent's state is pruned.
///
/// Drivers treat this code as a definitive build failure; every other build failure stays a
/// transient internal error (`-32603`).
pub const ENGINE_CANNOT_BUILD_ON_PARENT_CODE: i32 = -38_100;

/// JSON-RPC error message paired with [`ENGINE_CANNOT_BUILD_ON_PARENT_CODE`].
pub const ENGINE_CANNOT_BUILD_ON_PARENT_MESSAGE: &str = "cannot build on parent";

/// Error returned when an activated non-genesis block omits its beacon root commitment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingEtnaBeaconRoot {
    /// Height of the block whose beacon root is absent or zero.
    pub block_number: u64,
}

impl fmt::Display for MissingEtnaBeaconRoot {
    /// Describes the violated per-block beacon root requirement.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Etna block {} requires a non-zero parent beacon block root", self.block_number)
    }
}

impl core::error::Error for MissingEtnaBeaconRoot {}

/// Requires a non-zero parent beacon block root after Etna, except for the genesis block.
pub fn validate_etna_root(
    is_etna_active: bool,
    block_number: u64,
    root: Option<B256>,
) -> Result<(), MissingEtnaBeaconRoot> {
    if is_etna_active && block_number != 0 && root.is_none_or(|root| root.is_zero()) {
        return Err(MissingEtnaBeaconRoot { block_number });
    }
    Ok(())
}

/// Error returned when an activated non-genesis block lacks the 13-byte Etna extra data layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidEtnaExtraData {
    /// Height of the block whose extra data has the wrong length.
    pub block_number: u64,
    /// Actual extra data length in bytes.
    pub len: usize,
}

impl fmt::Display for InvalidEtnaExtraData {
    /// Describes the required Etna layout and the observed length.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Etna block {} requires {ETNA_EXTRA_DATA_LEN}-byte extraData \
             [basefeeSharingPctg | proposalId | anchorBlockNumber], got {} bytes",
            self.block_number, self.len
        )
    }
}

impl core::error::Error for InvalidEtnaExtraData {}

/// Requires the 13-byte Etna extra data layout after Etna, except for the genesis block.
pub fn validate_etna_extra_data(
    is_etna_active: bool,
    block_number: u64,
    extra_data: &[u8],
) -> Result<(), InvalidEtnaExtraData> {
    if is_etna_active && block_number != 0 && extra_data.len() != ETNA_EXTRA_DATA_LEN {
        return Err(InvalidEtnaExtraData { block_number, len: extra_data.len() });
    }
    Ok(())
}

/// Returns the executionBlock length, `rlp([header, [tx…]])` including its outer list header, or
/// `None` when it overflows `usize`.
///
/// `header_rlp_len` is the full RLP length of the EL header and `txs_payload_len` is the summed
/// block-body length of the transactions (the transaction list's payload, without its header).
const fn checked_execution_block_len(
    header_rlp_len: usize,
    txs_payload_len: usize,
) -> Option<usize> {
    let Some(txs_rlp_len) = txs_payload_len.checked_add(length_of_length(txs_payload_len)) else {
        return None;
    };
    let Some(block_payload_len) = header_rlp_len.checked_add(txs_rlp_len) else {
        return None;
    };
    block_payload_len.checked_add(length_of_length(block_payload_len))
}

/// Returns the length of the executionBlock `rlp([header, [tx…]])`, including its outer list
/// header, from the header's RLP length and the transaction list's payload length.
///
/// `header_rlp_len` is the full RLP length of the EL header. `txs_payload_len` is the sum of the
/// transactions' block-body encodings (a legacy transaction as an RLP list, a typed transaction as
/// an RLP string holding its EIP-2718 bytes), excluding the transaction list header. This is the
/// quantity bounded by `maxBlockBytes`. Saturates at `usize::MAX`, which no in-memory encoding
/// reaches.
pub const fn execution_block_len(header_rlp_len: usize, txs_payload_len: usize) -> usize {
    match checked_execution_block_len(header_rlp_len, txs_payload_len) {
        Some(len) => len,
        None => usize::MAX,
    }
}

/// Returns the largest transaction list payload length whose executionBlock still fits in
/// `max_block_bytes`, or `None` when even an empty transaction list does not fit.
///
/// This is the exact inverse of [`execution_block_len`] for a fixed header: for the returned `s`,
/// `execution_block_len(header_rlp_len, s) <= max_block_bytes` and
/// `execution_block_len(header_rlp_len, s + 1) > max_block_bytes`, across every RLP list header
/// size change. A limit above `usize::MAX` (only possible on 32-bit targets) is treated as
/// `usize::MAX`, since no larger encoding can exist in memory.
pub fn max_txs_payload_len(header_rlp_len: usize, max_block_bytes: u64) -> Option<u64> {
    let limit = usize::try_from(max_block_bytes).unwrap_or(usize::MAX);
    let fits = |txs_payload_len: usize| {
        checked_execution_block_len(header_rlp_len, txs_payload_len).is_some_and(|len| len <= limit)
    };
    // Both list headers take at least one byte, so no payload above `limit - header - 2` fits. The
    // length is strictly increasing in the payload and that start overshoots by at most the two
    // list headers' extra length bytes, so the loop runs a bounded handful of times.
    let mut txs_payload_len = limit.saturating_sub(header_rlp_len).saturating_sub(2);
    while !fits(txs_payload_len) {
        txs_payload_len = txs_payload_len.checked_sub(1)?;
    }
    u64::try_from(txs_payload_len).ok()
}

/// Returns the exact length of the executionBlock `rlp([header, [tx…]])` for `header` and
/// `transactions`, including its outer list header.
///
/// Transactions are measured in block-body form: a legacy transaction as an RLP list, a typed
/// transaction as an RLP string holding its EIP-2718 bytes.
pub fn execution_block_length(header: &Header, transactions: &[TransactionSigned]) -> usize {
    execution_block_len(header.length(), transactions.iter().map(Encodable::length).sum())
}

#[cfg(test)]
mod tests {
    use super::{
        ETNA_MIN_MAX_BLOCK_BYTES, InvalidEtnaExtraData, checked_execution_block_len,
        execution_block_len, execution_block_length, max_txs_payload_len, validate_etna_extra_data,
        validate_etna_root,
    };
    use alloy_consensus::{Header, Signed, TxEip1559, TxLegacy};
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
    use alloy_rlp::{Decodable, Encodable};
    use reth_ethereum_primitives::TransactionSigned;

    /// Asserts that `max_txs_payload_len` returns the largest fitting payload, or `None` when even
    /// an empty transaction list exceeds the limit.
    fn assert_tight_inverse(header_rlp_len: usize, max_block_bytes: u64) {
        let fits = |txs_payload_len: usize| {
            checked_execution_block_len(header_rlp_len, txs_payload_len)
                .is_some_and(|len| len as u64 <= max_block_bytes)
        };
        match max_txs_payload_len(header_rlp_len, max_block_bytes) {
            Some(txs_payload_len) => {
                let txs_payload_len =
                    usize::try_from(txs_payload_len).expect("payload length fits usize");
                assert!(
                    fits(txs_payload_len),
                    "h={header_rlp_len} limit={max_block_bytes}: {txs_payload_len} must fit"
                );
                assert!(
                    !fits(txs_payload_len + 1),
                    "h={header_rlp_len} limit={max_block_bytes}: {} must not fit",
                    txs_payload_len + 1
                );
            }
            None => assert!(
                !fits(0),
                "h={header_rlp_len} limit={max_block_bytes}: an empty list fits, None is wrong"
            ),
        }
    }

    #[test]
    fn execution_block_len_crosses_list_header_thresholds() {
        assert_eq!(execution_block_len(600, 0), 604);
        assert_eq!(execution_block_len(600, 55), 659);
        assert_eq!(execution_block_len(600, 56), 661);
        assert_eq!(execution_block_len(600, 255), 860);
        assert_eq!(execution_block_len(600, 256), 862);
        assert_eq!(execution_block_len(600, 65_535), 66_142);
        assert_eq!(execution_block_len(600, 65_536), 66_144);
    }

    #[test]
    fn max_txs_payload_len_is_the_tight_inverse() {
        assert_eq!(max_txs_payload_len(600, 124_000), Some(123_392));
        assert_eq!(execution_block_len(600, 123_392), 124_000);
        assert_eq!(execution_block_len(600, 123_393), 124_001);
        assert_eq!(max_txs_payload_len(600, 604), Some(0));
        assert_eq!(max_txs_payload_len(600, 603), None);
    }

    #[test]
    fn max_txs_payload_len_is_tight_at_every_list_header_threshold() {
        // Payload lengths at which an RLP list prefix gains a length byte.
        let thresholds = [56usize, 256, 65_536, 16_777_216];
        for header_rlp_len in [0usize, 1, 54, 55, 56, 255, 256, 600, 65_535, 65_536] {
            for threshold in thresholds {
                // `threshold` probes the outer list prefix, `threshold + header_rlp_len` probes the
                // transaction list prefix.
                for anchor in [threshold, threshold + header_rlp_len] {
                    for limit in anchor.saturating_sub(12)..=anchor + 12 {
                        assert_tight_inverse(header_rlp_len, limit as u64);
                    }
                }
            }
        }
        for limit in [0, 1, 2, ETNA_MIN_MAX_BLOCK_BYTES, u64::MAX] {
            assert_tight_inverse(600, limit);
        }
        assert!(max_txs_payload_len(600, u64::MAX).is_some());
    }

    #[test]
    fn execution_block_length_matches_the_encoded_embedded_list() {
        let header = Header {
            beneficiary: Address::with_last_byte(0x11),
            number: 7,
            gas_limit: 45_000_000,
            gas_used: 21_000,
            timestamp: 1_700_000_000,
            extra_data: Bytes::from(vec![0xab; 13]),
            base_fee_per_gas: Some(10_000_000),
            withdrawals_root: Some(B256::with_last_byte(1)),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::with_last_byte(2)),
            ..Default::default()
        };
        let signature = Signature::new(U256::from(1), U256::from(2), false);
        let legacy: TransactionSigned = Signed::new_unhashed(
            TxLegacy {
                chain_id: Some(167_001),
                nonce: 3,
                gas_price: 1_000_000_000,
                gas_limit: 100_000,
                to: TxKind::Call(Address::with_last_byte(0x22)),
                value: U256::from(5),
                input: Bytes::from(vec![0x01; 100]),
            },
            signature,
        )
        .into();
        let dynamic_fee: TransactionSigned = Signed::new_unhashed(
            TxEip1559 {
                chain_id: 167_001,
                nonce: 4,
                gas_limit: 200_000,
                max_fee_per_gas: 2_000_000_000,
                max_priority_fee_per_gas: 1_000_000,
                to: TxKind::Call(Address::with_last_byte(0x33)),
                value: U256::ZERO,
                access_list: Default::default(),
                input: Bytes::from(vec![0x02; 300]),
            },
            signature,
        )
        .into();

        // Block-body form: a legacy transaction is an RLP list, a typed one is an RLP string
        // wrapping its EIP-2718 bytes.
        assert_eq!(legacy.length(), legacy.eip2718_encoded_length());
        assert_eq!(
            dynamic_fee.length(),
            dynamic_fee.eip2718_encoded_length() +
                alloy_rlp::length_of_length(dynamic_fee.eip2718_encoded_length())
        );

        for txs in [vec![], vec![legacy.clone()], vec![legacy, dynamic_fee]] {
            let mut payload = Vec::new();
            header.encode(&mut payload);
            alloy_rlp::encode_list::<_, TransactionSigned>(&txs, &mut payload);
            let mut encoded = Vec::new();
            alloy_rlp::Header { list: true, payload_length: payload.len() }.encode(&mut encoded);
            encoded.extend_from_slice(&payload);

            assert_eq!(execution_block_length(&header, &txs), encoded.len());
            assert_eq!(
                execution_block_len(header.length(), txs.iter().map(Encodable::length).sum()),
                encoded.len()
            );

            let mut buf = encoded.as_slice();
            let outer = alloy_rlp::Header::decode(&mut buf).expect("executionBlock list header");
            assert!(outer.list, "executionBlock must be an RLP list");
            assert_eq!(outer.payload_length, buf.len());
            assert_eq!(Header::decode(&mut buf).expect("EL header"), header);
            assert_eq!(
                Vec::<TransactionSigned>::decode(&mut buf).expect("transactions in body form"),
                txs
            );
            assert!(buf.is_empty(), "executionBlock holds exactly [header, txs]");
        }
    }

    #[test]
    fn root_requirement_applies_only_to_non_genesis_etna_blocks() {
        assert!(validate_etna_root(false, 1, None).is_ok());
        assert!(validate_etna_root(true, 0, Some(B256::ZERO)).is_ok());
        assert!(validate_etna_root(true, 1, None).is_err());
        assert!(validate_etna_root(true, 1, Some(B256::ZERO)).is_err());
        assert!(validate_etna_root(true, 1, Some(B256::with_last_byte(1))).is_ok());
    }

    #[test]
    fn extra_data_requirement_applies_only_to_non_genesis_etna_blocks() {
        assert!(validate_etna_extra_data(false, 1, &[0; 7]).is_ok());
        assert!(validate_etna_extra_data(true, 0, &[]).is_ok());
        assert!(validate_etna_extra_data(true, 1, &[0; 13]).is_ok());
        for len in [0, 7, 12, 14, 32] {
            assert_eq!(
                validate_etna_extra_data(true, 1, &vec![0; len]),
                Err(InvalidEtnaExtraData { block_number: 1, len })
            );
        }
    }
}
