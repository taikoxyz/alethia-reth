use super::{
    lookup::{MAX_BACKWARD_SCAN_BLOCKS, max_backward_scan_blocks},
    *,
};
use alethia_reth_consensus::validation::ANCHOR_V4_SELECTOR;
use alethia_reth_db::model::{
    BatchToLastBlock, STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1Origin,
    StoredL1OriginTable,
};
use alethia_reth_primitives::decode_shasta_proposal_id;
use alloy_consensus::{BlockBody, Header, TxLegacy};
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use reth_db::{
    ClientVersion, TableSet, Tables,
    mdbx::{DatabaseArguments, init_db_for},
    table::TableInfo,
    test_utils::{
        TempDatabase, create_test_rocksdb_dir, create_test_static_files_dir, tempdir_path,
    },
};
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_ethereum::{TransactionSigned, chainspec::MAINNET};
use reth_primitives_traits::{RecoveredBlock, SealedHeader};
use reth_provider::{
    BlockWriter, ProviderFactory,
    providers::{BlockchainProvider, RocksDBBuilder, StaticFileProvider},
    test_utils::MockNodeTypesWithDB,
};
use std::{path::PathBuf, sync::Arc};
use tokio::runtime::Builder;

// ---------------------------------------------------------------------------
// Shared test helpers
// ---------------------------------------------------------------------------

/// Builds a signed anchor transaction with the provided calldata.
fn build_test_anchor_tx(input: &Bytes) -> TransactionSigned {
    let tx = TxLegacy {
        chain_id: None,
        nonce: 0,
        gas_price: 1,
        gas_limit: 21_000,
        to: TxKind::Call(Address::ZERO),
        value: U256::ZERO,
        input: input.clone(),
    };

    TransactionSigned::new_unhashed(tx.into(), Signature::new(U256::from(1), U256::from(2), false))
}

/// Standard anchor calldata with the V4 selector prefix.
fn anchor_v4_input() -> Bytes {
    let mut input = ANCHOR_V4_SELECTOR.to_vec();
    input.extend_from_slice(&[0u8; 4]);
    Bytes::from(input)
}

/// Builds a ProviderFactory wired with both reth and Taiko tables for lookup tests.
fn create_taiko_test_provider_factory() -> ProviderFactory<MockNodeTypesWithDB> {
    struct TaikoTables;

    impl TableSet for TaikoTables {
        fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
            Box::new(
                Tables::ALL.iter().map(|table| Box::new(*table) as Box<dyn TableInfo>).chain(
                    alethia_reth_db::model::Tables::ALL
                        .iter()
                        .map(|table| Box::new(*table) as Box<dyn TableInfo>),
                ),
            )
        }
    }

    let (static_dir, _) = create_test_static_files_dir();
    let (rocksdb_dir, _) = create_test_rocksdb_dir();
    let path = tempdir_path();
    let db = init_db_for::<PathBuf, TaikoTables>(
        path.clone(),
        DatabaseArguments::new(ClientVersion::default()),
    )
    .expect("init db");
    let db = Arc::new(TempDatabase::new(db, path));
    ProviderFactory::new(
        db,
        MAINNET.clone(),
        StaticFileProvider::read_write(static_dir.keep()).expect("static file provider"),
        RocksDBBuilder::new(&rocksdb_dir)
            .with_default_tables()
            .build()
            .expect("failed to create test RocksDB provider"),
        reth::tasks::Runtime::default(),
    )
    .expect("failed to create test provider factory")
}

/// Creates a Taiko test provider factory and inserts a genesis block.
///
/// Returns the factory and an open writable provider for further block insertions.
/// The caller must commit the provider when done writing.
fn create_taiko_test_provider_factory_with_genesis() -> (
    ProviderFactory<MockNodeTypesWithDB>,
    reth_provider::DatabaseProviderRW<
        Arc<TempDatabase<reth_db::mdbx::DatabaseEnv>>,
        MockNodeTypesWithDB,
    >,
) {
    let factory = create_taiko_test_provider_factory();
    let provider_rw = factory.provider_rw().expect("provider rw");
    let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };
    let genesis_block = genesis_header.into_block(BlockBody::default());
    let genesis_recovered = RecoveredBlock::new_unhashed(genesis_block, vec![]);
    provider_rw.insert_block(&genesis_recovered).expect("insert genesis block");
    (factory, provider_rw)
}

