//! Proof-history sidecar configuration and ExEx wiring for Taiko nodes.

use crate::TaikoNode;
use alethia_reth_rpc::debug::{TaikoDebugWitnessApiServer, TaikoDebugWitnessExt};
use eyre::{WrapErr, eyre};
use reth::{providers::HeaderProvider, tasks::TaskExecutor};
use reth_db::database_metrics::DatabaseMetrics;
use reth_node_api::{FullNodeComponents, NodeAddOns};
use reth_node_builder::{
    NodeAdapter, NodeBuilderWithComponents, NodeComponentsBuilder, WithLaunchContext,
    rpc::{RethRpcAddOns, RpcContext},
};
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_trie::{OpProofsStorage, db::MdbxProofsStorage};
use reth_rpc_eth_api::helpers::FullEthApi;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::info;

/// Default proof-history retention window in blocks.
const DEFAULT_PROOF_HISTORY_WINDOW: u64 = 1_296_000;

/// Default interval between proof-history prune passes.
const DEFAULT_PROOF_HISTORY_PRUNE_INTERVAL: Duration = Duration::from_secs(15);

/// Default interval, in blocks, between proof-history consistency checks.
const DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL: u64 = 0;

/// Configuration for the optional proof-history execution extension.
#[derive(Debug, Clone)]
pub struct ProofHistoryConfig {
    /// Whether proof-history indexing is installed on the node.
    pub enabled: bool,
    /// Filesystem path for the MDBX proof-history database.
    pub storage_path: Option<PathBuf>,
    /// Number of recent blocks retained in proof-history storage.
    pub window: u64,
    /// Wall-clock interval between proof-history prune passes.
    pub prune_interval: Duration,
    /// Block interval between proof-history consistency checks; zero disables verification.
    pub verification_interval: u64,
}

/// Shared storage type used by proof-history indexing and debug RPC overrides.
pub type ProofHistoryStorage = OpProofsStorage<Arc<MdbxProofsStorage>>;

/// Result returned by proof-history installation with the updated node builder and optional handle.
pub type ProofHistoryInstallResult<T, CB, AO> = eyre::Result<(
    WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    Option<ProofHistoryHandle>,
)>;

/// Handle returned when proof-history indexing is installed.
#[derive(Debug, Clone)]
pub struct ProofHistoryHandle {
    /// Shared storage handle used by both the ExEx and optional RPC replacement.
    storage: ProofHistoryStorage,
}

impl ProofHistoryHandle {
    /// Creates a proof-history handle from initialized shared storage.
    fn new(storage: ProofHistoryStorage) -> Self {
        Self { storage }
    }

    /// Returns a clone of the storage handle for components that need proof-history access.
    pub fn storage(&self) -> ProofHistoryStorage {
        self.storage.clone()
    }
}

impl ProofHistoryConfig {
    /// Returns a disabled proof-history configuration with production defaults.
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            storage_path: None,
            window: DEFAULT_PROOF_HISTORY_WINDOW,
            prune_interval: DEFAULT_PROOF_HISTORY_PRUNE_INTERVAL,
            verification_interval: DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL,
        }
    }

    /// Returns the configured storage path or an error if proof history requires one.
    pub fn required_storage_path(&self) -> eyre::Result<&PathBuf> {
        self.storage_path.as_ref().ok_or_else(|| {
            eyre!("proof-history storage path is required when proof history is enabled")
        })
    }
}

/// Installs the proof-history ExEx and proof database metrics task on a Taiko node builder.
pub fn install_proof_history<T, CB, AO>(
    node_builder: WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>,
    config: ProofHistoryConfig,
) -> ProofHistoryInstallResult<T, CB, AO>
where
    T: reth_node_api::FullNodeTypes<Types = TaikoNode>,
    CB: NodeComponentsBuilder<T>,
    AO: NodeAddOns<NodeAdapter<T, CB::Components>> + RethRpcAddOns<NodeAdapter<T, CB::Components>>,
    AO::EthApi: FullEthApi + Send + Sync + 'static,
{
    if !config.enabled {
        return Ok((node_builder, None));
    }

    let storage_path = config.required_storage_path()?.clone();
    let mdbx =
        Arc::new(MdbxProofsStorage::new(&storage_path).wrap_err_with(|| {
            format!("failed to create proof-history MDBX at {storage_path:?}")
        })?);
    let storage: ProofHistoryStorage = Arc::clone(&mdbx).into();
    let handle = ProofHistoryHandle::new(storage.clone());
    let storage_for_exex = storage.clone();

    Ok((
        node_builder
            .on_node_started(move |node| {
                spawn_proofs_db_metrics(
                    node.task_executor,
                    mdbx,
                    node.config.metrics.push_gateway_interval,
                );
                Ok(())
            })
            .install_exex("proofs-history", async move |exex_context| {
                Ok(OpProofsExEx::builder(exex_context, storage_for_exex)
                    .with_proofs_history_window(config.window)
                    .with_proofs_history_prune_interval(config.prune_interval)
                    .with_verification_interval(config.verification_interval)
                    .build()
                    .run())
            }),
        Some(handle),
    ))
}

/// Installs proof-history backed replacements for configured `debug_` witness RPC methods.
pub fn install_proof_history_debug_rpc<Node, EthApi>(
    ctx: &mut RpcContext<'_, Node, EthApi>,
    storage: ProofHistoryStorage,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    EthApi: FullEthApi + Send + Sync + 'static,
    Node::Provider:
        HeaderProvider<Header = reth::primitives::Header> + Clone + Send + Sync + 'static,
{
    let debug_ext = TaikoDebugWitnessExt::new(
        ctx.node().provider().clone(),
        ctx.registry.eth_api().clone(),
        storage,
        ctx.node().task_executor().clone(),
    );
    ctx.modules.replace_configured(debug_ext.into_rpc())?;
    Ok(())
}

/// Spawns periodic metric collection for the proof-history MDBX database.
fn spawn_proofs_db_metrics(
    executor: TaskExecutor,
    storage: Arc<MdbxProofsStorage>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_config_has_no_storage_path() {
        let config = ProofHistoryConfig::disabled();

        assert!(!config.enabled);
        assert!(config.storage_path.is_none());
        assert!(config.required_storage_path().is_err());
    }

    #[test]
    fn enabled_config_requires_storage_path() {
        let config = ProofHistoryConfig { enabled: true, ..ProofHistoryConfig::disabled() };

        assert!(config.required_storage_path().is_err());
    }
}
