//! Invariants introduced by the opt-in Etna hardfork.

use crate::extra_data::{ETNA_BASEFEE_SHARING_PCTG, ETNA_EXTRA_DATA_LEN};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, address};
use alloy_rlp::{Encodable, length_of_length};
use core::fmt;
use reth_ethereum_primitives::TransactionSigned;

/// Smallest `maxBlockBytes` build attribute accepted on an Etna payload job.
///
/// A sanity floor on the driver-supplied block bound: a smaller value leaves little room beyond
/// the header, so it is rejected as an invalid parameter before any job starts.
pub const ETNA_MIN_MAX_BLOCK_BYTES: u64 = 1_024;

/// Consensus bound on an Etna executionBlock's length, in bytes, which lets every block fit in
/// one blob on its own.
///
/// The driver passes it to every Etna job as `maxBlockBytes`; the transaction pool uses it to
/// refuse transactions no Etna block can carry.
pub const ETNA_MAX_BLOCK_BYTES: u64 = 124_000;

/// JSON-RPC error code an `engine_forkchoiceUpdatedV3` with Etna build attributes returns when its
/// build failed and a read of the parent's state then reports that state pruned.
///
/// Only that FCU answers it; `engine_getPayloadV5` for the failed job keeps the unknown-payload
/// answer. Drivers treat this code as a definitive build failure; every other build failure answers
/// reth's internal error (`-32603`), which does not say whether a retry can succeed.
pub const ENGINE_CANNOT_BUILD_ON_PARENT_CODE: i32 = -38_100;

/// JSON-RPC error message paired with [`ENGINE_CANNOT_BUILD_ON_PARENT_CODE`].
pub const ENGINE_CANNOT_BUILD_ON_PARENT_MESSAGE: &str = "cannot build on parent";

/// Selector of `onBlockStart(uint48)`, `bytes4(keccak256("onBlockStart(uint48)"))`.
///
/// The execution layer calls this function on the `EtnaStaking` contract once at the start of every
/// non-genesis Etna block, after the EIP-2935 and EIP-4788 system writes and before the first
/// transaction.
pub const ETNA_BLOCK_START_SELECTOR: [u8; 4] = [0xaa, 0x28, 0xc3, 0x2e];

/// Length in bytes of the block-start call's calldata: [`ETNA_BLOCK_START_SELECTOR`], then
/// `generation` from the block's own `extraData` as one 32-byte big-endian ABI word with zero high
/// bits. No other bytes follow.
pub const ETNA_BLOCK_START_CALLDATA_LEN: usize = 36;

/// Gas available to the block-start call's frame.
///
/// No intrinsic gas is charged and no transaction gas cap applies, so the frame gets exactly this
/// amount. The call pays no fee, and neither the block's gasUsed nor its zk gas counts it.
pub const ETNA_BLOCK_START_CALL_GAS: u64 = 30_000_000;

/// Caller of the block-start call, the system address the EIP-4788 and EIP-2935 writes also use.
///
/// The call never changes this account's nonce or balance.
pub const ETNA_SYSTEM_CALLER: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");

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

/// Error returned when an activated non-genesis block breaks the Etna extra data layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidEtnaExtraData {
    /// The extra data is not the 13-byte `[basefeeSharingPctg | generation | anchorBlockNumber]`
    /// layout.
    Length {
        /// Height of the block whose extra data has the wrong length.
        block_number: u64,
        /// Actual extra data length in bytes.
        len: usize,
    },
    /// Byte 0, the base-fee sharing percentage, is not [`ETNA_BASEFEE_SHARING_PCTG`].
    BasefeeSharingPctg {
        /// Height of the block whose base-fee sharing percentage is wrong.
        block_number: u64,
        /// Base-fee sharing percentage the block carries.
        pctg: u8,
    },
}

impl fmt::Display for InvalidEtnaExtraData {
    /// Describes the required Etna layout and the observed violation.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { block_number, len } => write!(
                f,
                "Etna block {block_number} requires {ETNA_EXTRA_DATA_LEN}-byte extraData \
                 [basefeeSharingPctg | generation | anchorBlockNumber], got {len} bytes"
            ),
            Self::BasefeeSharingPctg { block_number, pctg } => write!(
                f,
                "Etna block {block_number} requires extraData basefeeSharingPctg \
                 {ETNA_BASEFEE_SHARING_PCTG}, got {pctg}"
            ),
        }
    }
}

impl core::error::Error for InvalidEtnaExtraData {}

