//! Helpers for decoding Taiko-specific block `extraData` fields.

/// Minimum number of bytes required for Shasta extra data.
pub const SHASTA_EXTRA_DATA_LEN: usize = 7;

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
