use std::cmp::min;

use reth_primitives_traits::BlockHeader;

/// The initial base fee for the Shasta fork, which is set to 0.025 Gwei.
pub const SHASTA_INITIAL_BASE_FEE: u64 = 25_000_000;

/// EIP-4396 calculation constants.
pub const BASE_FEE_MAX_CHANGE_DENOMINATOR: u128 = 8;
pub const MAX_GAS_TARGET_PERCENT: u64 = 95;
pub const ELASTICITY_MULTIPLIER: u64 = 2;
pub const BLOCK_TIME_TARGET: u64 = 2;

/// The minimum base fee (inclusive) after the Shasta fork.
pub const MIN_BASE_FEE: u64 = 5_000_000; // 0.005 Gwei
/// The maximum base fee (inclusive) after the Shasta fork.
pub const MAX_BASE_FEE: u64 = 1_000_000_000; // 1 Gwei

/// Calculate the base fee for the next block based on the EIP-4396 specification.
/// https://github.com/ethereum/EIPs/blob/master/EIPS/eip-4396.md
///
/// This implementation follows the EIP-4396 formula which adjusts the base fee
/// based on parent block gas usage and block time.
///
/// # Arguments
/// * `parent` - The parent block header
/// * `parent_block_time` - The time between the parent block and its parent (in seconds)
/// * `parent_base_fee_per_gas` - The parent block base fee per gas (validated by caller)
///
/// # Returns
/// The calculated base fee for the next block
pub fn calculate_next_block_eip4396_base_fee<H: BlockHeader>(
    parent: &H,
    parent_block_time: u64,
    parent_base_fee_per_gas: u64,
) -> u64 {
    // First post-genesis block lacks a grandparent timestamp, so keep the default base
    // fee.
    if parent.number() == 0 {
        return SHASTA_INITIAL_BASE_FEE;
    }
    let parent_gas_limit = parent.gas_limit();
    let parent_base_gas_target = parent_gas_limit / ELASTICITY_MULTIPLIER;
    let parent_adjusted_gas_target = min(
        parent_base_gas_target * parent_block_time / BLOCK_TIME_TARGET,
        parent_gas_limit * MAX_GAS_TARGET_PERCENT / 100,
    );
    let parent_gas_used = parent.gas_used();
    let mut base_fee = parent_base_fee_per_gas;

    // Compare with adjusted gas target (not the base target)
    match parent_gas_used.cmp(&parent_adjusted_gas_target) {
        // If gas used equals adjusted target, base fee remains the same
        core::cmp::Ordering::Equal => {}

        // If gas used is greater than adjusted target, increase base fee
        core::cmp::Ordering::Greater => {
            let gas_used_delta = parent_gas_used - parent_adjusted_gas_target;
            let base_fee_per_gas_delta = core::cmp::max(
                parent_base_fee_per_gas as u128 * gas_used_delta as u128 /
                    parent_base_gas_target as u128 /
                    BASE_FEE_MAX_CHANGE_DENOMINATOR,
                1,
            ) as u64;
            base_fee = base_fee.saturating_add(base_fee_per_gas_delta);
        }

        // If gas used is less than adjusted target, decrease base fee
        core::cmp::Ordering::Less => {
            let gas_used_delta = parent_adjusted_gas_target - parent_gas_used;
            let base_fee_per_gas_delta = (parent_base_fee_per_gas as u128 * gas_used_delta as u128 /
                parent_base_gas_target as u128 /
                BASE_FEE_MAX_CHANGE_DENOMINATOR) as u64;
            base_fee = base_fee.saturating_sub(base_fee_per_gas_delta);
        }
    }

    clamp_shasta_base_fee(base_fee)
}

/// Clamp the base fee to be within the defined minimum and maximum limits for the Shasta blocks.
fn clamp_shasta_base_fee(base_fee: u64) -> u64 {
    base_fee.clamp(MIN_BASE_FEE, MAX_BASE_FEE)
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;

    use super::*;

    #[test]
    fn test_calculate_next_block_eip4396_base_fee() {
        let mut parent = Header {
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(900_000_000),
            number: 1,
            ..Default::default()
        };

        // Test 1: Gas used equals adjusted target with standard block time
        // Adjusted target = 15_000_000 * 2 / 2 = 15_000_000
        parent.gas_used = 15_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        assert_eq!(
            base_fee, 900_000_000,
            "Base fee should remain the same when at adjusted target"
        );

        // Test 2: Gas used above adjusted target
        parent.gas_used = 20_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        assert_eq!(base_fee, 937_500_000, "Base fee should increase when above adjusted target");

        // Test 3: Gas used below adjusted target
        parent.gas_used = 10_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        assert_eq!(base_fee, 862_500_000, "Base fee should decrease when below adjusted target");

        // Test 4: Edge case - zero gas used
        parent.gas_used = 0;
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        assert_eq!(base_fee, 787_500_000, "Base fee should decrease with zero gas used");

        // Test 5: Different block times affecting adjusted target
        parent.gas_used = 15_000_000;

        // With 1 second block time: adjusted_target = 15_000_000 * 1 / 2 = 7_500_000
        // gas_used (15_000_000) > adjusted_target (7_500_000), so base fee increases
        let base_fee_1s = calculate_next_block_eip4396_base_fee(
            &parent,
            1,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        assert_eq!(base_fee_1s, 956_250_000, "Base fee should increase with shorter block time");

        // With 4 seconds block time: adjusted_target = min(15_000_000 * 4 / 2, 30_000_000 * 0.95) =
        // min(30_000_000, 28_500_000) = 28_500_000 gas_used (15_000_000) < adjusted_target
        // (28_500_000), so base fee decreases
        let base_fee_4s = calculate_next_block_eip4396_base_fee(
            &parent,
            4,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        assert_eq!(base_fee_4s, 798_750_000, "Base fee should decrease with longer block time");

        // Test 6: Verify exact calculation
        parent.gas_used = 16_000_000;
        parent.base_fee_per_gas = Some(900_000_000);
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            2,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        // gas_used_delta = 16_000_000 - 15_000_000 = 1_000_000
        // base_fee_delta = max(900_000_000 * 1_000_000 / 15_000_000 / 8, 1) = max(7_500_000, 1) =
        // 7_500_000
        let expected = 900_000_000 + 7_500_000;
        assert_eq!(base_fee, expected, "Base fee calculation should match expected value");
    }

    #[test]
    fn test_calculate_next_block_eip4396_base_fee_clamp() {
        let mut parent = Header {
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(MAX_BASE_FEE),
            number: 1,
            ..Default::default()
        };

        parent.gas_used = 16_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        assert_eq!(base_fee, MAX_BASE_FEE, "Base fee should not exceed MAX_BASE_FEE");

        parent.base_fee_per_gas = Some(MIN_BASE_FEE);
        parent.gas_used = 14_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        assert_eq!(base_fee, MIN_BASE_FEE, "Base fee should not go below MIN_BASE_FEE");
    }

    #[test]
    fn test_calculate_next_block_eip4396_base_fee_first_block() {
        let parent = Header {
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(900_000_000),
            ..Default::default()
        };

        let base_fee = calculate_next_block_eip4396_base_fee(
            &parent,
            BLOCK_TIME_TARGET,
            parent.base_fee_per_gas.expect("parent base fee set"),
        );
        assert_eq!(
            base_fee, SHASTA_INITIAL_BASE_FEE,
            "First post-genesis block should use the default Shasta base fee"
        );
    }
}
