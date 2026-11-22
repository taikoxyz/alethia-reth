//! Helpers for encoding and decoding Taiko-specific block `extraData` fields.

use crate::etna::{
    ETNA_BLOCK_START_CALLDATA_LEN, ETNA_BLOCK_START_SELECTOR, check_etna_extra_data_layout,
};

/// Exact length of the Shasta extra data layout: `[basefeeSharingPctg | proposalId(6)]`.
/// Shasta and Unzen header validation rejects any other length.
pub const SHASTA_EXTRA_DATA_LEN: usize = 7;

/// Exact length of the non-genesis Etna extra data layout:
/// `[basefeeSharingPctg | generation(6) | anchorBlockNumber(6) | missBitmap(16)]`, with
/// `generation` and `anchorBlockNumber` `uint48` and `missBitmap` `uint128`, all big-endian.
/// The execution layer validates only the length and byte 0. It decodes `generation` and
/// `missBitmap` to build the block-start call's calldata ([`EtnaExtraData`]) but does not validate
/// them; derivation and provers check every field after byte 0.
pub const ETNA_EXTRA_DATA_LEN: usize = 29;

/// Base-fee sharing percentage every non-genesis Etna block carries in `extraData[0]`
/// (SPEC CONS-3.2-5): the beneficiary receives all base fees and the treasury none.
pub const ETNA_BASEFEE_SHARING_PCTG: u8 = 100;

/// Fields of a non-genesis Etna block's 29-byte `extraData`.
///
/// The layout, all big-endian, is byte 0 = [`ETNA_BASEFEE_SHARING_PCTG`] (100), bytes 1..7 =
/// `uint48 generation`, bytes 7..13 = `uint48 anchorBlockNumber` and bytes 13..29 =
/// `uint128 missBitmap`. Bit `s` of `missBitmap` (`(missBitmap >> s) & 1`) is seat `s`: seat 0 is
/// the low bit of byte 28 and seat 127 the high bit of byte 13.
///
/// Both `uint48` fields are always below 2^48, so [`Self::encode`] is lossless and
/// `decode(encode(x)) == Some(x)`. Byte 0 is implied by the type. Neither constructor checks the
/// field values against the chain: the execution layer validates only the length and byte 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EtnaExtraData {
    /// Committee generation, `uint48` at bytes 1..7; below 2^48.
    generation: u64,
    /// Number of the L1 block the block anchors to, `uint48` at bytes 7..13; below 2^48.
    anchor_block_number: u64,
    /// Missed-signature bitmap, `uint128` at bytes 13..29; bit `s` is seat `s`.
    miss_bitmap: u128,
}

impl EtnaExtraData {
    /// Returns the fields, or `None` when `generation` or `anchor_block_number` does not fit in 48
    /// bits (is 2^48 or more). Every `miss_bitmap` value is accepted.
    pub const fn new(generation: u64, anchor_block_number: u64, miss_bitmap: u128) -> Option<Self> {
        if generation >> 48 != 0 || anchor_block_number >> 48 != 0 {
            return None;
        }
        Some(Self { generation, anchor_block_number, miss_bitmap })
    }

    /// Decodes a non-genesis Etna block's `extraData`.
    ///
    /// Returns `None` unless `extra_data` is exactly [`ETNA_EXTRA_DATA_LEN`] bytes with byte 0
    /// equal to [`ETNA_BASEFEE_SHARING_PCTG`], the same check header validation applies. Any value
    /// of the other fields decodes.
    pub fn decode(extra_data: &[u8]) -> Option<Self> {
        // The block number only labels the error, which is discarded.
        check_etna_extra_data_layout(0, extra_data).ok()?;
        let mut miss_bitmap = [0u8; 16];
        miss_bitmap.copy_from_slice(&extra_data[13..ETNA_EXTRA_DATA_LEN]);
        Some(Self {
            generation: read_u48(&extra_data[1..7]),
            anchor_block_number: read_u48(&extra_data[7..13]),
            miss_bitmap: u128::from_be_bytes(miss_bitmap),
        })
    }

