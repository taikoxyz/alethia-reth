use super::{anchor::validate_input_selector, *};
use alethia_reth_chainspec::{TAIKO_DEVNET, TAIKO_MAINNET};
use alloy_consensus::Header;

#[test]
fn test_validate_against_parent_eip4396_base_fee() {
    let mut header = Header::default();

    assert!(validate_against_parent_eip4396_base_fee(&header).is_err());

    header.base_fee_per_gas = Some(1);
    assert!(validate_against_parent_eip4396_base_fee(&header).is_ok());
}

#[test]
fn test_validate_input_selector() {
    // Valid selector
    let input = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
    let expected_selector = [0x12, 0x34, 0x56, 0x78];
    assert!(validate_input_selector(&input, &expected_selector).is_ok());

    // Invalid selector
    let wrong_selector = [0x11, 0x22, 0x33, 0x44];
    assert!(validate_input_selector(&input, &wrong_selector).is_err());

    // Empty input
    let empty_input = [];
    assert!(validate_input_selector(&empty_input, &expected_selector).is_err());
}

#[test]
fn test_anchor_v4_selector_matches_protocol() {
    assert_eq!(ANCHOR_V4_SELECTOR, &[0x52, 0x3e, 0x68, 0x54]);
}

#[test]
fn test_validate_header_against_parent() {
    use crate::eip4396::{
        BLOCK_TIME_TARGET, MAX_BASE_FEE, MIN_BASE_FEE, calculate_next_block_eip4396_base_fee,
    };

    // Test calculate_next_block_eip4396_base_fee function
    let mut parent = Header {
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(1_000_000_000),
        number: 1,
        ..Default::default()
    };

    // Test 1: Gas used equals target (gas_limit / 2)
    parent.gas_used = 15_000_000;
    let base_fee = calculate_next_block_eip4396_base_fee(
        &parent,
        BLOCK_TIME_TARGET,
        parent.base_fee_per_gas.expect("parent base fee set"),
        MIN_BASE_FEE,
    );
    assert_eq!(base_fee, 1_000_000_000, "Base fee should remain the same when at target");

    // Test 2: Gas used above target
    parent.gas_used = 20_000_000;
    let base_fee = calculate_next_block_eip4396_base_fee(
        &parent,
        BLOCK_TIME_TARGET,
        parent.base_fee_per_gas.expect("parent base fee set"),
        MIN_BASE_FEE,
    );
    assert_eq!(
        base_fee, MAX_BASE_FEE,
        "Base fee should stay clamped at MAX_BASE_FEE when the parent is already at the cap"
    );

    // Test 3: Gas used below target
    parent.gas_used = 10_000_000;
    let base_fee = calculate_next_block_eip4396_base_fee(
        &parent,
        BLOCK_TIME_TARGET,
        parent.base_fee_per_gas.expect("parent base fee set"),
        MIN_BASE_FEE,
    );
    assert!(base_fee < 1_000_000_000, "Base fee should decrease when below target");
}

#[test]
fn test_min_base_fee_to_clamp_uses_chain_id() {
    let mut non_mainnet_spec = TAIKO_DEVNET.as_ref().clone();
    non_mainnet_spec.inner.chain = TAIKO_MAINNET.inner.chain;
    assert_eq!(
        min_base_fee_to_clamp(&non_mainnet_spec),
        MAINNET_MIN_BASE_FEE,
        "Mainnet clamp should be selected by chain id"
    );
}

#[test]
fn test_min_base_fee_to_clamp_defaults_for_non_mainnet() {
    assert_eq!(
        min_base_fee_to_clamp(TAIKO_DEVNET.as_ref()),
        MIN_BASE_FEE,
        "Non-mainnet chains should use the non-mainnet clamp"
    );
}
