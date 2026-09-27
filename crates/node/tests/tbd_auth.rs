//! JWT RPC regressions for target-block context in transaction-pool preselection.

#[allow(dead_code)]
mod support;

use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_node::TaikoNode;
use alethia_reth_rpc::eth::auth::{TaikoAuthExt, TaikoAuthExtApiServer};
use alloy_consensus::Transaction;
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, U256};
use jsonrpsee::core::client::{ClientT, Error as ClientError};
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_db::{
    ClientVersion, DatabaseEnv, TableSet, Tables,
    mdbx::{DatabaseArguments, init_db_for},
    table::TableInfo,
    test_utils::{TempDatabase, tempdir_path},
};
use reth_e2e_test_utils::{NodeHelperType, node::NodeTestContext};
use reth_node_api::{FullNodeComponents, TreeConfig};
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig};
use reth_node_core::args::RpcServerArgs;
use reth_provider::providers::BlockchainProvider;
use reth_rpc::eth::EthApiTypes;
use reth_rpc_server_types::RpcModuleSelection;
use reth_tasks::Runtime;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use support::{
    build, canonicalize, fixture_attributes, fixture_chain_spec, historical_chain_spec,
    run_live_test, signed_tx,
};

/// Tables required by the real Engine and auth RPC implementations.
struct AuthTables;

impl TableSet for AuthTables {
    fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
        Box::new(Tables::ALL.iter().map(|t| Box::new(*t) as Box<dyn TableInfo>).chain(
            alethia_reth_db::model::Tables::ALL.iter().map(|t| Box::new(*t) as Box<dyn TableInfo>),
        ))
    }
}

/// Stops node workers before the surrounding Tokio runtime and database are dropped.
struct AuthRuntime(Runtime);

impl Drop for AuthRuntime {
    fn drop(&mut self) {
        self.0.graceful_shutdown_with_timeout(Duration::from_secs(5));
    }
}