/// Requires the 13-byte Etna extra data layout
/// `[basefeeSharingPctg | generation | anchorBlockNumber]` after Etna, except for the genesis
/// block.
///
/// Byte 0, the base-fee sharing percentage, is fixed at [`ETNA_BASEFEE_SHARING_PCTG`] (100), so the
/// beneficiary receives every base fee. The execution layer is the only component that applies the
/// percentage, and one above 100 would credit the beneficiary more base fee than the sender paid.
/// The other fields are not checked here.
pub fn validate_etna_extra_data(
    is_etna_active: bool,
    block_number: u64,
    extra_data: &[u8],
) -> Result<(), InvalidEtnaExtraData> {
    if !is_etna_active || block_number == 0 {
        return Ok(());
    }
    check_etna_extra_data_layout(block_number, extra_data)
}

/// Checks the two parts of the Etna extra data layout the execution layer validates: the length is
/// [`ETNA_EXTRA_DATA_LEN`] (13) and byte 0 is [`ETNA_BASEFEE_SHARING_PCTG`] (100).
///
/// Nothing else is checked: `generation` and `anchorBlockNumber` may hold any value.
/// `block_number` only labels the returned error. Header validation and
/// [`crate::extra_data::EtnaExtraData::decode`] share this check, so they accept the same bytes.
pub(crate) fn check_etna_extra_data_layout(
    block_number: u64,
    extra_data: &[u8],
) -> Result<(), InvalidEtnaExtraData> {
    if extra_data.len() != ETNA_EXTRA_DATA_LEN {
        return Err(InvalidEtnaExtraData::Length { block_number, len: extra_data.len() });
    }
    let pctg = extra_data[0];
    if pctg != ETNA_BASEFEE_SHARING_PCTG {
        return Err(InvalidEtnaExtraData::BasefeeSharingPctg { block_number, pctg });
    }
    Ok(())
}

/// Returns the executionBlock length, `rlp([header, [tx…]])` including its outer list header, or
/// `None` when it overflows `usize`.
///
/// `header_rlp_len` is the full RLP length of the EL header and `txs_payload_len` is the summed
/// [`etna_tx_length`] of the transactions (the transaction list's payload, without its header).
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

/// Returns [`checked_execution_block_len`], saturating at `usize::MAX`, which no in-memory encoding
/// reaches.
const fn execution_block_len(header_rlp_len: usize, txs_payload_len: usize) -> usize {
    match checked_execution_block_len(header_rlp_len, txs_payload_len) {
        Some(len) => len,
        None => usize::MAX,
    }
}

/// Returns the largest transaction list payload length whose executionBlock still fits in
/// `max_block_bytes`, or `None` when even an empty transaction list does not fit.
///
/// This is the exact inverse of the executionBlock length for a fixed header: for the returned
/// `s`, the executionBlock with an `s`-byte transaction list payload fits and one with `s + 1`
/// bytes does not, across every RLP list header size change. A limit above `usize::MAX` (only
/// possible on 32-bit targets) is treated as `usize::MAX`, since no larger encoding can exist in
/// memory.
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

/// Returns the number of bytes `tx` adds to an Etna executionBlock's transaction list: an RLP byte
/// string holding its EIP-2718 encoding, whatever its type.
///
/// This is how the consensus envelope and the published blob carry every transaction, and the
/// per-transaction measure behind `maxBlockBytes`. For a typed transaction it equals the
/// block-body [`Encodable::length`]. A legacy transaction counts the string header around its RLP
/// list as well, which the block-body form leaves out. An EIP-2718 encoding is never a single byte,
/// so the string always has a header.
pub fn etna_tx_length(tx: &TransactionSigned) -> usize {
    etna_tx_length_of_encoding(tx.eip2718_encoded_length())
}

/// Returns [`etna_tx_length`] for a transaction whose EIP-2718 encoding is `eip2718_len` bytes
/// long.
pub fn etna_tx_length_of_encoding(eip2718_len: usize) -> usize {
    alloy_rlp::Header { list: false, payload_length: eip2718_len }.length_with_payload()
}

/// Returns the exact length of the executionBlock `rlp([header, [tx…]])` for `header` and
/// `transactions`, including its outer list header.
///
/// Every transaction, legacy included, is measured as an RLP byte string holding its EIP-2718
/// bytes ([`etna_tx_length`]), the form the consensus envelope and the published blob carry.
pub fn execution_block_length(header: &Header, transactions: &[TransactionSigned]) -> usize {
    execution_block_len(header.length(), transactions.iter().map(etna_tx_length).sum())
}

