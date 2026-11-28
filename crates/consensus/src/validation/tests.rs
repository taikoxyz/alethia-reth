use std::sync::Arc;

use super::{anchor::validate_input_selector, *};
use alethia_reth_chainspec::{
    TAIKO_DEVNET, TAIKO_HOODI, TAIKO_MAINNET, hardfork::TaikoHardfork, spec::TaikoChainSpec,
};
use alethia_reth_primitives::addresses::get_treasury_address;
use alloy_consensus::{
    BlockBody, EMPTY_OMMER_ROOT_HASH, Header, SignableTransaction, Signed, TxEip1559, TxEip4844,
    TxEip7702, TxLegacy, constants::EMPTY_ROOT_HASH, proofs,
};
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::{Address, B256, Bytes, ChainId, FixedBytes, Signature, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator};
use reth_ethereum_primitives::{Block, EthPrimitives, Receipt, TransactionSigned};
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader};

#[derive(Debug)]
struct NullBlockReader;

impl TaikoBlockReader for NullBlockReader {
    fn block_timestamp_by_hash(&self, _hash: B256) -> Option<u64> {
        None
    }
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
fn test_validate_block_pre_execution_rejects_blob_transactions() {
    let body = BlockBody { transactions: vec![make_blob_tx()], ..Default::default() };
    let header = Header {
        transactions_root: proofs::calculate_transaction_root(&body.transactions),
        ..Default::default()
    };

    let block = SealedBlock::seal_slow(Block { header, body });

    assert!(matches!(
        test_consensus(devnet_chain_spec()).validate_block_pre_execution(&block),
        Err(ConsensusError::Other(_))
    ));
}

#[test]
fn pre_unzen_header_still_rejects_nonzero_difficulty() {
    let consensus = test_consensus(pre_unzen_chain_spec());
    let header = Header { difficulty: U256::from(1_u64), ..Default::default() };

    let err = consensus
        .validate_header(&SealedHeader::new_unhashed(header))
        .expect_err("pre-Unzen headers must still reject nonzero difficulty");
    assert!(matches!(err, ConsensusError::TheMergeDifficultyIsNotZero));
}

#[test]
fn unzen_header_allows_nonzero_difficulty() {
    let consensus = test_consensus(unzen_chain_spec());
    let header = Header {
        timestamp: 1,
        difficulty: U256::from(7_u64),
        base_fee_per_gas: Some(1),
        gas_limit: 30_000_000,
        extra_data: shasta_extra_data(),
        ..canonical_unzen_header()
    };

    consensus
        .validate_header(&SealedHeader::new_unhashed(header))
        .expect("Unzen headers should allow nonzero difficulty");
}

#[test]
fn unzen_headers_require_canonical_fork_fields() {
    let consensus = test_consensus(unzen_chain_spec());
    let header = canonical_unzen_header();
    for noncanonical in [
        Header { parent_beacon_block_root: None, ..header.clone() },
        Header { blob_gas_used: Some(1), ..header.clone() },
        Header { excess_blob_gas: None, ..header.clone() },
        Header { requests_hash: Some(B256::ZERO), ..header.clone() },
        Header { withdrawals_root: None, ..header.clone() },
    ] {
        assert!(consensus.validate_header(&SealedHeader::new_unhashed(noncanonical)).is_err());
    }

    let pre_unzen = test_consensus(pre_unzen_chain_spec());
    pre_unzen.validate_header(&SealedHeader::new_unhashed(header)).unwrap();
}

#[test]
fn shasta_anchor_checks_envelope_recipient_and_both_fee_fields() {
    let mut spec = unzen_chain_spec();
    spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(2));
    let good = TxEip1559 {
        chain_id: spec.inner.chain.id(),
        gas_limit: ANCHOR_V3_V4_GAS_LIMIT,
        to: get_treasury_address(spec.inner.chain.id()).into(),
        input: Bytes::from_static(ANCHOR_V4_SELECTOR),
        max_fee_per_gas: 10,
        ..Default::default()
    };
    let legacy = TxLegacy {
        chain_id: Some(good.chain_id),
        gas_limit: good.gas_limit,
        gas_price: good.max_fee_per_gas,
        to: good.to,
        input: good.input.clone(),
        ..Default::default()
    };
    let check = |tx: &TransactionSigned, timestamp| {
        validate_anchor_transaction(
            tx,
            &spec,
            AnchorValidationContext { timestamp, block_number: 1, base_fee_per_gas: 10 },
        )
    };

