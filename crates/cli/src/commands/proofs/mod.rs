//! Alethia-specific CLI subcommands for managing the historical-proofs sidecar storage.

use std::sync::Arc;

use clap::Subcommand;
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_commands::common::CliNodeTypes;
use reth_ethereum::EthPrimitives;

use alethia_reth_chainspec::spec::TaikoChainSpec;

use crate::TaikoChainSpecParser;

pub mod init;
pub mod prune;
pub mod unwind;

/// Top-level alethia-reth extension subcommand exposed via `reth`'s `SubCmd` hook.
///
/// This enum has a single variant (`Proofs`) that renders as the `proofs` subcommand at the CLI
/// root. `reth` delegates to it through the `Commands::Ext(_)` arm on its top-level command enum.
#[derive(Debug, Subcommand)]
pub enum ProofsCommand<C: ChainSpecParser = TaikoChainSpecParser> {
    /// Manage storage of historical proofs in the bounded-history proofs sidecar.
    #[command(name = "proofs", subcommand)]
    Proofs(ProofsSubcommands<C>),
}

impl<C: ChainSpecParser<ChainSpec = TaikoChainSpec>> ProofsCommand<C> {
    /// Execute the selected `proofs` subcommand.
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec, Primitives = EthPrimitives>>(
        self,
        runtime: reth::tasks::Runtime,
    ) -> eyre::Result<()> {
        match self {
            Self::Proofs(ProofsSubcommands::Init(cmd)) => cmd.execute::<N>(runtime).await,
            Self::Proofs(ProofsSubcommands::Prune(cmd)) => cmd.execute::<N>(runtime).await,
            Self::Proofs(ProofsSubcommands::Unwind(cmd)) => cmd.execute::<N>(runtime).await,
        }
    }
}

impl<C: ChainSpecParser> ProofsCommand<C> {
    /// Returns the underlying chain being used to run this command.
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        match self {
            Self::Proofs(ProofsSubcommands::Init(cmd)) => cmd.chain_spec(),
            Self::Proofs(ProofsSubcommands::Prune(cmd)) => cmd.chain_spec(),
            Self::Proofs(ProofsSubcommands::Unwind(cmd)) => cmd.chain_spec(),
        }
    }
}

/// Subcommands under `alethia-reth proofs`.
#[derive(Debug, Subcommand)]
pub enum ProofsSubcommands<C: ChainSpecParser> {
    /// Initialize the proofs storage with the current state of the chain.
    #[command(name = "init")]
    Init(init::InitCommand<C>),
    /// Prune old proof history to reclaim space.
    #[command(name = "prune")]
    Prune(prune::PruneCommand<C>),
    /// Unwind the proofs storage to a specific block number.
    #[command(name = "unwind")]
    Unwind(unwind::UnwindCommand<C>),
}
