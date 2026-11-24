use std::sync::Arc;

use super::{anchor::validate_input_selector, *};
use alethia_reth_chainspec::{
    TAIKO_DEVNET, TAIKO_HOODI, TAIKO_MAINNET, hardfork::TaikoHardfork, spec::TaikoChainSpec,
};
use alloy_consensus::{
    BlockBody, EMPTY_OMMER_ROOT_HASH, Header, Signed, TxEip4844, TxLegacy,
    constants::{EMPTY_ROOT_HASH, EMPTY_WITHDRAWALS},
    proofs,
};
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use alloy_primitives::{Address, B256, Bytes, ChainId, FixedBytes, Signature, TxKind, U256};
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
        MAX_BASE_FEE, MIN_BASE_FEE, SHASTA_BLOCK_TIME_TARGET, calculate_next_block_eip4396_base_fee,
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
        2,
        SHASTA_BLOCK_TIME_TARGET,
        parent.base_fee_per_gas.expect("parent base fee set"),
        MIN_BASE_FEE,
    );
    assert_eq!(base_fee, 1_000_000_000, "Base fee should remain the same when at target");

    // Test 2: Gas used above target
    parent.gas_used = 20_000_000;
    let base_fee = calculate_next_block_eip4396_base_fee(
        &parent,
        2,
        SHASTA_BLOCK_TIME_TARGET,
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
        2,
        SHASTA_BLOCK_TIME_TARGET,
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
        ..Default::default()
    };

    consensus
        .validate_header(&SealedHeader::new_unhashed(header))
        .expect("Unzen headers should allow nonzero difficulty");
}