/// Creates a `TaikoAuthExt` API backed by the given factory with the provided latest header.
fn create_test_api(
    factory: ProviderFactory<MockNodeTypesWithDB>,
    latest: Header,
) -> TaikoAuthExt<(), (), (), BlockchainProvider<MockNodeTypesWithDB>> {
    let sealed = SealedHeader::seal_slow(latest);
    let provider = BlockchainProvider::with_latest(factory, sealed).expect("provider");
    TaikoAuthExt::new(provider, (), (), ())
}

// ---------------------------------------------------------------------------
// Deserialization tests
// ---------------------------------------------------------------------------

#[test]
/// Ensures `txPoolContent` accepts a camelCase object payload.
fn tx_pool_content_params_deserialize_from_camel_case() {
    let value = serde_json::json!({
        "beneficiary": Address::from([0x11; 20]),
        "baseFee": 10u64,
        "blockMaxGasLimit": 15_000_000u64,
        "maxBytesPerTxList": 120_000u64,
        "locals": [Address::from([0x22; 20])],
        "maxTransactionsLists": 4u64
    });

    let params: TxPoolContentParams =
        serde_json::from_value(value).expect("txPoolContent params should deserialize");
    assert_eq!(params.base_fee, 10);
    assert_eq!(params.block_max_gas_limit, 15_000_000);
    assert_eq!(params.max_bytes_per_tx_list, 120_000);
    assert_eq!(params.max_transactions_lists, 4);
    assert_eq!(
        params.locals,
        Some(vec![Address::from([0x22; 20])]),
        "locals should preserve each address"
    );
}

#[test]
/// Ensures `txPoolContentWithMinTip` accepts a camelCase object payload.
fn tx_pool_content_with_min_tip_params_deserialize_from_camel_case() {
    let value = serde_json::json!({
        "beneficiary": Address::from([0x33; 20]),
        "baseFee": 20u64,
        "blockMaxGasLimit": 20_000_000u64,
        "maxBytesPerTxList": 240_000u64,
        "locals": [Address::from([0x44; 20]), Address::from([0x55; 20])],
        "maxTransactionsLists": 8u64,
        "minTip": 2u64
    });

    let params: TxPoolContentWithMinTipParams =
        serde_json::from_value(value).expect("txPoolContentWithMinTip params should deserialize");
    assert_eq!(params.base_fee, 20);
    assert_eq!(params.block_max_gas_limit, 20_000_000);
    assert_eq!(params.max_bytes_per_tx_list, 240_000);
    assert_eq!(params.max_transactions_lists, 8);
    assert_eq!(params.min_tip, 2);
    assert_eq!(
        params.locals,
        Some(vec![Address::from([0x44; 20]), Address::from([0x55; 20])]),
        "locals should preserve each address"
    );
}

#[test]
/// Ensures converting `txPoolContent` params defaults `min_tip` to zero.
fn tx_pool_content_params_conversion_defaults_min_tip_to_zero() {
    let params = TxPoolContentParams {
        beneficiary: Address::from([0x77; 20]),
        base_fee: 42,
        block_max_gas_limit: 15_000_000,
        max_bytes_per_tx_list: 120_000,
        locals: None,
        max_transactions_lists: 2,
    };
    let with_tip = TxPoolContentWithMinTipParams::from(params);
    assert_eq!(with_tip.min_tip, 0);
}

// ---------------------------------------------------------------------------
// Proposal ID decoding tests
// ---------------------------------------------------------------------------

#[test]
/// Confirms Shasta proposal IDs decode correctly from extra data.
fn parses_shasta_proposal_id_from_extra_data() {
    let extra = [0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
    assert_eq!(
        decode_shasta_proposal_id(&extra).map(U256::from),
        Some(U256::from(0x010203040506u64))
    );
}

#[test]
/// Ensures truncated extra data yields no proposal ID.
fn returns_none_for_truncated_extra_data() {
    assert!(decode_shasta_proposal_id(&[0x2a]).is_none());
}

// ---------------------------------------------------------------------------
// Batch lookup integration tests
// ---------------------------------------------------------------------------

#[test]
/// Returns uncertainty when the head L1 origin is missing.
fn returns_uncertain_when_head_l1_origin_missing() {
    let proposal_id = U256::from(0x010203040506u64);
    let extra: Bytes = vec![0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06].into();
    let input = anchor_v4_input();
    let signed = build_test_anchor_tx(&input);

    let header =
        Header { number: 1, gas_limit: 1_000_000, extra_data: extra, ..Default::default() };
    let body = BlockBody { transactions: vec![signed], ..Default::default() };
    let block = header.clone().into_block(body);
    let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);

    let (factory, provider_rw) = create_taiko_test_provider_factory_with_genesis();
    provider_rw.insert_block(&recovered).expect("insert block");
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, header);

    let err = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap_err();
    assert_eq!(err.code(), -32000);
    assert_eq!(
        err.message(),
        "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed",
    );
}

