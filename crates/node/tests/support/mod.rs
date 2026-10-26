use alethia_reth_chainspec::{
    hardfork::{TAIKO_DEVNET_HARDFORKS, TaikoHardfork},
    spec::TaikoChainSpec,
};
use alethia_reth_node::TaikoNode;
use alethia_reth_primitives::payload::attributes::{
    RpcL1Origin, TaikoBlockMetadata, TaikoPayloadAttributes,
};
use alethia_reth_rpc::eth::auth::{TaikoAuthExt, TaikoAuthExtApiServer};
use alloy_eips::{eip2935, eip4788};
use alloy_genesis::Genesis;
use alloy_hardforks::ForkCondition;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV5, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes,
};
use jsonrpsee::{core::client::ClientT, rpc_params};
use reth::{args::RpcServerArgs, rpc::server_types::RpcModuleSelection, tasks::Runtime};
use reth_chainspec::ChainSpec;
use reth_db::{
    ClientVersion, DatabaseEnv, TableSet, Tables,
    mdbx::{DatabaseArguments, init_db_for},
    table::TableInfo,
    test_utils::{TempDatabase, tempdir_path},
};
use reth_node_api::{FullNodeComponents, TreeConfig};
use reth_node_builder::{
    EngineNodeLauncher, FullNode, Node, NodeAdapter, NodeBuilder, NodeConfig, RethFullAdapter,
};
use reth_provider::providers::BlockchainProvider;
use reth_rpc::eth::EthApiTypes;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};

type TestTypes = RethFullAdapter<Arc<TempDatabase<DatabaseEnv>>, TaikoNode>;
pub type TestNode = FullNode<NodeAdapter<TestTypes>, <TaikoNode as Node<TestTypes>>::AddOns>;

pub fn fixture_chain_spec() -> Arc<TaikoChainSpec> {
    let genesis: Genesis =
        serde_json::from_str(include_str!("../fixtures/etna-genesis.json")).unwrap();
    let mut forks = TAIKO_DEVNET_HARDFORKS.clone();
    forks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(100));
    let mut inner = ChainSpec::builder()
        .chain(genesis.config.chain_id.into())
        .genesis(genesis)
        .with_forks(forks)
        .build();
    inner.paris_block_and_final_difficulty = Some((0, U256::ZERO));
    Arc::new(TaikoChainSpec { inner })
}

pub fn fixture_attributes(timestamp: u64) -> TaikoPayloadAttributes {
    TaikoPayloadAttributes {
        payload_attributes: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::with_last_byte(0x42),
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(if timestamp >= 100 {
                B256::with_last_byte(1)
            } else {
                B256::ZERO
            }),
            slot_number: None,
            target_gas_limit: None,
        },
        base_fee_per_gas: U256::from(1),
        block_metadata: TaikoBlockMetadata {
            beneficiary: Address::with_last_byte(0x42),
            gas_limit: 30_000_000,
            timestamp: U256::from(timestamp),
            mix_hash: B256::ZERO,
            tx_list: Some(Bytes::from_static(&[0xc0])),
            // Etna targets carry proposal ID 0 and L1 anchor block number 1.
            extra_data: if timestamp >= 100 {
                Bytes::from_static(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
            } else {
                Bytes::from_static(&[0; 7])
            },
        },
        l1_origin: RpcL1Origin {
            block_id: U256::from(1),
            l2_block_hash: B256::ZERO,
            l1_block_height: Some(U256::from(1)),
            l1_block_hash: Some(B256::with_last_byte(2)),
            build_payload_args_id: [0; 8],
            is_forced_inclusion: false,
            signature: [0; 65],
        },
        anchor_transaction: None,
    }
}

pub fn normalize_v5(envelope: ExecutionPayloadEnvelopeV5) -> Value {
    let difficulty = u64::try_from(envelope.block_value).expect("zk gas fits u64");
    let mut payload = serde_json::to_value(envelope.execution_payload).unwrap();
    payload["headerDifficulty"] = difficulty.into();
    payload
}

