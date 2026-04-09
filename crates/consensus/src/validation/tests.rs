use std::sync::Arc;

use super::{anchor::validate_input_selector, *};
use alethia_reth_chainspec::{TAIKO_DEVNET, TAIKO_MAINNET};
use alloy_consensus::{BlockBody, EMPTY_OMMER_ROOT_HASH, Header, Signed, TxEip4844, TxLegacy};
use alloy_primitives::{Address, B256, Bytes, ChainId, FixedBytes, Signature, TxKind, U256};
use reth_consensus::{Consensus, ConsensusError};
use reth_ethereum::TransactionSigned;
use reth_ethereum_primitives::Block;
use reth_primitives_traits::SealedBlock;

#[derive(Debug)]
struct NoopBlockReader;

impl TaikoBlockReader for NoopBlockReader {
    fn block_timestamp_by_hash(&self, _hash: B256) -> Option<u64> {
        None
    }
}

fn test_consensus() -> TaikoBeaconConsensus {
    TaikoBeaconConsensus::new(TAIKO_DEVNET.clone(), Arc::new(NoopBlockReader))
}

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

#[test]
fn test_rejects_blob_transactions() {
    let transactions = vec![make_blob_tx()];

    let err =
        validate_no_blob_transactions(&transactions).expect_err("blob transactions should fail");
    assert!(matches!(err, ConsensusError::Other(_)));
}

#[test]
fn test_allows_non_blob_transactions() {
    let transactions = vec![make_legacy_tx()];

    assert!(validate_no_blob_transactions(&transactions).is_ok());
}

#[test]
fn test_validate_block_pre_execution_rejects_non_empty_ommer_hash() {
    let header = Header { ommers_hash: FixedBytes::<32>::with_last_byte(1), ..Default::default() };
    let expected = header.ommers_hash;

    let block = SealedBlock::seal_slow(Block { header, body: BlockBody::default() });

    assert!(matches!(
        test_consensus().validate_block_pre_execution(&block),
        Err(ConsensusError::BodyOmmersHashDiff(diff))
            if diff.got == expected && diff.expected == EMPTY_OMMER_ROOT_HASH
    ));
}

#[test]
fn test_validate_block_pre_execution_ignores_non_empty_ommers_body_when_header_hash_is_empty() {
    let header = Header::default();
    let body = BlockBody { ommers: vec![Header::default()], ..Default::default() };

    let block = SealedBlock::seal_slow(Block { header, body });

    assert!(
        test_consensus().validate_block_pre_execution(&block).is_ok(),
        "main-branch behavior only enforced the header ommer hash here"
    );
}

fn make_blob_tx() -> TransactionSigned {
    let tx = TxEip4844 {
        chain_id: ChainId::from(1u64),
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: 1,
        max_priority_fee_per_gas: 0,
        to: Address::ZERO,
        value: U256::ZERO,
        access_list: Default::default(),
        blob_versioned_hashes: vec![B256::ZERO],
        max_fee_per_blob_gas: 1,
        input: Bytes::default(),
    };
    let signature = Signature::new(U256::from(1), U256::from(2), false);
    Signed::new_unchecked(tx, signature, B256::ZERO).into()
}

fn make_legacy_tx() -> TransactionSigned {
    let tx = TxLegacy {
        chain_id: Some(ChainId::from(1u64)),
        nonce: 0,
        gas_price: 1,
        gas_limit: 21_000,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        input: Bytes::default(),
    };
    let signature = Signature::new(U256::from(1), U256::from(2), false);
    Signed::new_unchecked(tx, signature, B256::ZERO).into()
}
