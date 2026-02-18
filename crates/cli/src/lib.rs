#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! CLI entrypoints and command wiring for the Alethia Taiko node.
use std::{fmt, sync::Arc};

use alethia_reth_block::config::TaikoEvmConfig;
use alloy_consensus::Header;
use clap::Parser;
use eyre::Ok;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{common::CliNodeTypes, launcher::FnLauncher, node::NoArgs};
use reth_cli_runner::CliRunner;
use reth_db::DatabaseEnv;
use reth_ethereum::EthPrimitives;
use reth_ethereum_cli::{Cli, interface::Commands};
use reth_ethereum_forks::Hardforks;
use reth_node_api::{NodePrimitives, NodeTypes};
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_node_metrics::recorder::install_prometheus_recorder;
use reth_tracing::FileWorkerGuard;
use tracing::info;

use alethia_reth_node::{
    TaikoNode, chainspec::spec::TaikoChainSpec, consensus::validation::TaikoBeaconConsensus,
    node_builder::ProviderTaikoBlockReader,
};
use reth_storage_api::noop::NoopProvider;

use crate::command::{TaikoNodeCommand, TaikoNodeExtArgs};

/// Node-command wrappers and extension traits for Taiko runtime options.
pub mod command;
/// Chain-spec parser implementations for Taiko network names and genesis input.
pub mod parser;
/// Database table-set registration used by CLI DB initialization.
pub mod tables;

pub use parser::TaikoChainSpecParser;

/// Additional Taiko CLI arguments layered on top of the base CLI.
#[derive(Debug, clap::Args)]
pub struct TaikoCliExtArgs {
    /// Override the devnet Shasta hardfork activation timestamp (`0` keeps the embedded value).
    #[arg(
        long = "devnet-shasta-timestamp",
        value_name = "TIMESTAMP",
        default_value_t = 0u64,
        help_heading = "Taiko"
    )]
    pub devnet_shasta_timestamp: u64,
}

/// The main alethia-reth cli interface.
///
/// This is the entrypoint to the executable.
#[derive(Debug)]
pub struct TaikoCli<
    C: ChainSpecParser = TaikoChainSpecParser,
    Ext: clap::Args + fmt::Debug = NoArgs,
> {
    /// Wrapped `reth` CLI structure containing parsed commands and global options.
    pub inner: Cli<C, Ext>,
}

impl<C, Ext> TaikoCli<C, Ext>
where
    C: ChainSpecParser,
    Ext: clap::Args + fmt::Debug,
{
    /// Parsers only the default CLI arguments
    pub fn parse_args() -> Self {
        Self { inner: Cli::<C, Ext>::parse() }
    }

    /// Parsers only the default CLI arguments from the given iterator
    pub fn try_parse_args_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Cli::<C, Ext>::try_parse_from(itr).map(|inner| Self { inner })
    }
}

impl<
    C: ChainSpecParser<ChainSpec = TaikoChainSpec>,
    Ext: clap::Args + fmt::Debug + TaikoNodeExtArgs,
> TaikoCli<C, Ext>
{
    /// Execute the configured cli command.
    ///
    /// This accepts a closure that is used to launch the node via the
    /// [`TaikoNodeCommand`], to ensure that all Taiko related database tables are initialized
    /// before the node is started.
    pub fn run<L, Fut>(self, launcher: L) -> eyre::Result<()>
    where
        L: FnOnce(WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>, Ext) -> Fut,
        Fut: Future<Output = eyre::Result<()>>,
    {
        self.with_runner(CliRunner::try_default_runtime()?, launcher)
    }

    /// Execute the configured cli command with the provided [`CliRunner`].
    pub fn with_runner<L, Fut>(self, runner: CliRunner, launcher: L) -> eyre::Result<()>
    where
        L: FnOnce(WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>, Ext) -> Fut,
        Fut: Future<Output = eyre::Result<()>>,
    {
        self.with_runner_and_components::<TaikoNode>(runner, async move |builder, ext| {
            launcher(builder, ext).await
        })
    }

    /// Execute the configured cli command with the provided [`CliRunner`] and
    /// [`CliComponentsBuilder`].
    pub fn with_runner_and_components<N>(
        mut self,
        runner: CliRunner,
        launcher: impl AsyncFnOnce(
            WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>,
            Ext,
        ) -> eyre::Result<()>,
    ) -> eyre::Result<()>
    where
        N: CliNodeTypes<Primitives: NodePrimitives, ChainSpec: Hardforks>,
        C: ChainSpecParser<ChainSpec = TaikoChainSpec>,
        <<N as NodeTypes>::Primitives as NodePrimitives>::BlockHeader: From<Header>,
    {
        // Add network name if available to the logs dir
        if let Some(chain_spec) = self.inner.command.chain_spec() {
            self.inner.logs.log_file_directory =
                self.inner.logs.log_file_directory.join(chain_spec.inner.chain.to_string());
        }
        let _guard = self.init_tracing()?;
        info!(target: "reth::taiko::cli", "Initialized tracing, debug log directory: {}", self.inner.logs.log_file_directory);

        // Install the prometheus recorder to be sure to record all metrics
        let _ = install_prometheus_recorder();

        let components = |spec: Arc<C::ChainSpec>| {
            let evm = TaikoEvmConfig::new(spec.clone());
            let block_reader = Arc::new(ProviderTaikoBlockReader(NoopProvider::<
                TaikoChainSpec,
                EthPrimitives,
            >::new(spec.clone())));
            let consensus = Arc::new(TaikoBeaconConsensus::new(spec, block_reader));
            (evm, consensus)
        };
        match self.inner.command {
            // NOTE: We use the custom `TaikoNodeCommand` to handle the node commands, to initialize
            // all Taiko related database tables.
            Commands::Node(command) => runner.run_command_until_exit(|ctx| {
                TaikoNodeCommand(command).execute(ctx, FnLauncher::new::<C, Ext>(launcher))
            }),
            Commands::Init(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>())
            }
            Commands::InitState(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>())
            }
            Commands::Import(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode, _>(components))
            }
            Commands::ImportEra(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>())
            }
            Commands::ExportEra(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>())
            }
            Commands::DumpGenesis(command) => runner.run_blocking_until_ctrl_c(command.execute()),
            Commands::Db(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<TaikoNode>(ctx))
            }
            Commands::Download(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>())
            }
            Commands::Stage(command) => runner
                .run_command_until_exit(|ctx| command.execute::<TaikoNode, _>(ctx, components)),
            Commands::P2P(command) => runner.run_until_ctrl_c(command.execute::<TaikoNode>()),
            Commands::Config(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::Prune(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<TaikoNode>(ctx))
            }
            Commands::ReExecute(command) => {
                runner.run_until_ctrl_c(command.execute::<TaikoNode>(components))
            }
        }
    }

    /// Initializes tracing with the configured options.
    ///
    /// If file logging is enabled, this function returns a guard that must be kept alive to ensure
    /// that all logs are flushed to disk.
    pub fn init_tracing(&self) -> eyre::Result<Option<FileWorkerGuard>> {
        let guard = self.inner.logs.init_tracing()?;
        Ok(guard)
    }
}