    /// Returns the 29-byte `extraData` that carries these fields, with byte 0 set to
    /// [`ETNA_BASEFEE_SHARING_PCTG`]. [`Self::decode`] returns `Some(*self)` for it.
    pub fn encode(&self) -> [u8; ETNA_EXTRA_DATA_LEN] {
        let mut extra_data = [0u8; ETNA_EXTRA_DATA_LEN];
        extra_data[0] = ETNA_BASEFEE_SHARING_PCTG;
        extra_data[1..7].copy_from_slice(&self.generation.to_be_bytes()[2..]);
        extra_data[7..13].copy_from_slice(&self.anchor_block_number.to_be_bytes()[2..]);
        extra_data[13..].copy_from_slice(&self.miss_bitmap.to_be_bytes());
        extra_data
    }

    /// Returns the committee generation (bytes 1..7), below 2^48.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the number of the L1 block the block anchors to (bytes 7..13), below 2^48.
    pub const fn anchor_block_number(&self) -> u64 {
        self.anchor_block_number
    }

    /// Returns the missed-signature bitmap (bytes 13..29); bit `s` is seat `s`.
    pub const fn miss_bitmap(&self) -> u128 {
        self.miss_bitmap
    }

    /// Returns the calldata of the block's `onBlockStart(uint48 generation, uint128 missBitmap)`
    /// call: [`ETNA_BLOCK_START_SELECTOR`] at bytes 0..4, then `generation` as the 32-byte word at
    /// bytes 4..36 and `missBitmap` as the word at bytes 36..68, each big-endian with zero high
    /// bits (the standard ABI encoding).
    ///
    /// The anchor block number is not an argument, so two values that differ only in it give the
    /// same calldata.
    pub fn on_block_start_calldata(&self) -> [u8; ETNA_BLOCK_START_CALLDATA_LEN] {
        let mut calldata = [0u8; ETNA_BLOCK_START_CALLDATA_LEN];
        calldata[..4].copy_from_slice(&ETNA_BLOCK_START_SELECTOR);
        calldata[28..36].copy_from_slice(&self.generation.to_be_bytes());
        calldata[52..68].copy_from_slice(&self.miss_bitmap.to_be_bytes());
        calldata
    }
}

/// Returns the big-endian `uint48` held in `bytes`, which must be exactly 6 bytes long.
fn read_u48(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word[2..].copy_from_slice(bytes);
    u64::from_be_bytes(word)
}

/// Returns the base fee sharing percentage encoded in Shasta extra data.
pub fn decode_shasta_basefee_sharing_pctg(extra: &[u8]) -> u8 {
    extra.first().copied().unwrap_or_default()
}