    check(&sign_anchor(good.clone()), 2).unwrap();
    for (tx, error_message) in [
        (
            sign_anchor(TxEip7702 {
                chain_id: good.chain_id,
                gas_limit: good.gas_limit,
                max_fee_per_gas: good.max_fee_per_gas,
                to: good.to.to().copied().unwrap(),
                input: good.input.clone(),
                ..Default::default()
            }),
            "EIP-1559 envelope",
        ),
        (sign_anchor(TxEip1559 { to: Address::ZERO.into(), ..good.clone() }), "must call"),
        (
            sign_anchor(TxEip1559 { max_fee_per_gas: 11, ..good.clone() }),
            "max_fee_per_gas mismatch",
        ),
        (
            sign_anchor(TxEip1559 { max_priority_fee_per_gas: 1, ..good.clone() }),
            "max_priority_fee_per_gas mismatch",
        ),
    ] {
        let err = check(&tx, 2).unwrap_err();
        assert!(err.to_string().contains(error_message), "{err}");
    }

    // Before Shasta, the previous effective-tip rule accepts both of these shapes.
    check(
        &sign_anchor(TxLegacy {
            input: Bytes::from_static(ANCHOR_V3_SELECTOR),
            to: Address::ZERO.into(),
            ..legacy
        }),
        1,
    )
    .unwrap();
    check(
        &sign_anchor(TxEip1559 {
            input: Bytes::from_static(ANCHOR_V3_SELECTOR),
            max_fee_per_gas: 11,
            ..good
        }),
        1,
    )
    .unwrap();
}

fn sign_anchor<T: SignableTransaction<Signature>>(tx: T) -> TransactionSigned
where
    TransactionSigned: From<Signed<T>>,
{
    let signer: PrivateKeySigner =
        "0x92954368afd3caa1f3ce3ead0069c1af414054aefe1ef9aeacc1bf426222ce38".parse().unwrap();
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    Signed::new_unhashed(tx, signature).into()
}

#[test]
fn unzen_rejects_nonempty_withdrawals_commitments() {
    let mut spec = unzen_chain_spec();
    spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(2));
    let consensus = test_consensus(spec);
    let body =
        BlockBody { withdrawals: Some(vec![Default::default()].into()), ..Default::default() };
    let mut header = Header {
        withdrawals_root: Some(proofs::calculate_withdrawals_root(
            body.withdrawals.as_ref().unwrap(),
        )),
        ..canonical_unzen_header()
    };
    consensus.validate_header(&SealedHeader::new_unhashed(header.clone())).unwrap();
    consensus
        .validate_block_pre_execution(&SealedBlock::seal_slow(Block {
            header: header.clone(),
            body: body.clone(),
        }))
        .unwrap();

    header.timestamp = 2;
    assert!(matches!(
        consensus.validate_header(&SealedHeader::new_unhashed(header.clone())),
        Err(ConsensusError::BodyWithdrawalsRootDiff(_))
    ));

    let block = SealedBlock::seal_slow(Block { header, body });
    assert!(matches!(
        consensus.validate_block_pre_execution(&block),
        Err(ConsensusError::BodyWithdrawalsRootDiff(diff)) if diff.expected == EMPTY_WITHDRAWALS
    ));
}

#[test]
fn osaka_rejects_blocks_above_the_encoded_size_limit() {
    let mut spec = unzen_chain_spec();
    spec.inner.hardforks.insert(EthereumHardfork::Osaka, ForkCondition::Timestamp(2));
    let consensus = test_consensus(spec);
    let make_block = |timestamp, input_len| {
        let tx = Signed::new_unhashed(
            TxLegacy { input: Bytes::from(vec![0; input_len]), ..Default::default() },
            Signature::new(U256::from(1), U256::from(2), false),
        );
        let body = BlockBody {
            transactions: vec![tx.into()],
            withdrawals: Some(Default::default()),
            ..Default::default()
        };
        let header = Header {
            timestamp,
            transactions_root: proofs::calculate_transaction_root(&body.transactions),
            ..canonical_unzen_header()
        };
        SealedBlock::seal_slow(Block { header, body })
    };
    let oversized = make_block(2, MAX_RLP_BLOCK_SIZE);
    let exact_input_len = 2 * MAX_RLP_BLOCK_SIZE - oversized.rlp_length();
    let at_limit = make_block(2, exact_input_len);
    assert_eq!(at_limit.rlp_length(), MAX_RLP_BLOCK_SIZE);
    consensus.validate_block_pre_execution(&at_limit).unwrap();

    let error = consensus.validate_block_pre_execution(&oversized).unwrap_err();
    assert!(matches!(error, ConsensusError::BlockTooLarge { .. }));

    let pre_osaka = make_block(1, exact_input_len + 1);
    assert_eq!(pre_osaka.rlp_length(), MAX_RLP_BLOCK_SIZE + 1);
    consensus.validate_block_pre_execution(&pre_osaka).unwrap();
}