#[test]
/// Reports uncertainty when the matching proposal is at the head without a mapping.
fn returns_uncertain_when_match_at_head_without_mapping() {
    let proposal_id = U256::from(0x010203040506u64);
    let extra: Bytes = vec![0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06].into();
    let input = anchor_v4_input();
    let signed = build_test_anchor_tx(&input);

    let header =
        Header { number: 1, gas_limit: 1_000_000, extra_data: extra, ..Default::default() };
    let body = BlockBody { transactions: vec![signed], ..Default::default() };
    let block = header.clone().into_block(body);
    let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);

    let (factory, mut provider_rw) = create_taiko_test_provider_factory_with_genesis();
    provider_rw.insert_block(&recovered).expect("insert block");

    {
        let tx = provider_rw.tx_mut();
        tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, header.number)
            .expect("insert head l1 origin");
    }
    provider_rw.commit().expect("commit");

    let provider_ro = factory.provider().expect("provider ro");
    let head_number = provider_ro
        .into_tx()
        .get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)
        .expect("read head l1 origin");
    assert_eq!(head_number, Some(header.number));

    let api = create_test_api(factory, header);

    let err = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap_err();
    assert_eq!(err.code(), -32000);
    assert_eq!(
        err.message(),
        "proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed",
    );
}

#[test]
/// Verifies the production lookback limit constant.
fn uses_expected_lookback_limit_constant() {
    assert_eq!(MAX_BACKWARD_SCAN_BLOCKS, 192 * 21_600);
}

#[test]
/// Returns lookback-exceeded when scanning beyond the allowed limit.
fn returns_lookback_exceeded_when_scan_exceeds_limit() {
    let max_scan = max_backward_scan_blocks();
    let head_number = max_scan + 1;
    let target_batch_id = U256::from(1u64);
    let input = anchor_v4_input();

    let (factory, mut provider_rw) = create_taiko_test_provider_factory_with_genesis();

    let mut head_header = None;

    for number in 1..=head_number {
        let mut extra = [0u8; 7];
        extra[0] = 0x2a;
        extra[1..7].copy_from_slice(&number.to_be_bytes()[2..]);

        let header = Header {
            number,
            gas_limit: 1_000_000,
            extra_data: extra.to_vec().into(),
            ..Default::default()
        };
        let body =
            BlockBody { transactions: vec![build_test_anchor_tx(&input)], ..Default::default() };
        let block = header.clone().into_block(body);
        let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);
        provider_rw.insert_block(&recovered).expect("insert block");

        if number == head_number {
            head_header = Some(header);
        }
    }

    {
        let tx = provider_rw.tx_mut();
        let stored_origin = StoredL1Origin {
            block_id: U256::from(head_number),
            l2_block_hash: Default::default(),
            l1_block_height: U256::from(1u64),
            l1_block_hash: Default::default(),
            build_payload_args_id: [0u8; 8],
            is_forced_inclusion: false,
            signature: [0u8; 65],
        };
        tx.put::<StoredL1OriginTable>(head_number, stored_origin).expect("insert l1 origin");
        tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, head_number)
            .expect("insert head l1 origin");
    }
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, head_header.expect("head header"));

    let err = api.resolve_last_block_number_by_batch_id(target_batch_id).unwrap_err();
    assert_eq!(err.code(), -32000);
    assert_eq!(
        err.message(),
        "proposal last block lookback exceeded: BatchToLastBlockID missing and lookback limit reached",
    );
}

#[test]
/// Returns not-found when the proposal ID is lower than the target batch ID.
fn returns_not_found_when_proposal_id_below_batch_id() {
    let max_scan = max_backward_scan_blocks();
    let head_number = max_scan + 1;
    let target_batch_id = U256::from(head_number + 1);
    let input = anchor_v4_input();

    let (factory, provider_rw) = create_taiko_test_provider_factory_with_genesis();

    let mut head_header = None;

    for number in 1..=head_number {
        let mut extra = [0u8; 7];
        extra[0] = 0x2a;
        extra[1..7].copy_from_slice(&number.to_be_bytes()[2..]);

        let header = Header {
            number,
            gas_limit: 1_000_000,
            extra_data: extra.to_vec().into(),
            ..Default::default()
        };
        let body =
            BlockBody { transactions: vec![build_test_anchor_tx(&input)], ..Default::default() };
        let block = header.clone().into_block(body);
        let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);
        provider_rw.insert_block(&recovered).expect("insert block");

        if number == head_number {
            head_header = Some(header);
        }
    }
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, head_header.expect("head header"));

    let err = api.resolve_last_block_number_by_batch_id(target_batch_id).unwrap_err();
    assert_eq!(err.code(), -32000);
    assert_eq!(err.message(), "not found");
}