/// Returns the proposal ID encoded in Shasta extra data (bytes 1..6, big-endian).
pub fn decode_shasta_proposal_id(extra: &[u8]) -> Option<u64> {
    if extra.len() < SHASTA_EXTRA_DATA_LEN {
        return None;
    }

    let mut buf = [0u8; 8];
    buf[2..].copy_from_slice(&extra[1..7]);
    Some(u64::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::{ETNA_EXTRA_DATA_LEN, EtnaExtraData};
    use crate::etna::ETNA_BLOCK_START_SELECTOR;
    use alloy_primitives::{hex, keccak256};

    #[test]
    fn etna_extra_data_encodes_the_29_byte_layout() {
        let x = EtnaExtraData::new(1, 258, (1 << 127) + 1).unwrap();
        assert_eq!(x.encode(), hex!("6400000000000100000000010280000000000000000000000000000001"));
        assert_eq!(EtnaExtraData::decode(&x.encode()), Some(x));

        let zero = EtnaExtraData::new(0, 0, 0).unwrap();
        let mut expected = [0u8; ETNA_EXTRA_DATA_LEN];
        expected[0] = 0x64;
        assert_eq!(zero.encode(), expected);
        assert_eq!(EtnaExtraData::decode(&zero.encode()), Some(zero));
    }

    #[test]
    fn etna_extra_data_decode_is_strict() {
        for len in [13, 28, 30] {
            assert_eq!(EtnaExtraData::decode(&vec![100; len]), None, "{len} bytes must not decode");
        }
        for pctg in [0x65, 99] {
            let mut extra_data = [0u8; ETNA_EXTRA_DATA_LEN];
            extra_data[0] = pctg;
            assert_eq!(EtnaExtraData::decode(&extra_data), None, "byte 0 = {pctg} must not decode");
        }
    }

    #[test]
    fn etna_extra_data_rejects_fields_wider_than_48_bits() {
        assert_eq!(EtnaExtraData::new(1 << 48, 0, 0), None);
        assert_eq!(EtnaExtraData::new(0, 1 << 48, 0), None);
        let widest = EtnaExtraData::new((1 << 48) - 1, (1 << 48) - 1, u128::MAX).unwrap();
        assert_eq!(widest.generation(), (1 << 48) - 1);
        assert_eq!(widest.anchor_block_number(), (1 << 48) - 1);
        assert_eq!(widest.miss_bitmap(), u128::MAX);
    }

    #[test]
    fn miss_bitmap_bit_s_is_seat_s() {
        let seats_0_and_2 = EtnaExtraData::new(0, 0, 5).unwrap().encode();
        assert_eq!(seats_0_and_2[28], 0x05);
        assert!(seats_0_and_2[13..28].iter().all(|byte| *byte == 0));

        let seat_127 = EtnaExtraData::new(0, 0, 1 << 127).unwrap().encode();
        assert_eq!(seat_127[13], 0x80);
        assert_eq!(seat_127[28], 0);

        let mut last_byte = [0u8; ETNA_EXTRA_DATA_LEN];
        last_byte[0] = 100;
        last_byte[28] = 1;
        assert_eq!(EtnaExtraData::decode(&last_byte).unwrap().miss_bitmap(), 1);

        let mut first_bitmap_byte = [0u8; ETNA_EXTRA_DATA_LEN];
        first_bitmap_byte[0] = 100;
        first_bitmap_byte[13] = 1;
        assert_eq!(EtnaExtraData::decode(&first_bitmap_byte).unwrap().miss_bitmap(), 1 << 120);
    }

    #[test]
    fn on_block_start_calldata_is_the_abi_encoding() {
        // `cast calldata "onBlockStart(uint48,uint128)" 1 5`.
        let calldata = EtnaExtraData::new(1, 0, 5).unwrap().on_block_start_calldata();
        assert_eq!(
            calldata,
            hex!(
                "ea987592"
                "0000000000000000000000000000000000000000000000000000000000000001"
                "0000000000000000000000000000000000000000000000000000000000000005"
            )
        );
        assert_eq!(calldata.len(), 68);

        assert_eq!(ETNA_BLOCK_START_SELECTOR, keccak256("onBlockStart(uint48,uint128)")[..4]);

        // The generation word comes first, then the missBitmap word, each with zero high bits.
        let widest = EtnaExtraData::new((1 << 48) - 1, 7, u128::MAX).unwrap();
        let calldata = widest.on_block_start_calldata();
        assert!(calldata[4..30].iter().all(|byte| *byte == 0));
        assert!(calldata[30..36].iter().all(|byte| *byte == 0xff));
        assert!(calldata[36..52].iter().all(|byte| *byte == 0));
        assert!(calldata[52..68].iter().all(|byte| *byte == 0xff));

        // The anchor number is not an argument of the call.
        assert_eq!(
            EtnaExtraData::new(3, 0, 9).unwrap().on_block_start_calldata(),
            EtnaExtraData::new(3, (1 << 48) - 1, 9).unwrap().on_block_start_calldata()
        );
    }
}
