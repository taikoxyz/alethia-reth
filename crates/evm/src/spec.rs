//! Taiko hardfork spec identifiers for EVM feature gating.
use core::str::FromStr;
use reth_revm::primitives::hardfork::{SpecId, UnknownHardfork};

#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[allow(non_camel_case_types)]
/// Taiko network hardfork identifiers ordered by activation.
pub enum TaikoSpecId {
    /// Genesis chain spec for the Taiko network (pre-Ontake fork)
    GENESIS,
    /// Ontake hard fork for the Taiko network
    ONTAKE,
    /// Pacaya hard fork for the Taiko network
    #[default]
    PACAYA,
    /// Shasta hard fork for the Taiko network
    SHASTA,
}

impl TaikoSpecId {
    /// Converts the [`TaikoSpecId`] into a [`SpecId`].
    pub const fn into_eth_spec(self) -> SpecId {
        match self {
            Self::GENESIS | Self::ONTAKE | Self::PACAYA | Self::SHASTA => SpecId::PRAGUE,
        }
    }

    /// Return whether `self` is enabled when running under `other`.
    pub const fn is_enabled_in(self, other: TaikoSpecId) -> bool {
        other as u8 <= self as u8
    }
}

impl From<TaikoSpecId> for SpecId {
    /// Converts a Taiko hardfork spec id into the corresponding Ethereum spec id.
    fn from(spec: TaikoSpecId) -> Self {
        spec.into_eth_spec()
    }
}

impl FromStr for TaikoSpecId {
    type Err = UnknownHardfork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            name::GENESIS => Ok(TaikoSpecId::GENESIS),
            name::ONTAKE => Ok(TaikoSpecId::ONTAKE),
            name::PACAYA => Ok(TaikoSpecId::PACAYA),
            name::SHASTA => Ok(TaikoSpecId::SHASTA),
            _ => Err(UnknownHardfork),
        }
    }
}

impl From<TaikoSpecId> for &'static str {
    /// Converts a Taiko hardfork spec id into its canonical string identifier.
    fn from(spec_id: TaikoSpecId) -> Self {
        match spec_id {
            TaikoSpecId::GENESIS => name::GENESIS,
            TaikoSpecId::ONTAKE => name::ONTAKE,
            TaikoSpecId::PACAYA => name::PACAYA,
            TaikoSpecId::SHASTA => name::SHASTA,
        }
    }
}

/// String identifiers for Taiko hardforks
pub mod name {
    /// String name for `TaikoSpecId::GENESIS`.
    pub const GENESIS: &str = "Genesis";
    /// String name for `TaikoSpecId::ONTAKE`.
    pub const ONTAKE: &str = "Ontake";
    /// String name for `TaikoSpecId::PACAYA`.
    pub const PACAYA: &str = "Pacaya";
    /// String name for `TaikoSpecId::SHASTA`.
    pub const SHASTA: &str = "Shasta";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    #[test]
    fn test_taiko_spec_id_eth_spec_compatibility() {
        // Define test cases: (TaikoSpecId, enabled in ETH specs, enabled in Taiko specs)
        let test_cases = [(
            TaikoSpecId::PACAYA,
            vec![
                (SpecId::MERGE, true),
                (SpecId::SHANGHAI, true),
                (SpecId::CANCUN, true),
                (SpecId::PRAGUE, true),
            ],
            vec![
                (TaikoSpecId::GENESIS, true),
                (TaikoSpecId::ONTAKE, true),
                (TaikoSpecId::PACAYA, true),
                (TaikoSpecId::SHASTA, false),
            ],
        )];

        for (taiko_spec, eth_tests, taiko_tests) in test_cases {
            // Test ETH spec compatibility
            for (eth_spec, expected) in eth_tests {
                assert_eq!(
                    taiko_spec.into_eth_spec().is_enabled_in(eth_spec),
                    expected,
                    "{:?} should {} be enabled in ETH {:?}",
                    taiko_spec,
                    if expected { "" } else { "not " },
                    eth_spec
                );
            }

            // Test Taiko spec compatibility
            for (other_taiko_spec, expected) in taiko_tests {
                assert_eq!(
                    taiko_spec.is_enabled_in(other_taiko_spec),
                    expected,
                    "{:?} should {} be enabled in TAIKO {:?}",
                    taiko_spec,
                    if expected { "" } else { "not " },
                    other_taiko_spec
                );
            }
        }
    }
}
