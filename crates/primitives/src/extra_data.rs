//! Helpers for decoding Taiko-specific block `extraData` fields.

/// Exact length of the Shasta extra data layout: `[basefeeSharingPctg | proposalId(6)]`.
/// Shasta and Unzen header validation rejects any other length.
pub const SHASTA_EXTRA_DATA_LEN: usize = 7;

/// Exact length of the non-genesis Etna extra data layout:
/// `[basefeeSharingPctg | proposalId(6) | anchorBlockNumber(6)]`, both `uint48` big-endian.
/// The execution layer checks only the length; derivation and provers own the anchor number.
pub const ETNA_EXTRA_DATA_LEN: usize = 13;

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
