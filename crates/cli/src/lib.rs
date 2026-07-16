#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! CLI entrypoints and command wiring for the Alethia Taiko node.
use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use alloy_consensus::Header;
use clap::{CommandFactory, FromArgMatches};
use reth::{
    CliRunner,
    args::DatadirArgs,
    cli::{Cli, Commands},
    dirs::{DataDirPath, MaybePlatformPath},
    prometheus_exporter::install_prometheus_recorder,
};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{common::CliNodeTypes, launcher::FnLauncher, node::NoArgs};
use reth_db::DatabaseEnv;
use reth_ethereum_forks::Hardforks;
use reth_node_api::{NodePrimitives, NodeTypes};
use reth_node_builder::{NodeBuilder, WithLaunchContext};
use reth_tracing::TracingGuards;
use tracing::info;

use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_node::{
    TaikoNode,
    components::{ProviderTaikoBlockReader, evm_config_from_jit_args},
    consensus::validation::TaikoBeaconConsensus,
    proof_history::{
        DEFAULT_PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS,
        DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL, DEFAULT_PROOF_HISTORY_WINDOW,
    },
};
use reth_ethereum::EthPrimitives;
use reth_storage_api::noop::NoopProvider;

use crate::command::{TaikoNodeCommand, TaikoNodeExtArgs};

/// Parses a wall-clock duration and rejects zero-length timer intervals.
fn parse_nonzero_duration(value: &str) -> Result<Duration, String> {
    let duration = humantime::parse_duration(value).map_err(|error| error.to_string())?;
    if duration.is_zero() {
        return Err("duration must be greater than zero".to_string())
    }
    Ok(duration)
}

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
    /// Proof-history sidecar configuration.
    #[command(flatten)]
    pub proof_history: TaikoProofHistoryArgs,

    /// Override the devnet Unzen hardfork activation timestamp (`0` keeps the embedded value).
    #[arg(
        long = "devnet-unzen-timestamp",
        env = "ALETHIA_RETH_DEVNET_UNZEN_TIMESTAMP",
        value_name = "TIMESTAMP",
        default_value_t = 0u64,
        help_heading = "Taiko"
    )]
    pub devnet_unzen_timestamp: u64,
}

/// CLI arguments controlling the optional proof-history sidecar.
#[derive(Debug, Clone, clap::Args)]
pub struct TaikoProofHistoryArgs {
    /// Enable proof-history indexing and pruning.
    #[arg(long = "proofs-history", default_value_t = false, help_heading = "Taiko Proof History")]
    pub enabled: bool,

    /// Filesystem path for the proof-history MDBX database.
    #[arg(
        long = "proofs-history.storage-path",
        value_name = "PATH",
        help_heading = "Taiko Proof History"
    )]
    pub storage_path: Option<PathBuf>,

    /// Number of recent blocks retained in proof-history storage.
    #[arg(
        long = "proofs-history.window",
        value_name = "BLOCKS",
        default_value_t = DEFAULT_PROOF_HISTORY_WINDOW,
        help_heading = "Taiko Proof History"
    )]
    pub window: u64,

    /// Delay empty proof-history initialization until the finalized window start is reached.
    #[arg(
        long = "proofs-history.backfill-window-only",
        default_value_t = false,
        help_heading = "Taiko Proof History"
    )]
    pub backfill_window_only: bool,

    /// Wall-clock interval between proof-history prune passes.
    #[arg(
        long = "proofs-history.prune-interval",
        value_name = "DURATION",
        default_value = "15s",
        value_parser = parse_nonzero_duration,
        help_heading = "Taiko Proof History"
    )]
    pub prune_interval: Duration,

    /// Block interval between proof-history consistency checks; zero disables verification.
    #[arg(
        long = "proofs-history.verification-interval",
        value_name = "BLOCKS",
        default_value_t = DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL,
        help_heading = "Taiko Proof History"
    )]
    pub verification_interval: u64,

    /// Maximum number of retained blocks startup may prune automatically, e.g. after lowering
    /// the retention window. Startup refuses to prune more than this many blocks.
    #[arg(
        long = "proofs-history.max-startup-prune-blocks",
        value_name = "BLOCKS",
        default_value_t = DEFAULT_PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS,
        help_heading = "Taiko Proof History"
    )]
    pub max_startup_prune_blocks: u64,
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
    /// Parsed data directory for the re-execute command, retained for JIT debug dump placement.
    reexecute_datadir: Option<MaybePlatformPath<DataDirPath>>,
}

