//! Proof-history sidecar configuration and startup wiring for Taiko nodes.

mod config;
mod live;
mod sidecar;
mod storage_init;

pub use config::{
    DEFAULT_PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS, DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL,
    DEFAULT_PROOF_HISTORY_WINDOW, ProofHistoryConfig,
};
use sidecar::ProofHistorySidecar;

use crate::TaikoNode;
use alethia_reth_rpc::{
    debug::{TaikoDebugWitnessApiServer, TaikoDebugWitnessExt},
    eth::proofs::{TaikoEthProofApiServer, TaikoEthProofExt},
    proof_state::ProofHistoryReadiness,
};
use eyre::WrapErr;
use reth::{
    providers::{
        BlockHashReader, BlockNumReader, BlockReader, CanonStateSubscriptions, DBProvider,
        DatabaseProviderFactory, HeaderProvider, StageCheckpointReader,
    },
    tasks::TaskExecutor,
};
use reth_db::{Database, database_metrics::DatabaseMetrics};
use reth_ethereum::EthPrimitives;
use reth_node_api::{FullNodeComponents, NodeAddOns};
use reth_node_builder::{
    NodeAdapter, NodeBuilderWithComponents, NodeComponentsBuilder, WithLaunchContext,
    rpc::{RethRpcAddOns, RpcContext},
};
use reth_optimism_trie::{OpProofsStorage, OpProofsStorageError, db::MdbxProofsStorageV2};
use reth_rpc_builder::RethRpcModule;
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_storage_api::{
    ChainStateBlockReader, ChangeSetReader, StorageChangeSetReader, StorageSettingsCache,
};
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::info;

/// Shared storage type used by proof-history indexing and debug RPC overrides.
pub type ProofHistoryStorage = OpProofsStorage<Arc<MdbxProofsStorageV2>>;

/// Adapts the storage bound accessors' `NoBlocksFound` error back to the `Option` shape the
/// reconciliation logic in this module was written against.
pub(crate) fn opt_block(
    result: Result<alloy_eips::NumHash, OpProofsStorageError>,
) -> Result<Option<(u64, alloy_primitives::B256)>, OpProofsStorageError> {
    match result {
        Ok(numhash) => Ok(Some((numhash.number, numhash.hash))),
        Err(OpProofsStorageError::NoBlocksFound) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Storage and reconciliation-readiness handles shared with the proof-history RPC overrides.
pub type ProofHistoryRpcHandles = (ProofHistoryStorage, ProofHistoryReadiness);

/// Result returned by proof-history installation with the updated node builder and optional
/// RPC handles.
pub type ProofHistoryInstallResult<T, CB, AO> = eyre::Result<(
    WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    Option<ProofHistoryRpcHandles>,
)>;

/// Installs the proof-history sidecar and proof database metrics task on a Taiko node builder.
pub fn install_proof_history<T, CB, AO>(
    node_builder: WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    config: ProofHistoryConfig,
) -> ProofHistoryInstallResult<T, CB, AO>
where
    T: reth_node_api::FullNodeTypes<Types = TaikoNode>,
    CB: NodeComponentsBuilder<T>,
    AO: NodeAddOns<NodeAdapter<T, CB::Components>> + RethRpcAddOns<NodeAdapter<T, CB::Components>>,
    AO::EthApi: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
    T::Provider: BlockHashReader
        + BlockNumReader
        + BlockReader
        + CanonStateSubscriptions
        + DatabaseProviderFactory,
    <T::Provider as DatabaseProviderFactory>::Provider: BlockNumReader
        + ChainStateBlockReader
        + ChangeSetReader
        + DBProvider
        + HeaderProvider
        + StageCheckpointReader
        + StorageChangeSetReader
        + StorageSettingsCache,
    <T::DB as Database>::TX: Sync,
{
    if !config.enabled {
        return Ok((node_builder, None));
    }

    let storage_path = config.required_storage_path()?.clone();
    storage_init::refuse_legacy_v1_storage(&storage_path)?;
    let mdbx =
        Arc::new(MdbxProofsStorageV2::new(&storage_path).wrap_err_with(|| {
            format!("failed to create proof-history MDBX at {storage_path:?}")
        })?);
    let storage: ProofHistoryStorage = Arc::clone(&mdbx).into();
    let storage_for_sidecar = storage.clone();
    let storage_for_init = Arc::clone(&mdbx);
    // Starts not-ready: the RPC layer must not serve proof-history state until the sidecar has
    // reconciled the stored bounds against canonical block hashes.
    let readiness = ProofHistoryReadiness::new();
    let readiness_for_sidecar = readiness.clone();

    Ok((
        node_builder.on_node_started(move |node| {
            let task_executor = node.task_executor.clone();
            spawn_proofs_db_metrics(
                task_executor.clone(),
                mdbx,
                node.config.metrics.push_gateway_interval,
            );
            let sidecar = ProofHistorySidecar::<NodeAdapter<T, CB::Components>, _>::new(
                node.provider,
                node.evm_config,
                task_executor.clone(),
                storage_for_sidecar,
                storage_for_init,
                config,
                readiness_for_sidecar,
            );
            task_executor.spawn_critical_with_graceful_shutdown_signal(
                "taiko::proof_history::sidecar",
                move |shutdown| {
                    Box::pin(async move {
                        if let Err(error) = sidecar.run(shutdown).await {
                            panic!("proof-history sidecar crashed: {error}");
                        }
                    })
                },
            );
            Ok(())
        }),
        Some((storage, readiness)),
    ))
}

/// Installs proof-history backed replacements for configured `eth_` and `debug_` RPC methods.
pub fn install_proof_history_rpc<Node, EthApi>(
    ctx: &mut RpcContext<'_, Node, EthApi>,
    handles: ProofHistoryRpcHandles,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    EthApi: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
    Node::Provider:
        HeaderProvider<Header = reth::primitives::Header> + Clone + Send + Sync + 'static,
{
    let (storage, readiness) = handles;
    let eth_ext =
        TaikoEthProofExt::new(ctx.registry.eth_api().clone(), storage.clone(), readiness.clone());
    ctx.modules.add_or_replace_if_module_configured(RethRpcModule::Eth, eth_ext.into_rpc())?;

    let debug_ext = TaikoDebugWitnessExt::new(
        ctx.node().provider().clone(),
        ctx.registry.eth_api().clone(),
        storage,
        readiness,
    );
    ctx.modules.add_or_replace_if_module_configured(RethRpcModule::Debug, debug_ext.into_rpc())?;
    Ok(())
}

/// Spawns periodic metric collection for the proof-history MDBX database.
fn spawn_proofs_db_metrics(
    executor: TaskExecutor,
    storage: Arc<MdbxProofsStorageV2>,
    metrics_report_interval: Duration,
) {
    executor.spawn_critical_task("taiko-proofs-storage-metrics", async move {
        info!(
            target: "reth::taiko::proof_history",
            ?metrics_report_interval,
            "starting proof-history metrics task"
        );

        loop {
            sleep(metrics_report_interval).await;
            storage.report_metrics();
        }
    });
}
