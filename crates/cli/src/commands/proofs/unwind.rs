//! Command that unwinds the proofs-history storage to a specific block number.

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs};
use reth_ethereum::EthPrimitives;
use reth_node_core::version::version_metadata;
use reth_provider::{BlockReader, TransactionVariant};
use tracing::{info, warn};

use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_proofs_trie::{
    MdbxProofsStorage, ProofsProviderRO, ProofsProviderRw, ProofsStore,
};

/// Unwinds the proofs-history storage to a specific block number.
///
/// This command removes all proof history and state updates that come *after* the target
/// block number.
#[derive(Debug, Parser)]
pub struct UnwindCommand<C: ChainSpecParser> {
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

    /// The target block number to unwind to.
    ///
    /// All history *after* this block will be removed from the sidecar storage.
    #[arg(long, value_name = "TARGET_BLOCK")]
    pub target: u64,
}

impl<C: ChainSpecParser> UnwindCommand<C> {
    /// Validates that the target block number is within a valid range for unwinding.
    fn validate_unwind_range<Store: ProofsStore>(&self, storage: Store) -> eyre::Result<bool> {
        let provider_ro = storage.provider_ro()?;
        let (Some((earliest, _)), Some((latest, _))) =
            (provider_ro.get_earliest_block_number()?, provider_ro.get_latest_block_number()?)
        else {
            warn!(target: "reth::cli", "No blocks found in proofs storage. Nothing to unwind.");
            return Ok(false);
        };

        if self.target <= earliest {
            warn!(
                target: "reth::cli",
                unwind_target = ?self.target,
                ?earliest,
                "Target block is less than or equal to the earliest block in proofs storage. Nothing to unwind."
            );
            return Ok(false);
        }

        if self.target > latest {
            warn!(
                target: "reth::cli",
                unwind_target = ?self.target,
                ?latest,
                "Target block is greater than the latest block in proofs storage. Nothing to unwind."
            );
            return Ok(false);
        }

        Ok(true)
    }
}

impl<C: ChainSpecParser<ChainSpec = TaikoChainSpec>> UnwindCommand<C> {
    /// Execute `alethia-reth proofs unwind`.
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = EthPrimitives>>(
        self,
        runtime: reth_tasks::Runtime,
    ) -> eyre::Result<()> {
        info!(target: "reth::cli", "alethia-reth {} starting", version_metadata().short_version);
        info!(target: "reth::cli", "Unwinding proofs-history storage at: {:?}", self.storage_path);

        // Initialize the environment with read-only access to the main DB.
        let Environment { provider_factory, .. } = self.env.init::<N>(AccessRights::RO, runtime)?;

        // Create the proofs storage.
        let storage: Arc<MdbxProofsStorage> = Arc::new(
            MdbxProofsStorage::new(&self.storage_path)
                .map_err(|e| eyre::eyre!("Failed to create MdbxProofsStorage: {e}"))?,
        );

        // Validate that the target block is within a valid range for unwinding.
        if !self.validate_unwind_range(storage.clone())? {
            return Ok(());
        }

        // Resolve the target block from the main database to obtain the canonical hash/parent.
        let block = provider_factory
            .recovered_block(self.target.into(), TransactionVariant::NoHash)?
            .ok_or_else(|| {
                eyre::eyre!("Target block {} not found in the main database", self.target)
            })?;

        info!(
            target: "reth::cli",
            block_number = block.number,
            block_hash = %block.hash(),
            "Unwinding to target block"
        );
        let provider_rw = storage.provider_rw()?;
        provider_rw.unwind_history(block.block_with_parent())?;
        provider_rw.commit()?;

        Ok(())
    }
}

impl<C: ChainSpecParser> UnwindCommand<C> {
    /// Returns the underlying chain being used to run this command.
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.env.chain)
    }
}
