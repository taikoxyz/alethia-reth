//! Taiko network constants and address utilities.

use std::str::FromStr;

use alloy_primitives::{Address, hex};

/// System caller address used for Taiko anchor system-call pre-execution.
pub const TAIKO_GOLDEN_TOUCH_ADDRESS: [u8; 20] = hex!("0x0000777735367b36bc9b61c50022d9d0700db4ec");

/// Generates the network treasury address based on the chain ID.
#[inline]
pub fn get_treasury_address(chain_id: u64) -> Address {
    let prefix = chain_id.to_string();
    let suffix = "10001";

    let total_len = 40;
    let padding_len = total_len - prefix.len() - suffix.len();
    let padding = "0".repeat(padding_len);

    let hex_str = format!("0x{prefix}{padding}{suffix}");

    Address::from_str(&hex_str)
        .expect("treasury address generation should always produce valid address")
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
}
