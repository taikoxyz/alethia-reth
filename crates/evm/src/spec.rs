use core::str::FromStr;
use reth::revm::primitives::hardfork::{SpecId, UnknownHardfork};

#[repr(u8)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[allow(non_camel_case_types)]
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
            Self::GENESIS | Self::ONTAKE | Self::PACAYA | Self::SHASTA => SpecId::SHANGHAI,
        }
    }

    pub const fn is_enabled_in(self, other: TaikoSpecId) -> bool {
        other as u8 <= self as u8
    }
}

impl From<TaikoSpecId> for SpecId {
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
    pub const GENESIS: &str = "Genesis";
    pub const ONTAKE: &str = "Ontake";
    pub const PACAYA: &str = "Pacaya";
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
                (SpecId::CANCUN, false),
                (SpecId::default(), false),
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
