//! Invariants introduced by the opt-in Etna hardfork.

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

#[cfg(test)]
mod tests {
    use super::validate_etna_root;
    use alloy_primitives::B256;

    #[test]
    fn root_requirement_applies_only_to_non_genesis_etna_blocks() {
        assert!(validate_etna_root(false, 1, None).is_ok());
        assert!(validate_etna_root(true, 0, Some(B256::ZERO)).is_ok());
        assert!(validate_etna_root(true, 1, None).is_err());
        assert!(validate_etna_root(true, 1, Some(B256::ZERO)).is_err());
        assert!(validate_etna_root(true, 1, Some(B256::with_last_byte(1))).is_ok());
    }
}
