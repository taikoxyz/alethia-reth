//! Invariants introduced by the opt-in Etna hardfork.

use crate::extra_data::ETNA_EXTRA_DATA_LEN;
use alloy_primitives::B256;
use core::fmt;

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

#[cfg(test)]
mod tests {
    use super::{InvalidEtnaExtraData, validate_etna_extra_data, validate_etna_root};
    use alloy_primitives::B256;

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