#[test]
fn shasta_rejects_a_zero_gas_target_at_the_fork_boundary() {
    let mut spec = pre_unzen_chain_spec();
    spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(2));
    let consensus = test_consensus(spec);
    let mut header = Header { gas_limit: 1, ..canonical_unzen_header() };
    let parent = SealedHeader::seal_slow(header.clone());
    consensus.validate_header(&parent).unwrap();

    header.timestamp = 2;
    let err = consensus.validate_header(&SealedHeader::new_unhashed(header.clone())).unwrap_err();
    assert!(err.to_string().contains("nonzero gas target"));

    header.parent_hash = parent.hash();
    header.number = parent.number + 1;
    header.gas_limit = 30_000_000;
    let err = consensus
        .validate_header_against_parent(&SealedHeader::new_unhashed(header), &parent)
        .unwrap_err();
    assert!(err.to_string().contains("nonzero gas target"));
}

#[test]
fn shasta_anchor_validation_requires_a_transaction() {
    let block = Block { header: canonical_unzen_header(), body: BlockBody::default() };
    let block = RecoveredBlock::new_unhashed(block, vec![]);
    let err = validate_anchor_transaction_in_block(&block, &unzen_chain_spec()).unwrap_err();
    assert!(err.to_string().contains("missing its anchor transaction"));
}

#[test]
fn unzen_execution_requires_the_empty_withdrawals_body() {
    let consensus = test_consensus(unzen_chain_spec());
    let header = canonical_unzen_header();
    let missing =
        SealedBlock::seal_slow(Block { header: header.clone(), body: BlockBody::default() });
    assert!(matches!(
        consensus.validate_block_pre_execution(&missing),
        Err(ConsensusError::BodyWithdrawalsMissing)
    ));

    let body = BlockBody { withdrawals: Some(Default::default()), ..Default::default() };
    let block = SealedBlock::seal_slow(Block { header, body });
    consensus.validate_block_pre_execution(&block).expect("canonical empty withdrawals");
}

#[test]
fn shasta_header_requires_exact_extra_data_len() {
    let consensus = test_consensus(devnet_chain_spec());
    let base = Header {
        timestamp: 1,
        base_fee_per_gas: Some(1),
        gas_limit: 30_000_000,
        ..canonical_unzen_header()
    };

    consensus
        .validate_header(&SealedHeader::new_unhashed(Header {
            extra_data: shasta_extra_data(),
            ..base.clone()
        }))
        .expect("the 7-byte extraData layout should validate");

    for (name, extra) in [
        ("empty", Bytes::new()),
        ("2 bytes", Bytes::from_static(&[75, 0])),
        ("3 bytes", Bytes::from_static(&[75, 0, 1])),
        ("12 bytes", Bytes::from_static(&[0; 12])),
    ] {
        let err = consensus
            .validate_header(&SealedHeader::new_unhashed(Header {
                extra_data: extra,
                ..base.clone()
            }))
            .expect_err(name);
        assert!(matches!(err, ConsensusError::Other(_)), "{name}: unexpected error {err:?}");
    }
}

#[test]
fn pre_shasta_header_has_no_extra_data_len_rule() {
    let mut chain_spec = devnet_chain_spec();
    chain_spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Never);
    let consensus = test_consensus(chain_spec);
    let header = Header {
        timestamp: 1,
        base_fee_per_gas: Some(1),
        gas_limit: 30_000_000,
        ..canonical_unzen_header()
    };

    consensus
        .validate_header(&SealedHeader::new_unhashed(header))
        .expect("pre-Shasta headers have no extraData length rule");
}

#[test]
fn amsterdam_active_header_is_rejected_at_import() {
    let mut chain_spec = devnet_chain_spec();
    chain_spec.inner.hardforks.insert(EthereumHardfork::Amsterdam, ForkCondition::Timestamp(100));
    let consensus = test_consensus(chain_spec);
    let base = Header {
        timestamp: 99,
        base_fee_per_gas: Some(1),
        gas_limit: 30_000_000,
        extra_data: shasta_extra_data(),
        ..canonical_unzen_header()
    };

    consensus
        .validate_header(&SealedHeader::new_unhashed(base.clone()))
        .expect("headers before the Amsterdam activation should validate");

    let err = consensus
        .validate_header(&SealedHeader::new_unhashed(Header { timestamp: 100, ..base }))
        .expect_err("Amsterdam-active headers must be rejected at import");
    assert!(err.to_string().contains("Amsterdam"), "unexpected error: {err}");
}

#[test]
fn pre_amsterdam_header_rejects_block_access_list_hash() {
    let consensus = test_consensus(devnet_chain_spec());
    let header = Header {
        timestamp: 1,
        base_fee_per_gas: Some(1),
        gas_limit: 30_000_000,
        extra_data: shasta_extra_data(),
        block_access_list_hash: Some(B256::ZERO),
        ..canonical_unzen_header()
    };

    let err = consensus
        .validate_header(&SealedHeader::new_unhashed(header))
        .expect_err("pre-Amsterdam headers must not carry a block access list hash");
    assert!(matches!(err, ConsensusError::BlockAccessListHashUnexpected));
}