struct TaikoTables;
impl TableSet for TaikoTables {
    fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
        Box::new(Tables::ALL.iter().map(|t| Box::new(*t) as Box<dyn TableInfo>).chain(
            alethia_reth_db::model::Tables::ALL.iter().map(|t| Box::new(*t) as Box<dyn TableInfo>),
        ))
    }
}

pub async fn launch_test_node(chain_spec: Arc<TaikoChainSpec>) -> eyre::Result<TestNode> {
    let runtime = Runtime::test();
    TEST_RUNTIMES.lock().unwrap().push(runtime.clone());
    let path = tempdir_path();
    let database: Arc<TempDatabase<DatabaseEnv>> = Arc::new(TempDatabase::new(
        init_db_for::<PathBuf, TaikoTables>(
            path.clone(),
            DatabaseArguments::new(ClientVersion::default()),
        )?,
        path.clone(),
    ));
    let mut config =
        NodeConfig::new(chain_spec).with_unused_ports().with_disabled_discovery().with_rpc(
            RpcServerArgs::default()
                .with_unused_ports()
                .with_http()
                .with_http_api(RpcModuleSelection::All),
        );
    config.datadir.datadir = path.join("node").into();
    config.network.bootnodes = Some(vec![]);
    config.network.trusted_only = true;
    config.network.no_persist_peers = true;
    let taiko = TaikoNode;
    let builder = NodeBuilder::new(config)
        .with_database(database)
        .with_launch_context(runtime)
        .with_types_and_provider::<TaikoNode, BlockchainProvider<_>>()
        .with_components(taiko.components_builder())
        .with_add_ons(taiko.add_ons());
    let (builder, handles) = alethia_reth_node::proof_history::install_proof_history(
        builder,
        alethia_reth_node::proof_history::ProofHistoryConfig {
            enabled: true,
            storage_path: Some(path.join("proof-history")),
            ..alethia_reth_node::proof_history::ProofHistoryConfig::disabled()
        },
    )?;
    let builder = builder.extend_rpc_modules(move |mut ctx| {
        // Mirror the binary's authenticated `taikoAuth` registration.
        let taiko_auth = TaikoAuthExt::new(
            ctx.node().provider().clone(),
            ctx.node().pool().clone(),
            ctx.registry.eth_api().converter().clone(),
            ctx.node().evm_config().clone(),
        );
        ctx.auth_module.merge_auth_methods(taiko_auth.into_rpc())?;
        alethia_reth_node::proof_history::install_proof_history_rpc(&mut ctx, handles.unwrap())
    });
    let handle = tokio::time::timeout(
        Duration::from_secs(30),
        builder.launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                builder.task_executor().clone(),
                builder.config().datadir(),
                TreeConfig::default().with_cross_block_cache_size(1024 * 1024),
            );
            builder.launch_with(launcher)
        }),
    )
    .await??;
    Ok(handle.node)
}

pub async fn fcu(
    client: &impl ClientT,
    genesis: B256,
    head: B256,
    attrs: Option<TaikoPayloadAttributes>,
) -> eyre::Result<ForkchoiceUpdated> {
    Ok(tokio::time::timeout(
        Duration::from_secs(30),
        client.request(
            "engine_forkchoiceUpdatedV3",
            rpc_params![
                ForkchoiceState {
                    head_block_hash: head,
                    safe_block_hash: genesis,
                    finalized_block_hash: genesis
                },
                attrs
            ],
        ),
    )
    .await??)
}

pub fn signed_tx(
    nonce: u64,
    anchor: bool,
    target: Address,
    input: Bytes,
) -> reth_ethereum_primitives::TransactionSigned {
    use alloy_consensus::{SignableTransaction, Signed, TxEip1559};
    use alloy_signer::SignerSync;
    let signer: alloy_signer_local::PrivateKeySigner =
        "0x92954368afd3caa1f3ce3ead0069c1af414054aefe1ef9aeacc1bf426222ce38".parse().unwrap();
    let tx = TxEip1559 {
        chain_id: 167001,
        nonce,
        gas_limit: 1_000_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: if anchor { 0 } else { 7 },
        to: target.into(),
        value: U256::ZERO,
        access_list: Default::default(),
        input,
    };
    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    Signed::new_unhashed(tx, signature).into()
}

