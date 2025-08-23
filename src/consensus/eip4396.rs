use std::cmp::min;

use alloy_eips::eip1559::DEFAULT_ELASTICITY_MULTIPLIER;
use reth_primitives_traits::BlockHeader;

/// The initial base fee for the Shasta fork, which is set to 0.025 Gwei.
pub const SHASTA_INITIAL_BASE_FEE: u64 = 25_000_000;
pub const BASE_FEE_MAX_CHANGE_DENOMINATOR: u128 = 8;
pub const MAX_GAS_TARGET_PERCENT: u64 = 95;
pub const BLOCK_TIME_TARGET: u64 = 2;

/// Calculate the base fee for the next block based on the EIP-4396 specification.
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
    // Calculate the target gas by dividing the gas limit by the elasticity multiplier.
    let gas_target = parent.gas_limit() / DEFAULT_ELASTICITY_MULTIPLIER;
    let gas_target_adjusted = min(
        gas_target * parent_block_time / BLOCK_TIME_TARGET,
        parent.gas_limit() * MAX_GAS_TARGET_PERCENT / 100,
    );
    let parent_base_fee = parent.base_fee_per_gas().unwrap();

    match parent.gas_used().cmp(&gas_target) {
        // If the gas used in the current block is equal to the gas target, the base fee remains the
        // same (no increase).
        core::cmp::Ordering::Equal => parent_base_fee,
        // If the gas used in the current block is greater than the gas target, calculate a new
        // increased base fee.
        core::cmp::Ordering::Greater => {
            // Calculate the increase in base fee based on the formula defined by EIP-4396.
            parent_base_fee
                + (core::cmp::max(
                    // Ensure a minimum increase of 1.
                    1,
                    parent_base_fee as u128 * (parent.gas_used() - gas_target_adjusted) as u128
                        / (gas_target as u128 * BASE_FEE_MAX_CHANGE_DENOMINATOR),
                ) as u64)
        }
        // If the gas used in the current block is less than the gas target, calculate a new
        // decreased base fee.
        core::cmp::Ordering::Less => {
            // Calculate the decrease in base fee based on the formula defined by EIP-4396.
            // Use saturating_sub to avoid underflow if gas_target_adjusted < parent.gas_used()
            let delta = gas_target_adjusted.saturating_sub(parent.gas_used());
            parent_base_fee.saturating_sub(
                (parent_base_fee as u128 * delta as u128
                    / (gas_target as u128 * BASE_FEE_MAX_CHANGE_DENOMINATOR))
                    as u64,
            )
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

        // Test 1: Gas used equals target (gas_limit / 2)
        parent.gas_used = 15_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, 12);
        assert_eq!(base_fee, 1_000_000_000, "Base fee should remain the same when at target");

        // Test 2: Gas used above target
        parent.gas_used = 20_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, 12);
        assert!(base_fee > 1_000_000_000, "Base fee should increase when above target");

        // Test 3: Gas used below target
        parent.gas_used = 10_000_000;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, 12);
        assert!(base_fee < 1_000_000_000, "Base fee should decrease when below target");

        // Test 4: Edge case - zero gas used
        parent.gas_used = 0;
        let base_fee = calculate_next_block_eip4396_base_fee(&parent, 12);
        assert!(base_fee < 1_000_000_000, "Base fee should decrease with zero gas used");
    }
}
