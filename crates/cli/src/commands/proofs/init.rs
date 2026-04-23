//! Command that initializes the proofs-history storage with the current state of the chain.

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use reth_chainspec::ChainInfo;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs};
use reth_ethereum::EthPrimitives;
use reth_node_core::version::version_metadata;
use reth_optimism_trie::{InitializationJob, MdbxProofsStorage, OpProofsProviderRO, OpProofsStore};
use reth_provider::{BlockNumReader, DBProvider, DatabaseProviderFactory};
use tracing::info;

use alethia_reth_chainspec::spec::TaikoChainSpec;

/// Initializes the proofs storage with the current state of the chain.
///
/// This command must be run before starting the node with the proofs-history sidecar enabled.
/// It backfills the sidecar storage with trie nodes derived from the current chain state so
/// that subsequent live tracking can extend an already-consistent anchor.
#[derive(Debug, Parser)]
pub struct InitCommand<C: ChainSpecParser> {
    /// Shared environment arguments (`--datadir`, `--chain`, ...).
    #[command(flatten)]
    env: EnvironmentArgs<C>,

    /// The path to the storage DB for the proofs-history sidecar.
    ///
    /// This should match the path used when starting the node with
    /// `--proofs-history.storage-path`.
    #[arg(
        long = "proofs-history.storage-path",
        value_name = "PROOFS_HISTORY_STORAGE_PATH",
        required = true
    )]
    pub storage_path: PathBuf,
}

impl<C: ChainSpecParser<ChainSpec = TaikoChainSpec>> InitCommand<C> {
    /// Execute `alethia-reth proofs init`.
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = EthPrimitives>>(
        self,
        runtime: reth::tasks::Runtime,
    ) -> eyre::Result<()> {
        info!(target: "reth::cli", "alethia-reth {} starting", version_metadata().short_version);
        info!(target: "reth::cli", "Initializing proofs-history storage at: {:?}", self.storage_path);

        // Initialize the environment with read-only access to the main DB.
        let Environment { provider_factory, .. } = self.env.init::<N>(AccessRights::RO, runtime)?;

        // Create the proofs storage without any metrics wrapper. During initialization we
        // may write hundreds of millions of entries; the metrics layer would accumulate
        // per-observation bytes and could OOM on large chains.
        let storage: Arc<MdbxProofsStorage> = Arc::new(
            MdbxProofsStorage::new(&self.storage_path)
                .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))?,
        );

        // Check if already initialized
        if let Some((block_number, block_hash)) =
            storage.provider_ro()?.get_earliest_block_number()?
        {
            info!(
                target: "reth::cli",
                block_number,
                ?block_hash,
                "Proofs storage already initialized"
            );
            return Ok(());
        }

        // Get the current chain state to anchor the initialization job against.
        let ChainInfo { best_number, best_hash, .. } = provider_factory.chain_info()?;

        info!(
            target: "reth::cli",
            best_number,
            ?best_hash,
            "Starting backfill job for current chain state"
        );

        // Run the backfill job against a long-lived read transaction on the main DB.
        {
            let db_provider =
                provider_factory.database_provider_ro()?.disable_long_read_transaction_safety();
            let db_tx = db_provider.into_tx();

            InitializationJob::new(storage, db_tx).run(best_number, best_hash)?;
        }

        info!(
            target: "reth::cli",
            best_number,
            ?best_hash,
            "Proofs storage initialized successfully"
        );

        Ok(())
    }
}

impl<C: ChainSpecParser> InitCommand<C> {
    /// Returns the underlying chain being used to run this command.
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.env.chain)
    }
}
