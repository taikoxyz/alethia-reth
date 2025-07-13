use std::{ffi::OsString, fmt, path::PathBuf, sync::Arc};

use alloy_hardforks::EthereumHardforks;
use clap::Parser;
use reth::{CliContext, chainspec::EthChainSpec, core::version};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{NodeCommand, launcher::Launcher, node::NoArgs};
use reth_db::mdbx::init_db_for;
use reth_node_builder::{NodeBuilder, NodeConfig};

use crate::cli::tables::TaikoTables;

#[derive(Debug)]
pub struct TaikoNodeCommand<C: ChainSpecParser, Ext: clap::Args + fmt::Debug = NoArgs>(
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
        NodeCommand::<C, NoArgs>::try_parse_from(itr)
            .map(|inner| Self(Box::new(inner)))
            .map_err(|e| e.into())
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
    C::ChainSpec: EthChainSpec + EthereumHardforks,
    Ext: clap::Args + fmt::Debug,
{
    /// Launches the node
    ///
    /// This transforms the node command into a node config and launches the node using the given
    /// launcher.
    pub async fn execute<L>(self, ctx: CliContext, launcher: L) -> eyre::Result<()>
    where
        L: Launcher<C, Ext>,
    {
        tracing::info!(target: "reth::taiko::cli", version = ?version::SHORT_VERSION, "Starting taiko-reth");

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
        };

        let data_dir = node_config.datadir();
        let db_path = data_dir.db();

        // Initialize the database with extra tables for Taiko.
        tracing::info!(target: "reth::taiko::cli", path = ?db_path, "Opening database");
        let database = Arc::new(
            init_db_for::<PathBuf, TaikoTables>(db_path.clone(), self.0.db.database_args())?
                .with_metrics(),
        );

        if with_unused_ports {
            node_config = node_config.with_unused_ports();
        }

        let builder = NodeBuilder::new(node_config)
            .with_database(database)
            .with_launch_context(ctx.task_executor);

        launcher.entrypoint(builder, ext).await
    }
}