#[test]
/// Returns None when batch mapping exists but no L1 origin row is present.
fn returns_none_when_batch_mapping_exists_but_l1_origin_missing() {
    let batch_id = U256::from(1u64);
    let block_id = U256::from(7u64);
    let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };

    let (factory, mut provider_rw) = create_taiko_test_provider_factory_with_genesis();
    {
        let tx = provider_rw.tx_mut();
        tx.put::<BatchToLastBlock>(batch_id.to(), block_id.to()).expect("insert batch mapping");
    }
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, genesis_header);

    let resolved = api.resolve_last_block_number_by_batch_id(batch_id).unwrap();
    assert_eq!(resolved, block_id);
    assert_eq!(api.read_l1_origin_by_block_id(resolved).unwrap(), None);
}

#[test]
/// Returns `None` when the cached batch mapping is absent.
fn returns_none_for_last_certain_block_without_mapping() {
    let batch_id = U256::from(1u64);
    let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };

    let (factory, provider_rw) = create_taiko_test_provider_factory_with_genesis();
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, genesis_header);

    let resolved = api.read_cached_last_block_number_by_batch_id(batch_id).unwrap();
    assert_eq!(resolved, None);
}

#[test]
/// Returns the cached batch mapping when it exists.
fn returns_last_certain_block_from_mapping() {
    let batch_id = U256::from(1u64);
    let block_id = U256::from(7u64);
    let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };

    let (factory, mut provider_rw) = create_taiko_test_provider_factory_with_genesis();
    {
        let tx = provider_rw.tx_mut();
        tx.put::<BatchToLastBlock>(batch_id.to(), block_id.to()).expect("insert batch mapping");
    }
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, genesis_header);

    let resolved = api.read_cached_last_block_number_by_batch_id(batch_id).unwrap();
    assert_eq!(resolved, Some(block_id));
}

#[test]
/// Returns `None` when the cached batch mapping for the L1 origin is absent.
fn returns_none_for_last_certain_l1_origin_without_mapping() {
    let batch_id = U256::from(1u64);
    let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };

    let (factory, provider_rw) = create_taiko_test_provider_factory_with_genesis();
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, genesis_header);
    let runtime = Builder::new_current_thread().enable_all().build().expect("tokio runtime");

    let resolved = runtime
        .block_on(api.last_certain_l1_origin_by_batch_id(batch_id))
        .expect("resolve last certain l1 origin");
    assert_eq!(resolved, None);
}

#[test]
/// Returns the cached L1 origin when both the mapping and origin row exist.
fn returns_last_certain_l1_origin_from_mapping() {
    let batch_id = U256::from(1u64);
    let block_id = U256::from(7u64);
    let genesis_header = Header { number: 0, gas_limit: 1_000_000, ..Default::default() };

    let (factory, mut provider_rw) = create_taiko_test_provider_factory_with_genesis();
    {
        let tx = provider_rw.tx_mut();
        tx.put::<BatchToLastBlock>(batch_id.to(), block_id.to()).expect("insert batch mapping");
        tx.put::<StoredL1OriginTable>(
            block_id.to(),
            StoredL1Origin {
                block_id,
                l2_block_hash: Default::default(),
                l1_block_height: U256::from(3u64),
                l1_block_hash: Default::default(),
                build_payload_args_id: [0u8; 8],
                is_forced_inclusion: false,
                signature: [0u8; 65],
            },
        )
        .expect("insert l1 origin");
    }
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, genesis_header);
    let runtime = Builder::new_current_thread().enable_all().build().expect("tokio runtime");

    let resolved = runtime
        .block_on(api.last_certain_l1_origin_by_batch_id(batch_id))
        .expect("resolve last certain l1 origin")
        .expect("l1 origin should exist");
    assert_eq!(resolved.block_id, block_id);
    assert_eq!(resolved.l1_block_height, U256::from(3u64));
}