#[test]
fn shasta_header_requires_exact_extra_data_len() {
    let consensus = test_consensus(devnet_chain_spec());
    let base = Header {
        timestamp: 1,
        base_fee_per_gas: Some(1),
        gas_limit: 30_000_000,
        ..Default::default()
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
        ("13 bytes", Bytes::from_static(&[0; 13])),
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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

fn shasta_extra_data() -> Bytes {
    Bytes::from_static(&[75, 0, 0, 0, 0, 0x4c, 0x81])
}

fn etna_extra_data() -> Bytes {
    Bytes::from_static(&[100, 0, 0, 0, 0, 0x4c, 0x81, 0, 0, 0, 0, 0x12, 0x34])
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

fn etna_chain_spec() -> TaikoChainSpec {
    let mut chain_spec = devnet_chain_spec();
    chain_spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(0));
    chain_spec
}

/// Returns a non-genesis Etna header that satisfies every header rule.
fn etna_header() -> Header {
    Header {
        number: 1,
        timestamp: 1,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(1),
        extra_data: etna_extra_data(),
        parent_beacon_block_root: Some(B256::with_last_byte(7)),
        withdrawals_root: Some(EMPTY_WITHDRAWALS),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        ..Default::default()
    }
}

#[test]
fn etna_header_requires_nonzero_root_and_ordinary_first_transaction() {
    let spec = etna_chain_spec();
    let consensus = test_consensus(spec.clone());
    for root in [None, Some(B256::ZERO), Some(B256::with_last_byte(7))] {
        let header = Header { parent_beacon_block_root: root, ..etna_header() };
        assert_eq!(
            consensus.validate_header(&SealedHeader::seal_slow(header)).is_ok(),
            root.is_some_and(|r| !r.is_zero())
        );
    }
    // An Etna block's first transaction is ordinary, so the legacy anchor check must skip it.
    let tx: TransactionSigned = Signed::new_unchecked(
        TxLegacy::default(),
        Signature::new(U256::from(1), U256::from(2), false),
        B256::ZERO,
    )
    .into();
    let block = RecoveredBlock::new_unhashed(
        Block {
            header: Header { number: 1, timestamp: 1, ..Default::default() },
            body: reth_ethereum_primitives::BlockBody {
                transactions: vec![tx],
                ..Default::default()
            },
        },
        vec![Address::ZERO],
    );
    assert!(validate_anchor_transaction_in_block(&block, &spec).is_ok());
}

#[test]
fn etna_header_requires_empty_withdrawals_and_zero_blob_gas() {
    let consensus = test_consensus(etna_chain_spec());
    consensus
        .validate_header(&SealedHeader::seal_slow(etna_header()))
        .expect("an Etna header with empty body commitments should validate");

    for (name, header) in [
        ("missing withdrawals root", Header { withdrawals_root: None, ..etna_header() }),
        (
            "nonempty withdrawals root",
            Header { withdrawals_root: Some(B256::with_last_byte(1)), ..etna_header() },
        ),
        ("missing blob gas used", Header { blob_gas_used: None, ..etna_header() }),
        ("nonzero blob gas used", Header { blob_gas_used: Some(131_072), ..etna_header() }),
        ("missing excess blob gas", Header { excess_blob_gas: None, ..etna_header() }),
        ("nonzero excess blob gas", Header { excess_blob_gas: Some(1), ..etna_header() }),
    ] {
        let err = consensus.validate_header(&SealedHeader::seal_slow(header)).expect_err(name);
        assert!(matches!(err, ConsensusError::Other(_)), "{name}: unexpected error {err:?}");
    }

    // Genesis keeps its genesis-file header fields: no root, body commitments or extraData layout.
    let genesis = Header {
        number: 0,
        extra_data: Bytes::new(),
        parent_beacon_block_root: Some(B256::ZERO),
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        ..etna_header()
    };
    consensus
        .validate_header(&SealedHeader::seal_slow(genesis))
        .expect("the Etna genesis header is exempt from the non-genesis header rules");
}

#[test]
fn etna_header_requires_the_13_byte_extra_data_layout() {
    let consensus = test_consensus(etna_chain_spec());
    // `primitives::etna` covers every length; a Shasta-shaped 7-byte header, one byte too many and
    // 16 bytes appended after the anchor block number check the wiring.
    let with_tail = |tail: usize| {
        let mut extra_data = etna_extra_data().to_vec();
        extra_data.resize(extra_data.len() + tail, 0);
        extra_data
    };
    for extra_data in [vec![0; 7], with_tail(1), with_tail(16)] {
        let len = extra_data.len();
        let header = Header { extra_data: extra_data.into(), ..etna_header() };
        let err = consensus
            .validate_header(&SealedHeader::seal_slow(header))
            .expect_err("an Etna extraData that is not 13 bytes must be rejected");
        assert!(err.to_string().contains("extraData"), "{len} bytes: {err}");
    }
}

#[test]
fn etna_header_requires_full_base_fee_share() {
    let consensus = test_consensus(etna_chain_spec());
    // A share above 100 would credit the beneficiary more base fee than the sender paid.
    for pctg in [99, 101] {
        let mut extra_data = etna_extra_data().to_vec();
        extra_data[0] = pctg;
        let header = Header { extra_data: extra_data.into(), ..etna_header() };
        let err = consensus
            .validate_header(&SealedHeader::seal_slow(header))
            .expect_err("an Etna basefeeSharingPctg other than 100 must be rejected");
        assert!(err.to_string().contains("basefeeSharingPctg"), "{pctg}: {err}");
    }
}

#[test]
fn etna_rejects_a_block_that_commits_to_withdrawals() {
    let consensus = test_consensus(etna_chain_spec());
    let mut body = BlockBody { withdrawals: Some(Default::default()), ..Default::default() };
    let valid = SealedBlock::seal_slow(Block { header: etna_header(), body: body.clone() });
    consensus.validate_block_pre_execution(&valid).expect("an empty Etna body should validate");

    // A zero-amount withdrawal with a matching root changes neither Taiko execution nor the
    // body-against-header check, so only the Etna rules can reject it.
    body.withdrawals.as_mut().expect("withdrawals are present").push(Default::default());
    let header = Header {
        withdrawals_root: Some(proofs::calculate_withdrawals_root(
            body.withdrawals.as_ref().expect("withdrawals are present"),
        )),
        ..etna_header()
    };
    let mutated = SealedBlock::seal_slow(Block { header, body });
    <TaikoBeaconConsensus as Consensus<Block>>::validate_body_against_header(
        &consensus,
        mutated.body(),
        mutated.sealed_header(),
    )
    .expect("the mutated body still matches its own header");

    assert!(consensus.validate_block_pre_execution(&mutated).is_err());
}

/// Block reader that knows one block: the grandparent of the header under test.
#[derive(Debug)]
struct GrandparentReader {
    /// Grandparent hash.
    hash: B256,
    /// Grandparent timestamp.
    timestamp: u64,
}

impl TaikoBlockReader for GrandparentReader {
    fn block_timestamp_by_hash(&self, hash: B256) -> Option<u64> {
        (hash == self.hash).then_some(self.timestamp)
    }
}

/// Devnet with Etna active from `etna_time`, on chain id 167000 when `mainnet_id` is set.
fn base_fee_spec(etna_time: u64, mainnet_id: bool) -> TaikoChainSpec {
    let mut spec = devnet_chain_spec();
    spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(etna_time));
    if mainnet_id {
        spec.inner.chain = TAIKO_MAINNET.inner.chain;
    }
    spec
}

/// A parent header with a 30,000,000 gas limit.
fn fee_parent(number: u64, timestamp: u64, base_fee: u64, gas_used: u64) -> Header {
    Header {
        number,
        timestamp,
        gas_limit: 30_000_000,
        gas_used,
        base_fee_per_gas: Some(base_fee),
        ..Default::default()
    }
}

/// Runs `validate_header_against_parent` for a child of `parent` at `timestamp` carrying
/// `base_fee`, where `parent`'s own parent has `grandparent_timestamp`.
fn check_base_fee(
    spec: &TaikoChainSpec,
    grandparent_timestamp: u64,
    parent: &Header,
    timestamp: u64,
    base_fee: u64,
) -> Result<(), ConsensusError> {
    let grandparent = Header {
        number: parent.number.saturating_sub(1),
        timestamp: grandparent_timestamp,
        ..Default::default()
    };
    let parent = Header { parent_hash: grandparent.hash_slow(), ..parent.clone() };
    let reader =
        GrandparentReader { hash: grandparent.hash_slow(), timestamp: grandparent_timestamp };
    let consensus = TaikoBeaconConsensus::new(Arc::new(spec.clone()), Arc::new(reader));
    let child = Header {
        number: parent.number + 1,
        parent_hash: parent.hash_slow(),
        timestamp,
        base_fee_per_gas: Some(base_fee),
        ..Default::default()
    };
    consensus.validate_header_against_parent(
        &SealedHeader::seal_slow(child),
        &SealedHeader::seal_slow(parent),
    )
}

/// Asserts that a child carrying `got` is invalid because the expected base fee is `expected`.
fn assert_base_fee_rejected(
    spec: &TaikoChainSpec,
    grandparent_timestamp: u64,
    parent: &Header,
    timestamp: u64,
    got: u64,
    expected: u64,
    vector: &str,
) {
    let err =
        check_base_fee(spec, grandparent_timestamp, parent, timestamp, got).expect_err(vector);
    assert!(
        matches!(err, ConsensusError::BaseFeeDiff(ref diff) if diff.got == got && diff.expected == expected),
        "{vector}: {err:?}"
    );
}

/// Asserts that `expected` is the only base fee a child at `timestamp` may carry.
fn assert_base_fee_is(
    spec: &TaikoChainSpec,
    grandparent_timestamp: u64,
    parent: &Header,
    timestamp: u64,
    expected: u64,
    vector: &str,
) {
    check_base_fee(spec, grandparent_timestamp, parent, timestamp, expected)
        .unwrap_or_else(|err| panic!("{vector}: {err:?}"));
    for got in [expected - 1, expected + 1] {
        assert_base_fee_rejected(
            spec,
            grandparent_timestamp,
            parent,
            timestamp,
            got,
            expected,
            vector,
        );
    }
}

/// The EIP-4396 formula vectors in taiko-mono's `rules/vectors/A.md` at `178d7a7a6`, through
/// header validation of an Etna block: the parent's gasLimit is 30,000,000 and the chain id 167000
/// unless a row says otherwise.
#[test]
fn eip4396_formula_vectors_hold_at_header_validation() {
    use crate::eip4396::{
        ETNA_BLOCK_TIME_TARGET, MAINNET_MIN_BASE_FEE, SHASTA_BLOCK_TIME_TARGET,
        calculate_next_block_eip4396_base_fee,
    };
    let mainnet = base_fee_spec(0, true);
    let devnet = base_fee_spec(0, false);
    let grandparent = 1_000;
    // (vector, parent base fee, parent gasUsed, parent minus grandparent timestamp, expected)
    let accept: [(&str, u64, u64, u64, u64); 9] = [
        ("10a.a", 100_000_000, 15_000_000, 4, 100_000_000),
        ("10a.b", 100_000_000, 30_000_000, 4, 112_500_000),
        ("10a.c", 100_000_000, 15_000_000, 8, 88_750_000),
        ("10a.d", 10_000_000, 0, 4, 10_000_000),
        ("10a.e", 10_000_000, 0, 4, 8_750_000),
        ("10a.f", 10_000_000, 15_000_001, 4, 10_000_001),
        ("10a.g", 950_000_000, 30_000_000, 4, 1_000_000_000),
        ("10a.k", 100_000_000, 7_500_000, 2, 100_000_000),
        ("10a.l", 100_000_000, 15_000_000, 2, 106_250_000),
    ];
    for (vector, base_fee, gas_used, interval, expected) in accept {
        let spec = if vector == "10a.e" { &devnet } else { &mainnet };
        let parent = fee_parent(5, grandparent + interval, base_fee, gas_used);
        assert_base_fee_is(spec, grandparent, &parent, parent.timestamp + 1, expected, vector);
    }

    // 10a.h: the interval is the parent's minus the grandparent's timestamp (4 s here), not the
    // block's own minus its parent's (8 s here, which yields 10a.c's value).
    let parent = fee_parent(5, grandparent + 4, 100_000_000, 15_000_000);
    let own_interval = calculate_next_block_eip4396_base_fee(
        &parent,
        8,
        ETNA_BLOCK_TIME_TARGET,
        100_000_000,
        MAINNET_MIN_BASE_FEE,
    );
    assert_eq!(own_interval, 88_750_000);
    assert_base_fee_rejected(
        &mainnet,
        grandparent,
        &parent,
        grandparent + 12,
        own_interval,
        100_000_000,
        "10a.h",
    );

    // 10a.i: every division floors. delta = 100,000,007 * 15,000,000 / 15,000,000 / 8 =
    // 12,500,000.875 floors to 12,500,000; the rounded 112,500,008 is the rejected `+ 1` case.
    let parent = fee_parent(5, grandparent + 4, 100_000_007, 30_000_000);
    assert_base_fee_is(&mainnet, grandparent, &parent, parent.timestamp + 1, 112_500_007, "10a.i");

    // 10a.j: the parent's own gasLimit, with no anchor reserve taken off before `target`.
    let parent = fee_parent(5, grandparent + 4, 100_000_000, 15_000_000);
    let reserved = calculate_next_block_eip4396_base_fee(
        &Header { gas_limit: 29_000_000, ..parent.clone() },
        4,
        ETNA_BLOCK_TIME_TARGET,
        100_000_000,
        MAINNET_MIN_BASE_FEE,
    );
    assert_eq!(reserved, 100_431_034);
    assert_base_fee_rejected(
        &mainnet,
        grandparent,
        &parent,
        parent.timestamp + 1,
        reserved,
        100_000_000,
        "10a.j",
    );

    // 10a.m: an Etna block divides the interval by 4 s, not by Unzen's 2 s.
    let parent = fee_parent(5, grandparent + 2, 100_000_000, 15_000_000);
    let unzen_target = calculate_next_block_eip4396_base_fee(
        &parent,
        2,
        SHASTA_BLOCK_TIME_TARGET,
        100_000_000,
        MAINNET_MIN_BASE_FEE,
    );
    assert_eq!(unzen_target, 100_000_000);
    assert_base_fee_rejected(
        &mainnet,
        grandparent,
        &parent,
        parent.timestamp + 1,
        unzen_target,
        106_250_000,
        "10a.m",
    );
}

/// The `baseFeePerGas` header vectors in taiko-mono's `rules/vectors/3.2.md` at `178d7a7a6`
/// (chain id 167000). Their row that accepts a later block whose base fee follows the EIP-4396
/// adjustment within the chain's bounds is covered by the formula vectors' accept rows above.
#[test]
fn base_fee_field_vectors_hold_at_header_validation() {
    use crate::eip4396::{
        ETNA_BLOCK_TIME_TARGET, MAINNET_MIN_BASE_FEE, calculate_next_block_eip4396_base_fee,
    };
    // S = 0: block 1 is fixed at 0.025 gwei (8.a), so 0.01 gwei is invalid (8.c).
    let s_zero = base_fee_spec(0, true);
    let genesis = fee_parent(0, 0, 10_000_000, 0);
    assert_base_fee_is(&s_zero, 0, &genesis, 1, 25_000_000, "8.a");
    assert_base_fee_rejected(&s_zero, 0, &genesis, 1, 10_000_000, 25_000_000, "8.c");

    // 8.d: below the 0.01 gwei minimum (10a.d's parent, whose raw result is 0.00875 gwei).
    let parent = fee_parent(5, 1_004, 10_000_000, 0);
    assert_base_fee_rejected(&s_zero, 1_000, &parent, 1_005, 9_000_000, 10_000_000, "8.d");
    // 8.e: above the 1 gwei maximum (10a.g's parent, whose raw result is 1.06875 gwei).
    let parent = fee_parent(5, 1_004, 950_000_000, 30_000_000);
    assert_base_fee_rejected(&s_zero, 1_000, &parent, 1_005, 1_010_000_000, 1_000_000_000, "8.e");
    // 8.f: a fixed 12 s interval instead of the parent's 4 s.
    let parent = fee_parent(5, 1_004, 100_000_000, 15_000_000);
    let fixed = calculate_next_block_eip4396_base_fee(
        &parent,
        12,
        ETNA_BLOCK_TIME_TARGET,
        100_000_000,
        MAINNET_MIN_BASE_FEE,
    );
    assert_base_fee_rejected(&s_zero, 1_000, &parent, 1_005, fixed, 100_000_000, "8.f");

    // S > 0 with Etna at 1,000: S - 1 at 992 and S at 996 are Unzen blocks, S + 1 at 1,000 is
    // the first Etna block. S + 1 follows EIP-4396 at Etna's 4 s target (8.g), not the fixed
    // 0.025 gwei (8.i).
    let switching = base_fee_spec(1_000, true);
    let s = fee_parent(5, 996, 100_000_000, 30_000_000);
    assert_base_fee_is(&switching, 992, &s, 1_000, 112_500_000, "8.g");
    assert_base_fee_rejected(&switching, 992, &s, 1_000, 25_000_000, 112_500_000, "8.i");
    // 8.h: S + 2 after a three-hour outage, so S + 1's interval is ts(S + 1) - ts(S) = 10,800 s
    // and `adjusted` is capped at 28,500,000.
    let s_plus_1 = fee_parent(6, 996 + 10_800, 100_000_000, 0);
    assert_base_fee_is(&switching, 996, &s_plus_1, 996 + 10_801, 76_250_000, "8.h");
}

/// The target follows the block being validated, not its parent: on the same Unzen parent, an
/// Unzen child uses 2 s and the first Etna child 4 s.
#[test]
fn base_fee_target_switches_at_the_first_etna_block() {
    let switching = base_fee_spec(1_000, true);
    // The parent S used all 30,000,000 gas 4 s after its own parent: 2 s gives `adjusted` =
    // min(30,000,000, 28,500,000), 4 s gives 15,000,000.
    let s = fee_parent(5, 996, 100_000_000, 30_000_000);
    assert_base_fee_is(&switching, 992, &s, 999, 101_250_000, "Unzen child");
    assert_base_fee_is(&switching, 992, &s, 1_000, 112_500_000, "Etna child");
    assert_base_fee_rejected(
        &switching,
        992,
        &s,
        1_000,
        101_250_000,
        112_500_000,
        "Etna child at 2 s",
    );
}
