//! Taiko node command wrapper and execution entrypoint.
use std::{ffi::OsString, fmt, path::PathBuf, sync::Arc};

use alloy_hardforks::EthereumHardforks;
use clap::Parser;
use reth::{CliContext, chainspec::EthChainSpec, core::version};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::{NodeCommand, launcher::Launcher, node::NoArgs};
use reth_db::mdbx::init_db_for;
use reth_node_builder::{NodeBuilder, NodeConfig};

use alethia_reth_node::{chainspec::spec::TaikoDevnetConfigExt, proof_history::ProofHistoryConfig};

use crate::{TaikoCliExtArgs, tables::TaikoTables};

/// Trait implemented by CLI extensions that can tweak Taiko-specific runtime options.
pub trait TaikoNodeExtArgs {
    /// Returns the optional devnet Etna activation timestamp override.
    fn devnet_etna_timestamp(&self) -> Option<u64>;

    /// Returns the configured proof-history sidecar options.
    fn proof_history_config(&self) -> ProofHistoryConfig;
}

impl TaikoNodeExtArgs for NoArgs {
    /// Leaves Etna disabled for commands without Taiko options.
    fn devnet_etna_timestamp(&self) -> Option<u64> {
        None
    }

    /// Returns a disabled proof-history configuration for commands without Taiko options.
    fn proof_history_config(&self) -> ProofHistoryConfig {
        ProofHistoryConfig::disabled()
    }
}

impl TaikoNodeExtArgs for TaikoCliExtArgs {
    /// Returns the optional devnet Etna activation timestamp override.
    fn devnet_etna_timestamp(&self) -> Option<u64> {
        self.devnet_etna_timestamp
    }

    /// Returns proof-history configuration derived from parsed Taiko CLI flags.
    fn proof_history_config(&self) -> ProofHistoryConfig {
        ProofHistoryConfig {
            enabled: self.proof_history.enabled,
            storage_path: self.proof_history.storage_path.clone(),
            window: self.proof_history.window,
            backfill_window_only: self.proof_history.backfill_window_only,
            prune_interval: self.proof_history.prune_interval,
            verification_interval: self.proof_history.verification_interval,
            max_startup_prune_blocks: self.proof_history.max_startup_prune_blocks,
        }
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

/// Applies `--devnet-etna-timestamp` to `chain`.
///
/// Canonical Taiko devnet and alloc-only variants accept the override. Other chains keep their
/// schedule and log a warning, so a misconfigured node does not silently start with Etna off.
fn apply_devnet_etna_override<S: TaikoDevnetConfigExt>(chain: &mut Arc<S>, timestamp: Option<u64>) {
    let Some(timestamp) = timestamp else { return };
    match chain.clone_with_devnet_etna_timestamp(timestamp) {
        Some(overridden) => *chain = Arc::new(overridden),
        None => tracing::warn!(
            target: "reth::taiko::cli",
            timestamp,
            "Ignoring --devnet-etna-timestamp: Genesis fields outside alloc must match canonical Taiko devnet"
        ),
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
            jit,
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
            jit,
        };

        apply_devnet_etna_override(&mut node_config.chain, ext.devnet_etna_timestamp());

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alethia_reth_node::chainspec::{
        TAIKO_DEVNET, TAIKO_MAINNET, hardfork::TaikoHardfork, spec::TaikoExecutorSpec,
    };
    use alloy_hardforks::ForkCondition;

    use super::apply_devnet_etna_override;

    #[test]
    fn parsed_custom_devnet_json_requires_the_explicit_etna_flag() {
        use std::io::Write;

        use alloy_consensus::BlockHeader;
        use clap::Parser;
        use reth::chainspec::EthChainSpec;
        use reth_cli_commands::NodeCommand;

        use crate::{TaikoChainSpecParser, TaikoCliExtArgs, command::TaikoNodeExtArgs};

        let _lock = crate::tests::env_lock();
        unsafe { std::env::remove_var("ALETHIA_RETH_DEVNET_ETNA_TIMESTAMP") };
        let mut json = serde_json::to_value(TAIKO_DEVNET.genesis()).unwrap();
        json["alloc"]["0x1670010000000000000000000000000000010001"] = serde_json::json!({
            "balance": "0x2a", "nonce": "0x1", "code": "0x60015f5500"
        });
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "{json}").unwrap();
        let path = file.path().to_str().unwrap();
        let mut plain = NodeCommand::<TaikoChainSpecParser, TaikoCliExtArgs>::try_parse_from([
            "alethia-reth",
            "--chain",
            path,
        ])
        .unwrap();
        let original = plain.chain.clone();
        apply_devnet_etna_override(&mut plain.chain, plain.ext.devnet_etna_timestamp());
        assert!(Arc::ptr_eq(&plain.chain, &original));
        assert!(!plain.chain.is_etna_active(u64::MAX));
        assert!(!plain.chain.is_unzen_active(0));
        assert_eq!(plain.chain.genesis_header().requests_hash, None);

        for timestamp in ["0", "100"] {
            let mut command =
                NodeCommand::<TaikoChainSpecParser, TaikoCliExtArgs>::try_parse_from([
                    "alethia-reth",
                    "--chain",
                    path,
                    "--devnet-etna-timestamp",
                    timestamp,
                ])
                .unwrap();
            apply_devnet_etna_override(&mut command.chain, command.ext.devnet_etna_timestamp());
            let timestamp: u64 = timestamp.parse().unwrap();
            assert!(command.chain.is_unzen_active(0));
            assert_eq!(command.chain.is_etna_active(0), timestamp == 0);
            if timestamp > 0 {
                assert!(!command.chain.is_etna_active(timestamp - 1));
            }
            assert!(command.chain.is_etna_active(timestamp));
            assert_eq!(command.chain.genesis(), original.genesis());
            assert_eq!(
                command.chain.genesis_header().state_root(),
                original.genesis_header().state_root()
            );
            assert_ne!(command.chain.genesis_hash(), original.genesis_hash());
            assert_ne!(command.chain.genesis_hash(), TAIKO_DEVNET.genesis_hash());
            assert!(command.chain.genesis_header().requests_hash.is_some());
        }
    }

    #[test]
    fn devnet_etna_override_applies_only_to_the_canonical_devnet() {
        let mut devnet = TAIKO_DEVNET.clone();
        apply_devnet_etna_override(&mut devnet, Some(0));
        assert_eq!(devnet.taiko_fork_activation(TaikoHardfork::Etna), ForkCondition::Timestamp(0));

        // An omitted flag and a non-devnet chain keep the original spec.
        let mut devnet = TAIKO_DEVNET.clone();
        apply_devnet_etna_override(&mut devnet, None);
        assert!(Arc::ptr_eq(&devnet, &TAIKO_DEVNET));
        let mut mainnet = TAIKO_MAINNET.clone();
        apply_devnet_etna_override(&mut mainnet, Some(0));
        assert!(Arc::ptr_eq(&mainnet, &TAIKO_MAINNET));
    }
}