pub fn with_txs(
    mut attrs: TaikoPayloadAttributes,
    txs: &[reth_ethereum_primitives::TransactionSigned],
) -> TaikoPayloadAttributes {
    attrs.block_metadata.tx_list = Some(alloy_rlp::encode(txs.to_vec()).into());
    attrs
}

pub struct Built {
    pub id: alloy_rpc_types_engine::PayloadId,
    pub payload: Value,
    pub root: B256,
    pub block: reth_primitives_traits::SealedBlock<reth_ethereum_primitives::Block>,
    pub attrs: TaikoPayloadAttributes,
}

pub async fn build(
    client: &impl ClientT,
    spec: Arc<TaikoChainSpec>,
    parent: &alloy_consensus::Header,
    mut attrs: TaikoPayloadAttributes,
) -> eyre::Result<Built> {
    use alethia_reth_consensus::eip4396::{MIN_BASE_FEE, calculate_next_block_eip4396_base_fee};
    use alethia_reth_primitives::engine::{TaikoEngineTypes, osaka::TaikoExecutionPayloadV3};
    use reth_chainspec::EthChainSpec;
    use reth_node_api::PayloadValidator;
    assert!(parent.number <= 1, "build() assumes the grandparent is genesis at timestamp 0");
    // Fixture chains are at most two blocks deep, so the grandparent is genesis at timestamp 0.
    attrs.base_fee_per_gas = U256::from(calculate_next_block_eip4396_base_fee(
        parent,
        parent.timestamp,
        parent.base_fee_per_gas.unwrap(),
        MIN_BASE_FEE,
    ));
    attrs.l1_origin.block_id = U256::from(parent.number + 1);
    let status = fcu(client, spec.genesis_hash(), parent.hash_slow(), Some(attrs.clone())).await?;
    assert!(status.payload_status.status.is_valid(), "{status:?}");
    let id = status.payload_id.unwrap();
    let root = attrs.payload_attributes.parent_beacon_block_root.unwrap();
    let envelope: ExecutionPayloadEnvelopeV5 = tokio::time::timeout(
        Duration::from_secs(30),
        client.request("engine_getPayloadV5", rpc_params![id]),
    )
    .await??;
    let payload = normalize_v5(envelope);
    let wire: TaikoExecutionPayloadV3 = serde_json::from_value(payload.clone())?;
    let data = wire.into_execution_data(vec![], root, vec![])?;
    let validator = alethia_reth_rpc::engine::validator::TaikoEngineValidator::new(spec);
    let block =
        <_ as PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(&validator, data)?;
    Ok(Built { id, payload, root, block, attrs })
}

pub async fn import(
    client: &impl ClientT,
    built: &Built,
) -> eyre::Result<alloy_rpc_types_engine::PayloadStatus> {
    let request = client.request(
        "engine_newPayloadV4",
        rpc_params![&built.payload, Vec::<B256>::new(), built.root, Vec::<Bytes>::new()],
    );
    Ok(tokio::time::timeout(Duration::from_secs(30), request).await??)
}

pub async fn canonicalize(client: &impl ClientT, genesis: B256, built: &Built) -> eyre::Result<()> {
    let status = import(client, built).await?;
    assert!(status.status.is_valid(), "{status:?}");
    let status = fcu(client, genesis, built.block.hash(), None).await?;
    assert!(status.payload_status.status.is_valid(), "{status:?}");
    Ok(())
}