/// Launches the same authenticated extension registration used by the node binary.
async fn launch_auth_node(
    chain_spec: Arc<TaikoChainSpec>,
    runtime: Runtime,
) -> eyre::Result<NodeHelperType<TaikoNode>> {
    let path = tempdir_path();
    let database: Arc<TempDatabase<DatabaseEnv>> = Arc::new(TempDatabase::new(
        init_db_for::<PathBuf, AuthTables>(
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
        .with_add_ons(taiko.add_ons())
        .extend_rpc_modules(|ctx| {
            let api = TaikoAuthExt::new(
                ctx.node().provider().clone(),
                ctx.node().pool().clone(),
                ctx.registry.eth_api().converter().clone(),
                ctx.node().evm_config().clone(),
            );
            ctx.auth_module.merge_auth_methods(api.into_rpc())?;
            Ok(())
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
    NodeTestContext::new(handle.node, fixture_attributes).await
}

/// Sends old or extended positional arguments through the real JWT HTTP client.
async fn pool_content(
    client: &impl ClientT,
    with_tip: bool,
    context: Option<Value>,
) -> Result<Value, ClientError> {
    let mut params = vec![
        json!(Address::with_last_byte(0x42)),
        json!(1),
        json!(30_000_000),
        json!(1_000_000),
        Value::Null,
        json!(1),
    ];
    let method = if with_tip {
        params.push(json!(0));
        "taikoAuth_txPoolContentWithMinTip"
    } else {
        "taikoAuth_txPoolContent"
    };
    if let Some(context) = context {
        params.push(context);
    }
    client.request(method, params).await
}

/// Encodes a target context using the documented JSON quantity and hex fields.
fn context(timestamp: u64, root: B256, extra_data: Bytes) -> Value {
    json!({
        "timestamp": format!("0x{timestamp:x}"),
        "parentBeaconBlockRoot": root,
        "extraData": extra_data,
    })
}

/// Checks the public error class and identifies the rejected context field.
fn assert_invalid_params(result: Result<Value, ClientError>, field: &str) {
    let ClientError::Call(error) = result.expect_err("malformed context must be rejected") else {
        panic!("expected a JSON-RPC error response, not a transport failure");
    };
    assert_eq!(error.code(), -32602, "{error}");
    assert!(
        error.message().contains(field) ||
            error.data().is_some_and(|data| data.get().contains(field)),
        "expected {field} diagnostic: {error}"
    );
}

#[test]
fn auth_pool_rpc_preserves_legacy_arity_and_trailing_null() -> eyre::Result<()> {
    run_live_test(async {
        let runtime = AuthRuntime(Runtime::test());
        let node = launch_auth_node(fixture_chain_spec(), runtime.0.clone()).await?;
        let client = node.auth_server_handle().http_client();
        let tx = signed_tx(0, false, Address::with_last_byte(0x21), Bytes::new());
        let hash = node.rpc.inject_tx(tx.encoded_2718().into()).await?;
        for with_tip in [false, true] {
            let old = pool_content(&client, with_tip, None).await?;
            assert_eq!(old[0]["txList"].as_array().unwrap().len(), 1);
            assert_eq!(old[0]["txList"][0]["hash"], json!(hash));
            let null = pool_content(&client, with_tip, Some(Value::Null)).await?;
            assert_eq!(null, old);
        }
        Ok(())
    })
}

#[test]
fn auth_pool_rpc_rejects_invalid_target_context_at_tbd_boundary() -> eyre::Result<()> {
    run_live_test(async {
        let runtime = AuthRuntime(Runtime::test());
        let spec = fixture_chain_spec();
        let node = launch_auth_node(spec.clone(), runtime.0.clone()).await?;
        let client = node.auth_server_handle().http_client();
        let parent =
            build(&client, spec.clone(), spec.genesis_header(), 0, fixture_attributes(99)).await?;
        canonicalize(&client, spec.genesis_hash(), &parent).await?;
        let valid = context(100, B256::with_last_byte(7), Bytes::from(vec![0; 7]));
        for with_tip in [false, true] {
            let mut zero_root = valid.clone();
            zero_root["parentBeaconBlockRoot"] = json!(B256::ZERO);
            assert_invalid_params(
                pool_content(&client, with_tip, Some(zero_root)).await,
                "parentBeaconBlockRoot",
            );
            let mut missing_root = valid.clone();
            missing_root.as_object_mut().unwrap().remove("parentBeaconBlockRoot");
            assert_invalid_params(
                pool_content(&client, with_tip, Some(missing_root)).await,
                "parentBeaconBlockRoot",
            );
            for length in [6, 8] {
                let mut bad_extra = valid.clone();
                bad_extra["extraData"] = json!(Bytes::from(vec![0; length]));
                assert_invalid_params(
                    pool_content(&client, with_tip, Some(bad_extra)).await,
                    "extraData",
                );
            }
            let mut stale = valid.clone();
            stale["timestamp"] = json!("0x63");
            assert_invalid_params(pool_content(&client, with_tip, Some(stale)).await, "timestamp");
            pool_content(&client, with_tip, Some(valid.clone())).await?;
        }
        let activated =
            build(&client, spec.clone(), parent.block.header(), 0, fixture_attributes(100)).await?;
        canonicalize(&client, spec.genesis_hash(), &activated).await?;
        for with_tip in [false, true] {
            for omitted in [None, Some(Value::Null)] {
                assert_invalid_params(
                    pool_content(&client, with_tip, omitted).await,
                    "blockContext",
                );
            }
            pool_content(
                &client,
                with_tip,
                Some(context(101, B256::with_last_byte(8), Bytes::from(vec![0; 7]))),
            )
            .await?;
        }
        Ok(())
    })
}

#[test]
fn auth_pool_rpc_rejects_oversized_pre_shasta_metadata_without_panicking() -> eyre::Result<()> {
    run_live_test(async {
        let runtime = AuthRuntime(Runtime::test());
        let node = launch_auth_node(historical_chain_spec(1), runtime.0.clone()).await?;
        let client = node.auth_server_handle().http_client();
        for with_tip in [false, true] {
            for byte in [0, 1] {
                let oversized = context(1, B256::ZERO, Bytes::from(vec![byte; 33]));
                assert_invalid_params(
                    pool_content(&client, with_tip, Some(oversized)).await,
                    "extraData",
                );
                // The same server must answer another valid request after each malformed input.
                pool_content(
                    &client,
                    with_tip,
                    Some(context(1, B256::ZERO, Bytes::from(vec![0; 32]))),
                )
                .await?;
            }
        }
        Ok(())
    })
}

/// Adds a contract that consumes all gas unless 4788 returns the calldata-specified root.
fn root_reader_chain_spec() -> Arc<TaikoChainSpec> {
    let base = fixture_chain_spec();
    let mut genesis = base.inner.genesis.clone();
    // Store the requested timestamp, STATICCALL the canonical beacon-roots contract, then
    // compare its return value with calldata[32..64]. INVALID distinguishes a missing/wrong
    // record from success through the RPC's estimatedGasUsed without exposing simulated state.
    let mut code = vec![
        0x60, 0x00, 0x35, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0x60, 0x20, 0x60, 0x00, 0x73,
    ];
    code.extend_from_slice(alloy_eips::eip4788::BEACON_ROOTS_ADDRESS.as_slice());
    code.extend_from_slice(&[0x5a, 0xfa, 0x60, 0x00, 0x51, 0x60, 0x20, 0x35, 0x14, 0x16]);
    let success = u8::try_from(code.len() + 4).unwrap();
    code.extend_from_slice(&[0x60, success, 0x57, 0xfe, 0x5b, 0x00]);
    genesis.alloc.insert(
        Address::with_last_byte(0x31),
        alloy_genesis::GenesisAccount {
            nonce: Some(1),
            code: Some(code.into()),
            ..Default::default()
        },
    );
    let mut inner = ChainSpec::builder()
        .chain(base.inner.chain)
        .genesis(genesis)
        .with_forks(base.inner.hardforks.clone())
        .build();
    inner.paris_block_and_final_difficulty = Some((0, U256::ZERO));
    Arc::new(TaikoChainSpec { inner })
}

#[test]
fn auth_pool_rpc_executes_with_the_supplied_target_timestamp_and_root() -> eyre::Result<()> {
    run_live_test(async {
        let runtime = AuthRuntime(Runtime::test());
        let spec = root_reader_chain_spec();
        let node = launch_auth_node(spec.clone(), runtime.0.clone()).await?;
        let client = node.auth_server_handle().http_client();
        let parent =
            build(&client, spec.clone(), spec.genesis_header(), 0, fixture_attributes(99)).await?;
        canonicalize(&client, spec.genesis_hash(), &parent).await?;
        let mut input = U256::from(100).to_be_bytes::<32>().to_vec();
        input.extend_from_slice(B256::with_last_byte(7).as_slice());
        let tx = signed_tx(0, false, Address::with_last_byte(0x31), input.into());
        let gas_limit = tx.gas_limit();
        let hash = node.rpc.inject_tx(tx.encoded_2718().into()).await?;
        for with_tip in [false, true] {
            for (timestamp, root, succeeds) in [
                (100, B256::with_last_byte(7), true),
                (100, B256::with_last_byte(8), false),
                (101, B256::with_last_byte(7), false),
            ] {
                let result = pool_content(
                    &client,
                    with_tip,
                    Some(context(timestamp, root, Bytes::from(vec![0; 7]))),
                )
                .await?;
                assert_eq!(result[0]["txList"].as_array().unwrap().len(), 1, "{result}");
                assert_eq!(result[0]["txList"][0]["hash"], json!(hash));
                let gas = result[0]["estimatedGasUsed"].as_u64().unwrap();
                if succeeds {
                    assert!(gas < 100_000, "4788 target root was not visible: {result}");
                } else {
                    assert_eq!(gas, gas_limit, "unexpected 4788 target root: {result}");
                }
            }
        }
        Ok(())
    })
}
