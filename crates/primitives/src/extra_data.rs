//! Helpers for decoding Taiko-specific block `extraData` fields.

use std::{error::Error, fmt::Display};

/// Index of the end-of-proposal flag in Shasta extra data.
pub const SHASTA_EXTRA_DATA_END_OF_PROPOSAL_INDEX: usize = 7;

/// Exact number of bytes required for Shasta extra data.
pub const SHASTA_EXTRA_DATA_LEN: usize = 8;

/// Error indicating that the Shasta extra data has an invalid length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShastaExtraDataError {
    /// The actual length of the provided extra data.
    pub got: usize,
    /// The expected length of the Shasta extra data.
    pub expected: usize,
}

impl Display for ShastaExtraDataError {
    /// Formats the error message indicating the invalid length of Shasta extra data.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid Shasta extra data length: {} != {}", self.got, self.expected)
    }
}

/// Implements the standard Error trait for ShastaExtraDataError.
impl Error for ShastaExtraDataError {}

/// Returns the base fee sharing percentage encoded in Shasta extra data.
pub fn decode_shasta_basefee_sharing_pctg(extra: &[u8]) -> u8 {
    extra.first().copied().unwrap_or_default()
}

/// Returns the proposal ID and end-of-proposal flag encoded in Shasta extra data.
pub fn decode_shasta_proposal_id(extra: &[u8]) -> Result<(u64, bool), ShastaExtraDataError> {
    if extra.len() != SHASTA_EXTRA_DATA_LEN {
        return Err(ShastaExtraDataError { got: extra.len(), expected: SHASTA_EXTRA_DATA_LEN });
    }

    let mut buf = [0u8; 8];
    buf[2..].copy_from_slice(&extra[1..7]);
    let proposal_id = u64::from_be_bytes(buf);
    let end_of_proposal = extra[SHASTA_EXTRA_DATA_END_OF_PROPOSAL_INDEX] != 0;
    Ok((proposal_id, end_of_proposal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_shasta_proposal_id_and_end_of_proposal() {
        let extra = [0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x01];
        let (proposal_id, end_of_proposal) = decode_shasta_proposal_id(&extra).unwrap();
        assert_eq!(proposal_id, 0x010203040506);
        assert!(end_of_proposal);
    }

    #[test]
    fn rejects_invalid_shasta_extra_data_len() {
        let err = decode_shasta_proposal_id(&[0x01, 0x02, 0x03]).unwrap_err();
        assert_eq!(err.expected, SHASTA_EXTRA_DATA_LEN);
        assert_eq!(err.got, 3);
    }
}
