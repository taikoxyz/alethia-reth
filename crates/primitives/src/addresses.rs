//! Taiko-specific addresses and address derivation helpers.
//!
//! Taiko Mainnet and Taiko Hoodi have no built-in `EtnaStaking` proxy address yet. The release
//! that schedules Etna on either chain pins that chain's address here.

use std::str::FromStr;

use alloy_primitives::{Address, hex};

/// System caller address used for Taiko anchor system-call pre-execution.
pub const TAIKO_GOLDEN_TOUCH_ADDRESS: [u8; 20] = hex!("0x0000777735367b36bc9b61c50022d9d0700db4ec");

/// Generates the network treasury address based on the chain ID.
#[inline]
pub fn get_treasury_address(chain_id: u64) -> Address {
    chain_constant_address(chain_id, 10001)
}

/// Returns the `EtnaStaking` proxy address built in for `chain_id`, or `None` when the chain has
/// none.
///
/// The first arm is Taiko Mainnet (167000) and Taiko Hoodi (167013), which have no address. Every
/// other chain id, custom chainspecs included, gets the chain's constant address with suffix
/// `10003`, where a new network's genesis predeploys the proxy.
pub fn etna_staking_address(chain_id: u64) -> Option<Address> {
    match chain_id {
        167_000 | 167_013 => None,
        _ => Some(chain_constant_address(chain_id, 10003)),
    }
}

/// Builds a per-chain constant address: the decimal digits of `chain_id`, then zero padding,
/// then the decimal digits of `suffix`, read as 40 hex characters.
///
/// Decimal digits are valid hex digits, and the longest `u64` and `u32` together take 30 of the
/// 40 characters, so every input yields an address.
fn chain_constant_address(chain_id: u64, suffix: u32) -> Address {
    let prefix = chain_id.to_string();
    let suffix = suffix.to_string();

    let total_len = 40;
    let padding_len = total_len - prefix.len() - suffix.len();
    let padding = "0".repeat(padding_len);

    let hex_str = format!("0x{prefix}{padding}{suffix}");

    Address::from_str(&hex_str)
        .expect("chain constant address generation should always produce a valid address")
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_get_treasury_address() {
        let treasury = get_treasury_address(167000);
        assert_eq!(
            treasury,
            Address::from_str("0x1670000000000000000000000000000000010001").unwrap()
        );
    }

    #[test]
    fn etna_staking_address_is_the_per_chain_constant_address() {
        use alloy_primitives::address;

        for (chain_id, expected) in [
            (167_001, address!("0x1670010000000000000000000000000000010003")),
            (167, address!("0x1670000000000000000000000000000000010003")),
            (1, address!("0x1000000000000000000000000000000000010003")),
            (u64::MAX, address!("0x1844674407370955161500000000000000010003")),
        ] {
            assert_eq!(etna_staking_address(chain_id), Some(expected), "chain {chain_id}");
        }
        for chain_id in [167_000, 167_013] {
            assert_eq!(etna_staking_address(chain_id), None, "chain {chain_id}");
        }
    }
}
