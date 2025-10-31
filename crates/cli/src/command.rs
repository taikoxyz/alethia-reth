use std::{ffi::OsString, fmt, path::PathBuf, sync::Arc};

use alloy_hardforks::EthereumHardforks;
use clap::Parser;
use eyre::ensure;
use reth::{CliContext, args::RessArgs, chainspec::EthChainSpec, core::version};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{NodeCommand, launcher::Launcher, node::NoArgs};
use reth_db::mdbx::init_db_for;
use reth_node_builder::{NodeBuilder, NodeConfig};

use crate::tables::TaikoTables;
use alethia_reth_node::chainspec::{
    TAIKO_DEVNET_GENESIS_HASH, hardfork::DEFAULT_DEVNET_SHASTA_TIMESTAMP, spec::TaikoChainSpec,
    taiko_devnet_chain_spec_with_shasta_timestamp,
};

/// CLI argument extension trait for handling optional devnet Shasta timestamp overrides.
pub trait DevnetShastaArgs {
    /// Returns the Shasta activation timestamp for the Taiko devnet.
    fn devnet_shasta_timestamp(&self) -> u64;

    /// Returns true if the devnet Shasta timestamp was explicitly overridden on the CLI.
    fn devnet_shasta_timestamp_overridden(&self) -> bool {
        false
    }
}

impl DevnetShastaArgs for NoArgs {
    fn devnet_shasta_timestamp(&self) -> u64 {
        DEFAULT_DEVNET_SHASTA_TIMESTAMP
    }
}

impl DevnetShastaArgs for RessArgs {
    fn devnet_shasta_timestamp(&self) -> u64 {
        DEFAULT_DEVNET_SHASTA_TIMESTAMP
    }
}

/// Additional CLI arguments supported by alethia-reth.
#[derive(Clone, Debug, clap::Args)]
pub struct TaikoCliExt {
    /// Arguments for configuring RESS, forwarded to reth.
    #[clap(flatten)]
    pub ress: RessArgs,

    /// Override the Shasta hardfork activation timestamp when running against the Taiko devnet.
    #[clap(
        long,
        value_name = "SECONDS",
        num_args = 0..=1,
        default_missing_value = "0",
        help = "Set a custom Shasta hardfork activation timestamp when using the devnet chainspec. Defaults to 0.",
        help_heading = "Taiko Devnet"
    )]
    pub devnet_shasta_timestamp: Option<u64>,
}

impl DevnetShastaArgs for TaikoCliExt {
    fn devnet_shasta_timestamp(&self) -> u64 {
        self.devnet_shasta_timestamp.unwrap_or(DEFAULT_DEVNET_SHASTA_TIMESTAMP)
    }

    fn devnet_shasta_timestamp_overridden(&self) -> bool {
        self.devnet_shasta_timestamp.is_some()
    }
}

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
    C: ChainSpecParser<ChainSpec = TaikoChainSpec>,
    C::ChainSpec: EthChainSpec + EthereumHardforks,
    Ext: clap::Args + fmt::Debug + DevnetShastaArgs,
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
        } = *self.0;

        let shasta_timestamp = ext.devnet_shasta_timestamp();
        let chain = if ext.devnet_shasta_timestamp_overridden() {
            ensure!(
                chain.genesis_hash() == TAIKO_DEVNET_GENESIS_HASH,
                "--devnet-shasta-timestamp can only be used with the devnet chainspec",
            );
            tracing::info!(
                target: "reth::taiko::cli",
                shasta_timestamp,
                "Overriding Taiko devnet Shasta activation timestamp"
            );
            Arc::new(taiko_devnet_chain_spec_with_shasta_timestamp(shasta_timestamp))
        } else {
            chain
        };

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