pub async fn assert_execution_parity(node: &TestNode, built: &Built) -> eyre::Result<()> {
    use alethia_reth_block::derived_block::{assemble_filtered_block, execute_derived_block};
    use reth_revm::database::StateProviderDatabase;
    use reth_storage_api::{HeaderProvider, StateProviderFactory, StateRootProvider};
    let parent = node.provider.sealed_header_by_hash(built.block.parent_hash)?.unwrap();
    let state = node.provider.history_by_block_hash(parent.hash())?;
    let recovered = built.block.clone().try_recover()?;
    let derived = execute_derived_block(
        &node.evm_config,
        &parent,
        &recovered,
        StateProviderDatabase::new(&*state),
    )?;
    assert_eq!(U256::from(derived.finalized_block_zk_gas), built.block.difficulty);
    let root = state.state_root(derived.hashed_state)?;
    assert_eq!(root, built.block.state_root);
    let assembled = assemble_filtered_block(
        &node.evm_config,
        &parent,
        &recovered,
        derived.committed_transactions,
        &derived.execution_result,
        derived.finalized_block_zk_gas,
        root,
    )?;
    assert_eq!(assembled.header(), built.block.header());
    let http = node.rpc_server_handle().http_client().unwrap();
    let witness: alloy_rpc_types_debug::ExecutionWitness =
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let result = http
                    .request("debug_executionWitnessByBlockHash", rpc_params![built.block.hash()])
                    .await;
                match result {
                    Ok(witness) => break Ok::<_, eyre::Report>(witness),
                    Err(error) if error.to_string().contains("not ready") => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(error) => break Err(error.into()),
                }
            }
        })
        .await??;
    let replay: alloy_rpc_types_debug::ExecutionWitness = tokio::time::timeout(
        Duration::from_secs(30),
        http.request(
            "debug_executionWitnessForTxList",
            rpc_params![
                alloy_eips::BlockId::hash(built.block.hash()),
                built.attrs.block_metadata.tx_list.clone().unwrap()
            ],
        ),
    )
    .await??;
    // Witness node ordering is not contractual; compare the exact sets of proof preimages.
    let normalized = |mut witness: alloy_rpc_types_debug::ExecutionWitness| {
        witness.state.sort();
        witness.codes.sort();
        witness.keys.sort();
        witness.headers.sort();
        witness
    };
    assert_eq!(normalized(witness.clone()), normalized(replay));
    assert!(!witness.state.is_empty());
    assert!(!witness.headers.is_empty());
    for (address, code) in [
        (eip4788::BEACON_ROOTS_ADDRESS, eip4788::BEACON_ROOTS_CODE.clone()),
        (eip2935::HISTORY_STORAGE_ADDRESS, eip2935::HISTORY_STORAGE_CODE.clone()),
    ] {
        assert!(witness.codes.contains(&code), "missing {address} code");
        assert!(
            witness.keys.contains(&Bytes::copy_from_slice(address.as_slice())),
            "missing {address} account key"
        );
    }
    for slot in [
        built.block.timestamp % 8191,
        built.block.timestamp % 8191 + 8191,
        (built.block.number - 1) % 8191,
    ] {
        assert!(
            witness.keys.contains(&Bytes::copy_from_slice(&U256::from(slot).to_be_bytes::<32>())),
            "missing storage key {slot}"
        );
    }
    Ok(())
}

static TEST_RUNTIMES: std::sync::Mutex<Vec<Runtime>> = std::sync::Mutex::new(Vec::new());

struct RuntimeCleanup;
impl Drop for RuntimeCleanup {
    fn drop(&mut self) {
        // Engine tree/persistence workers keep MDBX alive until their shutdown signal fires.
        // This guard runs before Tokio is dropped, including while unwinding a failed assertion.
        for runtime in TEST_RUNTIMES.lock().unwrap().drain(..) {
            // Never panic again while unwinding; the explicit normal-path shutdown below
            // reports timeouts as test errors.
            runtime.graceful_shutdown_with_timeout(Duration::from_secs(5));
        }
    }
}

pub fn run_live_test(
    future: impl std::future::Future<Output = eyre::Result<()>>,
) -> eyre::Result<()> {
    // Serialize across cargo test threads and nextest processes. Hold the lock until both
    // Reth's worker threads and Tokio have shut down, so MDBX mappings cannot accumulate.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(std::env::temp_dir().join("alethia-reth-etna-engine-tests.lock"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(error) => return Err(error.into()),
        }
    }
    let tokio = tokio::runtime::Runtime::new()?;
    let _cleanup = RuntimeCleanup;
    let result =
        tokio.block_on(async { tokio::time::timeout(Duration::from_secs(60), future).await? });
    let mut shut_down = true;
    for runtime in TEST_RUNTIMES.lock().unwrap().drain(..) {
        shut_down &= runtime.graceful_shutdown_with_timeout(Duration::from_secs(5));
    }
    eyre::ensure!(shut_down, "node shutdown timed out");
    result
}
