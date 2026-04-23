//! Command that prunes the proofs-history storage.

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs};
use reth_ethereum::EthPrimitives;
use reth_node_core::version::version_metadata;
use tracing::info;

use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_proofs_trie::{
    MdbxProofsStorage, ProofsProviderRO, ProofsStoragePruner, ProofsStore,
};

/// Prunes the proofs-history storage by removing old proof history and state updates.
#[derive(Debug, Parser)]
pub struct PruneCommand<C: ChainSpecParser> {
    /// Shared environment arguments (`--datadir`, `--chain`, ...).
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    /// The path to the storage DB for the proofs-history sidecar.
    #[arg(
        long = "proofs-history.storage-path",
        value_name = "PROOFS_HISTORY_STORAGE_PATH",
        required = true
    )]
    pub storage_path: PathBuf,

    /// The retention window for proofs history, in blocks. The default is 72 hours at a 1s block
    /// time (`72 * 60 * 60 = 259_200`).
    #[arg(
        long = "proofs-history.window",
        default_value_t = 259_200,
        value_name = "PROOFS_HISTORY_WINDOW"
    )]
    pub proofs_history_window: u64,

    /// The maximum number of blocks processed per prune batch.
    #[arg(
        long = "proofs-history.prune-batch-size",
        default_value_t = 10_000,
        value_name = "PROOFS_HISTORY_PRUNE_BATCH_SIZE"
    )]
    pub proofs_history_prune_batch_size: u64,
}

impl<C: ChainSpecParser<ChainSpec = TaikoChainSpec>> PruneCommand<C> {
    /// Execute `alethia-reth proofs prune`.
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = EthPrimitives>>(
        self,
        runtime: reth_tasks::Runtime,
    ) -> eyre::Result<()> {
        info!(target: "reth::cli", "alethia-reth {} starting", version_metadata().short_version);
        info!(target: "reth::cli", "Pruning proofs-history storage at: {:?}", self.storage_path);

        // Initialize the environment with read-only access to the main DB.
        let Environment { provider_factory, .. } = self.env.init::<N>(AccessRights::RO, runtime)?;

        let storage: Arc<MdbxProofsStorage> = Arc::new(
            MdbxProofsStorage::new(&self.storage_path)
                .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))?,
        );

        let provider_ro = storage.provider_ro()?;
        let earliest_block = provider_ro.get_earliest_block_number()?;
        let latest_block = provider_ro.get_latest_block_number()?;
        info!(
            target: "reth::cli",
            ?earliest_block,
            ?latest_block,
            "Current proofs storage block range"
        );

        let pruner = ProofsStoragePruner::new(
            storage,
            provider_factory,
            self.proofs_history_window,
            self.proofs_history_prune_batch_size,
        );
        pruner.run();
        Ok(())
    }
}

impl<C: ChainSpecParser> PruneCommand<C> {
    /// Returns the underlying chain being used to run this command.
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.env.chain)
    }
}
