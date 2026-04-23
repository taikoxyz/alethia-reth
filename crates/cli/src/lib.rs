#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! CLI entrypoints and command wiring for the Alethia Taiko node.
use std::{fmt, sync::Arc};

use alloy_consensus::Header;
use clap::Parser;
use reth::{
    CliRunner,
    cli::{Cli, Commands},
    prometheus_exporter::install_prometheus_recorder,
};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{common::CliNodeTypes, launcher::FnLauncher, node::NoArgs};
use reth_db::DatabaseEnv;
use reth_ethereum_forks::Hardforks;
use reth_node_api::{NodePrimitives, NodeTypes};
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_tracing::FileWorkerGuard;
use tracing::info;

use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_node::{
    TaikoNode, consensus::validation::TaikoBeaconConsensus, node_builder::ProviderTaikoBlockReader,
};
use reth_ethereum::EthPrimitives;
use reth_storage_api::noop::NoopProvider;

use crate::command::{TaikoNodeCommand, TaikoNodeExtArgs};

/// CLI extension arguments for alethia-reth-specific features.
pub mod args;
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
    /// Override the devnet Uzen hardfork activation timestamp (`0` keeps the embedded value).
    #[arg(
        long = "devnet-uzen-timestamp",
        env = "ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP",
        value_name = "TIMESTAMP",
        default_value_t = 0u64,
        help_heading = "Taiko"
    )]
    pub devnet_uzen_timestamp: u64,

    /// Configuration for the historical-proofs sidecar.
    #[command(flatten)]
    pub proofs_history: crate::args::ProofsHistoryArgs,
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
        let rt = runner.runtime();

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
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>(rt))
            }
            Commands::InitState(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>(rt))
            }
            Commands::Import(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode, _>(components, rt))
            }
            Commands::ImportEra(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>(rt))
            }
            Commands::ExportEra(command) => {
                runner.run_blocking_until_ctrl_c(command.execute::<TaikoNode>(rt))
            }
            Commands::SnapshotManifest(command) => command.execute(),
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
                runner.run_until_ctrl_c(command.execute::<TaikoNode>(components, rt))
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

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use clap::Parser;

    use super::TaikoCliExtArgs;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().expect("env lock should not be poisoned")
    }

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(flatten)]
        ext: TaikoCliExtArgs,
    }

    #[test]
    fn test_parse_devnet_uzen_timestamp_flag() {
        let _lock = env_lock();
        unsafe { std::env::remove_var("ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP") };
        let cli = TestCli::try_parse_from(["alethia-reth", "--devnet-uzen-timestamp", "42"])
            .expect("flag should parse");

        assert_eq!(cli.ext.devnet_uzen_timestamp, 42);
    }

    #[test]
    fn test_parse_devnet_uzen_timestamp_default() {
        let _lock = env_lock();
        unsafe { std::env::remove_var("ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP") };
        let cli = TestCli::try_parse_from(["alethia-reth"]).expect("default args should parse");

        assert_eq!(cli.ext.devnet_uzen_timestamp, 0);
    }

    #[test]
    fn test_parse_devnet_uzen_timestamp_from_env() {
        let _lock = env_lock();
        unsafe { std::env::set_var("ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP", "42") };
        let cli = TestCli::try_parse_from(["alethia-reth"]).expect("env-backed args should parse");
        unsafe { std::env::remove_var("ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP") };

        assert_eq!(cli.ext.devnet_uzen_timestamp, 42);
    }

    #[test]
    fn test_rejects_legacy_devnet_shasta_timestamp_flag() {
        let _lock = env_lock();
        unsafe { std::env::remove_var("ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP") };
        let err = TestCli::try_parse_from(["alethia-reth", "--devnet-shasta-timestamp", "42"])
            .expect_err("legacy flag should be rejected");

        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
