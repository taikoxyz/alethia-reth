use std::sync::Arc;

use reth_cli::chainspec::{ChainSpecParser, parse_genesis};

use crate::chainspec::{TAIKO_DEVNET, TAIKO_MAINNET, spec::TaikoChainSpec};

/// Chains supported by taiko-reth. First value should be used as the default.
pub const SUPPORTED_CHAINS: &[&str] = &["mainnet", "hekla", "devnet"];

/// Clap value parser for [`ChainSpec`]s.
///
/// The value parser matches either a known chain, the path
/// to a json file, or a json formatted string in-memory. The json needs to be a Genesis struct.
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<TaikoChainSpec>, eyre::Error> {
    Ok(match s {
        "mainnet" => TAIKO_MAINNET.clone(),
        "devnet" => TAIKO_DEVNET.clone(),
        _ => Arc::new(parse_genesis(s)?.into()),
    })
}

/// Taiko chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct TaikoChainSpecParser;

impl ChainSpecParser for TaikoChainSpecParser {
    type ChainSpec = TaikoChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<TaikoChainSpec>> {
        chain_value_parser(s)
    }
}