#[test]
/// Does not fall back to chain scanning when the cached batch mapping is absent.
fn last_certain_block_does_not_fallback_to_scanning() {
    let proposal_id = U256::from(0x010203040506u64);
    let extra: Bytes = vec![0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06].into();
    let input = anchor_v4_input();

    let (factory, mut provider_rw) = create_taiko_test_provider_factory_with_genesis();

    let header =
        Header { number: 1, gas_limit: 1_000_000, extra_data: extra, ..Default::default() };
    let body = BlockBody { transactions: vec![build_test_anchor_tx(&input)], ..Default::default() };
    let block = header.clone().into_block(body);
    let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);
    provider_rw.insert_block(&recovered).expect("insert block");

    {
        let tx = provider_rw.tx_mut();
        let confirmed_origin = StoredL1Origin {
            block_id: U256::from(1u64),
            l2_block_hash: Default::default(),
            l1_block_height: U256::from(1u64),
            l1_block_hash: Default::default(),
            build_payload_args_id: [0u8; 8],
            is_forced_inclusion: false,
            signature: [0u8; 65],
        };
        tx.put::<StoredL1OriginTable>(1, confirmed_origin).expect("insert confirmed l1 origin");
    }
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, header);

    let resolved = api.read_cached_last_block_number_by_batch_id(proposal_id).unwrap();
    assert_eq!(resolved, None);
    let fallback_resolved = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap();
    assert_eq!(fallback_resolved, U256::from(1u64));
}

#[test]
/// Returns invalid params when extra data is too short to decode the proposal ID.
fn returns_invalid_params_when_extra_data_too_short() {
    let batch_id = U256::from(1u64);
    let input = anchor_v4_input();

    let (factory, provider_rw) = create_taiko_test_provider_factory_with_genesis();

    let header = Header {
        number: 1,
        gas_limit: 1_000_000,
        extra_data: vec![0x2a].into(),
        ..Default::default()
    };
    let body = BlockBody { transactions: vec![build_test_anchor_tx(&input)], ..Default::default() };
    let block = header.clone().into_block(body);
    let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);
    provider_rw.insert_block(&recovered).expect("insert block");
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, header);

    let err = api.resolve_last_block_number_by_batch_id(batch_id).unwrap_err();
    assert_eq!(err.code(), -32602);
    assert_eq!(err.message(), "extraData too short for proposalId: 1");
}

#[test]
/// Skips preconfirmation blocks while scanning for the last block by batch ID.
fn skips_preconfirmation_blocks_when_scanning() {
    let proposal_id = U256::from(0x010203040506u64);
    let extra: Bytes = vec![0x2a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06].into();
    let input = anchor_v4_input();

    let (factory, mut provider_rw) = create_taiko_test_provider_factory_with_genesis();

    let mut head_header = None;

    for number in 1..=2u64 {
        let header = Header {
            number,
            gas_limit: 1_000_000,
            extra_data: extra.clone(),
            ..Default::default()
        };
        let body =
            BlockBody { transactions: vec![build_test_anchor_tx(&input)], ..Default::default() };
        let block = header.clone().into_block(body);
        let recovered = RecoveredBlock::new_unhashed(block, vec![Address::ZERO]);
        provider_rw.insert_block(&recovered).expect("insert block");

        if number == 2 {
            head_header = Some(header);
        }
    }

    {
        let tx = provider_rw.tx_mut();
        let confirmed_origin = StoredL1Origin {
            block_id: U256::from(1u64),
            l2_block_hash: Default::default(),
            l1_block_height: U256::from(1u64),
            l1_block_hash: Default::default(),
            build_payload_args_id: [0u8; 8],
            is_forced_inclusion: false,
            signature: [0u8; 65],
        };
        tx.put::<StoredL1OriginTable>(1, confirmed_origin).expect("insert confirmed l1 origin");
        let preconf_origin = StoredL1Origin {
            block_id: U256::from(2u64),
            l2_block_hash: Default::default(),
            l1_block_height: U256::ZERO,
            l1_block_hash: Default::default(),
            build_payload_args_id: [0u8; 8],
            is_forced_inclusion: false,
            signature: [0u8; 65],
        };
        tx.put::<StoredL1OriginTable>(2, preconf_origin).expect("insert preconf l1 origin");
    }
    provider_rw.commit().expect("commit");

    let api = create_test_api(factory, head_header.expect("head header"));

    let block_id = api.resolve_last_block_number_by_batch_id(proposal_id).unwrap();
    assert_eq!(block_id, U256::from(1u64));
}