#[cfg(test)]
mod tests {
    use super::{
        ETNA_MIN_MAX_BLOCK_BYTES, InvalidEtnaExtraData, checked_execution_block_len,
        etna_tx_length, execution_block_len, execution_block_length, max_txs_payload_len,
        validate_etna_extra_data, validate_etna_root,
    };
    use alloy_consensus::{Header, Signed, TxEip1559, TxLegacy};
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
    use alloy_rlp::Encodable;
    use reth_ethereum_primitives::TransactionSigned;
    use reth_primitives_traits::SignedTransaction;

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

    /// Returns an Etna-shaped EL header with a 13-byte extraData and Cancun-era fields set.
    fn sample_header() -> Header {
        Header {
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
        }
    }

    /// Returns a legacy transaction with 100 bytes of calldata and an EIP-1559 transaction with
    /// 300, both with a placeholder signature.
    fn sample_txs() -> (TransactionSigned, TransactionSigned) {
        let signature = Signature::new(U256::from(1), U256::from(2), false);
        let legacy = Signed::new_unhashed(
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
        let dynamic_fee = Signed::new_unhashed(
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
        (legacy, dynamic_fee)
    }

    /// Returns the EIP-2718 encoding of `tx`.
    fn eip2718<T: SignedTransaction>(tx: &T) -> Bytes {
        tx.encoded_2718().into()
    }

    /// Returns `rlp([header, [tx…]])` with the transactions in block-body form: a legacy
    /// transaction as its bare RLP list, a typed one as a byte string of its EIP-2718 bytes.
    fn block_body_form(header: &Header, txs: &[TransactionSigned]) -> Vec<u8> {
        let mut payload = Vec::new();
        header.encode(&mut payload);
        alloy_rlp::encode_list::<_, TransactionSigned>(txs, &mut payload);
        let mut encoded = Vec::new();
        alloy_rlp::Header { list: true, payload_length: payload.len() }.encode(&mut encoded);
        encoded.extend_from_slice(&payload);
        encoded
    }

    #[test]
    fn execution_block_length_matches_the_consensus_envelope_encoding() {
        let header = sample_header();
        let (legacy, dynamic_fee) = sample_txs();
        let legacy_string_header =
            alloy_rlp::Header { list: false, payload_length: legacy.eip2718_encoded_length() }
                .length();

        for (txs, legacy_count) in [
            (vec![], 0),
            (vec![legacy.clone()], 1),
            (vec![dynamic_fee.clone()], 0),
            (vec![legacy.clone(), dynamic_fee.clone(), legacy.clone()], 2),
        ] {
            // The consensus envelope's executionBlock: `[header, [tx…]]` with every transaction a
            // byte string of its EIP-2718 bytes.
            let raw: Vec<Bytes> = txs.iter().map(eip2718).collect();
            let mut payload = Vec::new();
            header.encode(&mut payload);
            raw.encode(&mut payload);
            let mut encoded = Vec::new();
            alloy_rlp::Header { list: true, payload_length: payload.len() }.encode(&mut encoded);
            encoded.extend_from_slice(&payload);

            assert_eq!(execution_block_length(&header, &txs), encoded.len());
            assert_eq!(
                execution_block_len(header.length(), txs.iter().map(etna_tx_length).sum()),
                encoded.len()
            );

            // Block-body form leaves out each legacy transaction's string header; no list header
            // changes size between the two forms at these lengths.
            assert_eq!(
                encoded.len() - block_body_form(&header, &txs).len(),
                legacy_count * legacy_string_header
            );
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
        let mut extra_data = [0; 13];
        extra_data[0] = 100;
        assert!(validate_etna_extra_data(true, 1, &extra_data).is_ok());
        for len in [0, 7, 12, 14, 29, 32] {
            assert_eq!(
                validate_etna_extra_data(true, 1, &vec![100; len]),
                Err(InvalidEtnaExtraData::Length { block_number: 1, len })
            );
        }
        assert_eq!(
            InvalidEtnaExtraData::Length { block_number: 1, len: 29 }.to_string(),
            "Etna block 1 requires 13-byte extraData \
             [basefeeSharingPctg | generation | anchorBlockNumber], got 29 bytes"
        );
        for pctg in [0, 99, 101, 255] {
            extra_data[0] = pctg;
            assert_eq!(
                validate_etna_extra_data(true, 1, &extra_data),
                Err(InvalidEtnaExtraData::BasefeeSharingPctg { block_number: 1, pctg })
            );
        }
    }
}
