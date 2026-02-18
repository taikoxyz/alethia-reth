//! Taiko node command wrapper and execution entrypoint.
use std::{ffi::OsString, fmt, path::PathBuf, sync::Arc};

use alloy_hardforks::EthereumHardforks;
use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{NodeCommand, launcher::Launcher, node::NoArgs};
use reth_cli_runner::CliContext;
use reth_db::mdbx::init_db_for;
use reth_node_builder::{NodeBuilder, NodeConfig};
use reth_node_core::version;

use alethia_reth_node::chainspec::spec::TaikoDevnetConfigExt;

use crate::{TaikoCliExtArgs, tables::TaikoTables};

/// Trait implemented by CLI extensions that can tweak Taiko-specific runtime options.
pub trait TaikoNodeExtArgs {
    /// Returns the configured devnet Shasta activation timestamp override.
    fn devnet_shasta_timestamp(&self) -> u64;
}

impl TaikoNodeExtArgs for NoArgs {
    // Default to 0 if not specified.
    fn devnet_shasta_timestamp(&self) -> u64 {
        0
    }
}

impl TaikoNodeExtArgs for TaikoCliExtArgs {
    // Return the configured devnet Shasta activation timestamp.
    fn devnet_shasta_timestamp(&self) -> u64 {
        self.devnet_shasta_timestamp
    }
}

/// Wrapper around `reth` `NodeCommand` that injects Taiko DB initialization and overrides.
#[derive(Debug)]
pub struct TaikoNodeCommand<C: ChainSpecParser, Ext: clap::Args + fmt::Debug = NoArgs>(
    /// Inner `reth` node command configuration.
    pub Box<NodeCommand<C, Ext>>,
);

impl<C: ChainSpecParser> TaikoNodeCommand<C> {
    /// Parsers only the default CLI arguments
    pub fn parse_args() -> Self {
        Self(Box::new(NodeCommand::<C, NoArgs>::parse()))
    }

    /// Parsers only the default [`NodeCommand`] arguments from the given iterator
    pub fn try_parse_args_from<I, T>(itr: I) -> Result<Self, clap::error::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        NodeCommand::<C, NoArgs>::try_parse_from(itr).map(|inner| Self(Box::new(inner)))
    }
}

impl<C: ChainSpecParser, Ext: clap::Args + fmt::Debug> TaikoNodeCommand<C, Ext> {
    /// Returns the underlying chain being used to run this command
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.0.chain)
    }
}

impl<C, Ext> TaikoNodeCommand<C, Ext>
where
    C: ChainSpecParser,
    C::ChainSpec: EthChainSpec + EthereumHardforks + TaikoDevnetConfigExt,
    Ext: clap::Args + fmt::Debug + TaikoNodeExtArgs,
{
    /// Launches the node
    ///
    /// This transforms the node command into a node config and launches the node using the given
    /// launcher.
    pub async fn execute<L>(self, ctx: CliContext, launcher: L) -> eyre::Result<()>
    where
        L: Launcher<C, Ext>,
    {
        tracing::info!(target: "reth::taiko::cli", version = ?version::version_metadata().short_version, "Starting alethia-reth");

        let NodeCommand {
            datadir,
            config,
            chain,
            metrics,
            instance,
            with_unused_ports,
            network,
            rpc,
            txpool,
            builder,
            debug,
            db,
            dev,
            pruning,
            ext,
            engine,
            era,
            static_files,
            storage,
        } = *self.0;

        // set up node config
        let mut node_config = NodeConfig {
            datadir,
            config,
            chain,
            metrics,
            instance,
            network,
            rpc,
            txpool,
            builder,
            debug,
            db,
            dev,
            pruning,
            engine,
            era,
            static_files,
            storage,
        };

        // Apply Taiko-specific devnet Shasta timestamp override if specified.
        if let Some(overridden_chain) = node_config
            .chain
            .as_ref()
            .clone_with_devnet_shasta_timestamp(ext.devnet_shasta_timestamp())
        {
            node_config.chain = Arc::new(overridden_chain);
        }

        let data_dir = node_config.datadir();
        let db_path = data_dir.db();

        // Initialize the database with extra tables for Taiko.
        tracing::info!(target: "reth::taiko::cli", path = ?db_path, "Opening database");
        let database =
            init_db_for::<PathBuf, TaikoTables>(db_path.clone(), self.0.db.database_args())?
                .with_metrics();

        if with_unused_ports {
            node_config = node_config.with_unused_ports();
        }

        let builder = NodeBuilder::new(node_config)
            .with_database(database)
            .with_launch_context(ctx.task_executor);

        launcher.entrypoint(builder, ext).await
    }
}