#[test]
fn pre_amsterdam_header_rejects_slot_number() {
    let consensus = test_consensus(devnet_chain_spec());
    let header = Header {
        timestamp: 1,
        base_fee_per_gas: Some(1),
        gas_limit: 30_000_000,
        extra_data: shasta_extra_data(),
        slot_number: Some(1),
        ..canonical_unzen_header()
    };

    let err = consensus
        .validate_header(&SealedHeader::new_unhashed(header))
        .expect_err("pre-Amsterdam headers must not carry a slot number");
    assert!(matches!(err, ConsensusError::SlotNumberUnexpected));
}

#[test]
fn shipped_taiko_specs_never_activate_amsterdam() {
    // Unzen, Taiko's latest fork, activates Osaka-level execution rules only. No shipped spec
    // may ever schedule Amsterdam, or `validate_header` would reject every block at import.
    for (name, spec) in [
        ("mainnet", &**TAIKO_MAINNET),
        ("taiko-hoodi", &**TAIKO_HOODI),
        ("devnet", &**TAIKO_DEVNET),
    ] {
        assert!(
            !spec.is_amsterdam_active_at_timestamp(u64::MAX),
            "{name} must never activate Amsterdam"
        );
    }
}

#[test]
fn unzen_post_execution_rejects_body_past_truncation_point() {
    let consensus = test_consensus(unzen_chain_spec());
    let result = BlockExecutionResult::<Receipt>::default();
    let header = Header {
        timestamp: 1,
        base_fee_per_gas: Some(1),
        gas_limit: 30_000_000,
        gas_used: 0,
        receipts_root: EMPTY_ROOT_HASH,
        requests_hash: Some(result.requests.requests_hash()),
        ..Default::default()
    };
    let block = Block {
        header,
        body: BlockBody { transactions: vec![make_legacy_tx()], ommers: vec![], withdrawals: None },
    };
    let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);

    let err =
        <TaikoBeaconConsensus as FullConsensus<EthPrimitives>>::validate_block_post_execution(
            &consensus, &recovered, &result, None, None,
        )
        .expect_err("Unzen blocks must reject bodies that extend past the truncation point");
    assert!(matches!(err, ConsensusError::Other(_)));
    assert!(err.to_string().contains("truncation"));
}

#[test]
fn test_validate_block_pre_execution_rejects_non_empty_ommer_hash() {
    let header = Header { ommers_hash: FixedBytes::<32>::with_last_byte(1), ..Default::default() };
    let expected = header.ommers_hash;

    let block = SealedBlock::seal_slow(Block { header, body: BlockBody::default() });

    assert!(matches!(
        test_consensus(devnet_chain_spec()).validate_block_pre_execution(&block),
        Err(ConsensusError::BodyOmmersHashDiff(diff))
            if diff.got == expected && diff.expected == EMPTY_OMMER_ROOT_HASH
    ));
}

#[test]
fn test_validate_block_pre_execution_rejects_non_empty_ommers_body() {
    let header = Header::default();
    let body = BlockBody { ommers: vec![Header::default()], ..Default::default() };

    let block = SealedBlock::seal_slow(Block { header, body });

    assert!(matches!(
        test_consensus(devnet_chain_spec()).validate_block_pre_execution(&block),
        Err(ConsensusError::BodyOmmersHashDiff(_))
    ));
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

fn test_consensus(chain_spec: TaikoChainSpec) -> TaikoBeaconConsensus {
    TaikoBeaconConsensus::new(Arc::new(chain_spec), Arc::new(NullBlockReader))
}

fn canonical_unzen_header() -> Header {
    Header {
        number: 1,
        timestamp: 1,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(1),
        extra_data: shasta_extra_data(),
        withdrawals_root: Some(EMPTY_WITHDRAWALS),
        parent_beacon_block_root: Some(B256::ZERO),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        requests_hash: Some(EMPTY_REQUESTS_HASH),
        ..Default::default()
    }
}

fn shasta_extra_data() -> Bytes {
    Bytes::from_static(&[75, 0, 0, 0, 0, 0x4c, 0x81])
}

fn devnet_chain_spec() -> TaikoChainSpec {
    (*TAIKO_DEVNET).as_ref().clone()
}

fn pre_unzen_chain_spec() -> TaikoChainSpec {
    let mut chain_spec = devnet_chain_spec();
    chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Never);
    chain_spec
}

fn unzen_chain_spec() -> TaikoChainSpec {
    let mut chain_spec = devnet_chain_spec();
    chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(0));
    chain_spec
}
