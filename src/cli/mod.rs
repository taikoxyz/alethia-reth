use std::{fmt, sync::Arc};

use clap::Parser;
use eyre::Ok;
use reth::{
    CliRunner,
    beacon_consensus::EthBeaconConsensus,
    cli::{Cli, Commands},
    prometheus_exporter::install_prometheus_recorder,
};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{launcher::FnLauncher, node::NoArgs};
use reth_db::DatabaseEnv;
use reth_evm_ethereum::EthEvmConfig;
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_tracing::FileWorkerGuard;
use tracing::info;

use crate::{
    TaikoNode,
    chainspec::{parser::TaikoChainSpecParser, spec::TaikoChainSpec},
    cli::command::TaikoNodeCommand,
};

pub mod command;
pub mod tables;

/// The main taiko-reth cli interface.
///
/// This is the entrypoint to the executable.
#[derive(Debug)]
pub struct TaikoCli<
    C: ChainSpecParser = TaikoChainSpecParser,
    Ext: clap::Args + fmt::Debug = NoArgs,
> {
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

impl<C: ChainSpecParser<ChainSpec = TaikoChainSpec>, Ext: clap::Args + fmt::Debug>
    TaikoCli<C, Ext>
{
    /// Execute the configured cli command.
    ///
    /// This accepts a closure that is used to launch the node via the
    /// [`TaikoNodeCommand`], to ensure that all Taiko related database tables are initialized
    /// before the node is started.
    pub fn run<L, Fut>(self, launcher: L) -> eyre::Result<()>
    where
        L: FnOnce(WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, C::ChainSpec>>, Ext) -> Fut,
        Fut: Future<Output = eyre::Result<()>>,
    {
        self.with_runner(CliRunner::try_default_runtime()?, launcher)
    }

    /// Execute the configured cli command with the provided [`CliRunner`].
    pub fn with_runner<L, Fut>(mut self, runner: CliRunner, launcher: L) -> eyre::Result<()>
    where
        L: FnOnce(WithLaunchContext<NodeBuilder<Arc<DatabaseEnv>, C::ChainSpec>>, Ext) -> Fut,
        Fut: Future<Output = eyre::Result<()>>,
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
            (EthEvmConfig::ethereum(spec.clone()), EthBeaconConsensus::new(spec))
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
            Commands::DumpGenesis(command) => runner.run_blocking_until_ctrl_c(command.execute()),
            Commands::Db(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>())
            }
            Commands::Download(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>())
            }
            Commands::Stage(command) => runner
                .run_command_until_exit(|ctx| command.execute::<TaikoNode, _>(ctx, components)),
            Commands::P2P(command) => runner.run_until_ctrl_c(command.execute::<TaikoNode>()),
            Commands::Config(command) => runner.run_until_ctrl_c(command.execute()),
            Commands::Debug(_) => {
                info!(target: "reth::taiko::cli", "Debug command is not implemented yet.");
                Ok(())
            }
            Commands::Recover(command) => {
                runner.run_command_until_exit(|ctx| command.execute::<TaikoNode>(ctx))
            }
            Commands::Prune(command) => runner.run_until_ctrl_c(command.execute::<TaikoNode>()),
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