impl<C, Ext> TaikoCli<C, Ext>
where
    C: ChainSpecParser,
    Ext: clap::Args + fmt::Debug,
{
    /// Parsers only the default CLI arguments
    pub fn parse_args() -> Self {
        Self::try_parse_args_from(std::env::args_os()).unwrap_or_else(|err| err.exit())
    }

    /// Parsers only the default CLI arguments from the given iterator
    pub fn try_parse_args_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let mut matches = Cli::<C, Ext>::command().try_get_matches_from(itr)?;
        let reexecute_datadir = matches
            .subcommand_matches("re-execute")
            .and_then(|matches| matches.get_one::<MaybePlatformPath<DataDirPath>>("datadir"))
            .cloned();
        let inner = Cli::<C, Ext>::from_arg_matches_mut(&mut matches)?;

        Ok(Self { inner, reexecute_datadir })
    }
}

impl<
    C: ChainSpecParser<ChainSpec = TaikoChainSpec>,
    Ext: clap::Args + fmt::Debug + TaikoNodeExtArgs,
> TaikoCli<C, Ext>
{
    /// Returns the JIT compiler dump directory requested by the re-execute command.
    fn reexecute_jit_dump_dir(&self) -> Option<PathBuf> {
        let Commands::ReExecute(command) = &self.inner.command else {
            return None;
        };
        if !command.jit.debug {
            return None;
        }

        let chain = command.chain_spec()?.inner.chain;
        let datadir = DatadirArgs {
            datadir: self.reexecute_datadir.clone().unwrap_or_default(),
            ..Default::default()
        }
        .resolve_datadir(chain);
        Some(datadir.data_dir().join("jit"))
    }

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
        let reexecute_jit_dump_dir = self.reexecute_jit_dump_dir();

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
                // reth's re-execute never consumes its own `--jit` args (v2.4.0), and the
                // components closure it accepts cannot see them, so the JIT-aware EVM config
                // must be built here. Mirrors the node's executor builder, including the hard
                // error when `--jit` is requested on a non-jit build.
                let chain_spec = command
                    .chain_spec()
                    .cloned()
                    .ok_or_else(|| eyre::eyre!("re-execute requires a chain spec"))?;
                let evm =
                    evm_config_from_jit_args(chain_spec, &command.jit, reexecute_jit_dump_dir)?;
                let components = move |spec: Arc<C::ChainSpec>| {
                    let block_reader = Arc::new(ProviderTaikoBlockReader(NoopProvider::<
                        TaikoChainSpec,
                        EthPrimitives,
                    >::new(
                        spec.clone()
                    )));
                    let consensus = Arc::new(TaikoBeaconConsensus::new(spec, block_reader));
                    (evm.clone(), consensus)
                };
                runner.run_until_ctrl_c(command.execute::<TaikoNode>(components, rt))
            }
        }
    }

    /// Initializes tracing with the configured options.
    ///
    /// If file logging is enabled, the returned [`TracingGuards`] must be kept alive to ensure
    /// that all logs are flushed to disk.
    pub fn init_tracing(&self) -> eyre::Result<TracingGuards> {
        let guard = self.inner.logs.init_tracing()?;
        Ok(guard)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Mutex, OnceLock},
        time::Duration,
    };

    use clap::Parser;

    use super::{
        DEFAULT_PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS,
        DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL, DEFAULT_PROOF_HISTORY_WINDOW,
        TaikoChainSpecParser, TaikoCli, TaikoCliExtArgs,
    };
    use crate::command::TaikoNodeExtArgs;

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
    fn test_parse_devnet_unzen_timestamp_flag() {
        let _lock = env_lock();
        unsafe { std::env::remove_var("ALETHIA_RETH_DEVNET_UNZEN_TIMESTAMP") };
        let cli = TestCli::try_parse_from(["alethia-reth", "--devnet-unzen-timestamp", "42"])
            .expect("flag should parse");

        assert_eq!(cli.ext.devnet_unzen_timestamp, 42);
    }

    #[test]
    fn test_parse_devnet_unzen_timestamp_default() {
        let _lock = env_lock();
        unsafe { std::env::remove_var("ALETHIA_RETH_DEVNET_UNZEN_TIMESTAMP") };
        let cli = TestCli::try_parse_from(["alethia-reth"]).expect("default args should parse");

        assert_eq!(cli.ext.devnet_unzen_timestamp, 0);
    }

    #[test]
    fn test_reexecute_jit_debug_resolves_dump_dir_from_datadir() {
        let cli = TaikoCli::<TaikoChainSpecParser, TaikoCliExtArgs>::try_parse_args_from([
            "alethia-reth",
            "re-execute",
            "--datadir",
            "/tmp/alethia-reth",
            "--jit",
            "--jit.debug",
        ])
        .expect("re-execute JIT arguments should parse");

        assert_eq!(cli.reexecute_jit_dump_dir(), Some(PathBuf::from("/tmp/alethia-reth/jit")));
    }

    #[test]
    fn test_parse_devnet_unzen_timestamp_from_env() {
        let _lock = env_lock();
        unsafe { std::env::set_var("ALETHIA_RETH_DEVNET_UNZEN_TIMESTAMP", "42") };
        let cli = TestCli::try_parse_from(["alethia-reth"]).expect("env-backed args should parse");
        unsafe { std::env::remove_var("ALETHIA_RETH_DEVNET_UNZEN_TIMESTAMP") };

        assert_eq!(cli.ext.devnet_unzen_timestamp, 42);
    }

    #[test]
    fn test_parse_proof_history_flags() {
        let cli = TestCli::try_parse_from([
            "alethia-reth",
            "--proofs-history",
            "--proofs-history.storage-path",
            "/tmp/proofs-history",
            "--proofs-history.window",
            "256",
            "--proofs-history.backfill-window-only",
            "--proofs-history.prune-interval",
            "30s",
            "--proofs-history.verification-interval",
            "16",
            "--proofs-history.max-startup-prune-blocks",
            "5000",
        ])
        .expect("proof-history args should parse");

        assert!(cli.ext.proof_history.enabled);
        assert_eq!(cli.ext.proof_history.storage_path, Some(PathBuf::from("/tmp/proofs-history")));
        assert_eq!(cli.ext.proof_history.window, 256);
        assert!(cli.ext.proof_history.backfill_window_only);
        assert_eq!(cli.ext.proof_history.prune_interval, Duration::from_secs(30));
        assert_eq!(cli.ext.proof_history.verification_interval, 16);
        assert_eq!(cli.ext.proof_history.max_startup_prune_blocks, 5000);
    }

    #[test]
    fn test_rejects_zero_proof_history_prune_interval() {
        let err =
            TestCli::try_parse_from(["alethia-reth", "--proofs-history.prune-interval", "0s"])
                .expect_err("a zero prune interval would panic when the sidecar starts its timer");

        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        assert!(err.to_string().contains("greater than zero"));
    }

    #[test]
    fn test_default_proof_history_config_is_disabled() {
        let cli = TestCli::try_parse_from(["alethia-reth"]).expect("default args should parse");
        let config = cli.ext.proof_history_config();

        assert!(!config.enabled);
        assert!(config.storage_path.is_none());
        assert_eq!(config.window, DEFAULT_PROOF_HISTORY_WINDOW);
        assert!(!config.backfill_window_only);
        assert_eq!(config.prune_interval, Duration::from_secs(15));
        assert_eq!(config.verification_interval, DEFAULT_PROOF_HISTORY_VERIFICATION_INTERVAL);
        assert_eq!(config.max_startup_prune_blocks, DEFAULT_PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS);
    }

    #[test]
    fn test_enabled_proof_history_config_without_path_returns_storage_error() {
        let cli = TestCli::try_parse_from(["alethia-reth", "--proofs-history"])
            .expect("proof-history can be enabled without parser-level storage validation");
        let config = cli.ext.proof_history_config();

        assert!(config.enabled);
        assert!(config.required_storage_path().is_err());
    }

    #[test]
    fn test_rejects_legacy_devnet_shasta_timestamp_flag() {
        let _lock = env_lock();
        unsafe { std::env::remove_var("ALETHIA_RETH_DEVNET_UNZEN_TIMESTAMP") };
        let err = TestCli::try_parse_from(["alethia-reth", "--devnet-shasta-timestamp", "42"])
            .expect_err("legacy flag should be rejected");

        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
