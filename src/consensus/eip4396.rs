use std::cmp::min;

use reth_primitives_traits::BlockHeader;

/// The initial base fee for the Shasta fork, which is set to 0.025 Gwei.
pub const SHASTA_INITIAL_BASE_FEE: u64 = 25_000_000;
pub const BASE_FEE_MAX_CHANGE_DENOMINATOR: u128 = 8;
pub const MAX_GAS_TARGET_PERCENT: u64 = 95;
pub const ELASTICITY_MULTIPLIER: u64 = 2;
pub const BLOCK_TIME_TARGET: u64 = 2;

/// Calculate the base fee for the next block based on the EIP-4396 specification.
/// https://github.com/ethereum/EIPs/blob/master/EIPS/eip-4396.md
///
/// This implementation follows the EIP-4396 formula which adjusts the base fee
/// based on parent block gas usage and block time.
///
/// # Arguments
/// * `parent` - The parent block header
/// * `parent_block_time` - The time between the parent block and its parent (in seconds)
///
/// # Returns
/// The calculated base fee for the next block
pub fn calculate_next_block_eip4396_base_fee<H: BlockHeader>(
    parent: &H,
    parent_block_time: u64,
) -> u64 {
    let parent_gas_limit = parent.gas_limit();
    let parent_base_gas_target = parent_gas_limit / ELASTICITY_MULTIPLIER;
    let parent_adjusted_gas_target = min(
        parent_base_gas_target * parent_block_time / BLOCK_TIME_TARGET,
        parent_gas_limit * MAX_GAS_TARGET_PERCENT / 100,
    );
    let parent_base_fee_per_gas =
        parent.base_fee_per_gas().expect("parent block must have base fee");
    let parent_gas_used = parent.gas_used();

    // Compare with adjusted gas target (not the base target)
    match parent_gas_used.cmp(&parent_adjusted_gas_target) {
        // If gas used equals adjusted target, base fee remains the same
        core::cmp::Ordering::Equal => parent_base_fee_per_gas,

        // If gas used is greater than adjusted target, increase base fee
        core::cmp::Ordering::Greater => {
            let gas_used_delta = parent_gas_used - parent_adjusted_gas_target;
            let base_fee_per_gas_delta = core::cmp::max(
                parent_base_fee_per_gas as u128 * gas_used_delta as u128
                    / parent_base_gas_target as u128
                    / BASE_FEE_MAX_CHANGE_DENOMINATOR,
                1,
            ) as u64;
            parent_base_fee_per_gas + base_fee_per_gas_delta
        }

        // If gas used is less than adjusted target, decrease base fee
        core::cmp::Ordering::Less => {
            let gas_used_delta = parent_adjusted_gas_target - parent_gas_used;
            let base_fee_per_gas_delta = (parent_base_fee_per_gas as u128 * gas_used_delta as u128
                / parent_base_gas_target as u128
                / BASE_FEE_MAX_CHANGE_DENOMINATOR) as u64;
            parent_base_fee_per_gas.saturating_sub(base_fee_per_gas_delta)
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;

    use super::*;

    #[test]
    fn test_calculate_next_block_eip4396_base_fee() {
        let mut parent = Header::default();
        parent.gas_limit = 30_000_000;
        parent.base_fee_per_gas = Some(1_000_000_000);

        // Test 1: Gas used equals adjusted target with standard block time
        // Adjusted target = 15_000_000 * 2 / 2 = 15_000_000
        parent.gas_used = 15_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, BLOCK_TIME_TARGET);
        assert_eq!(
            base_fee, 1_000_000_000,
            "Base fee should remain the same when at adjusted target"
        );

        // Test 2: Gas used above adjusted target
        parent.gas_used = 20_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, BLOCK_TIME_TARGET);
        assert!(base_fee > 1_000_000_000, "Base fee should increase when above adjusted target");

        // Test 3: Gas used below adjusted target
        parent.gas_used = 10_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, BLOCK_TIME_TARGET);
        assert!(base_fee < 1_000_000_000, "Base fee should decrease when below adjusted target");

        // Test 4: Edge case - zero gas used
        parent.gas_used = 0;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, BLOCK_TIME_TARGET);
        assert!(base_fee < 1_000_000_000, "Base fee should decrease with zero gas used");

        // Test 5: Different block times affecting adjusted target
        parent.gas_used = 15_000_000;

        // With 1 second block time: adjusted_target = 15_000_000 * 1 / 2 = 7_500_000
        // gas_used (15_000_000) > adjusted_target (7_500_000), so base fee increases
        let base_fee_1s = calculate_next_block_eip4396_base_fee(&parent, 1);
        assert!(base_fee_1s > 1_000_000_000, "Base fee should increase with shorter block time");

        // With 4 seconds block time: adjusted_target = min(15_000_000 * 4 / 2, 30_000_000 * 0.95) =
        // min(30_000_000, 28_500_000) = 28_500_000 gas_used (15_000_000) < adjusted_target
        // (28_500_000), so base fee decreases
        let base_fee_4s = calculate_next_block_eip4396_base_fee(&parent, 4);
        assert!(base_fee_4s < 1_000_000_000, "Base fee should decrease with longer block time");

        // Test 6: Verify exact calculation
        parent.gas_used = 16_000_000;
        parent.base_fee_per_gas = Some(1_000_000_000);
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, 2);
        // gas_used_delta = 16_000_000 - 15_000_000 = 1_000_000
        // base_fee_delta = max(1_000_000_000 * 1_000_000 / 15_000_000 / 8, 1) = max(8333333, 1) =
        // 8333333
        let expected = 1_000_000_000 + 8_333_333;
        assert_eq!(base_fee, expected, "Base fee calculation should match expected value");
    }
}
