use std::sync::Arc;

use alethia_reth_chainspec::{TAIKO_DEVNET, TAIKO_HOODI, TAIKO_MAINNET, spec::TaikoChainSpec};
use reth_cli::chainspec::{ChainSpecParser, parse_genesis};

/// Chains supported by alethia-reth. First value should be used as the default.
pub const SUPPORTED_CHAINS: &[&str] = &["mainnet", "taiko-hoodi", "devnet"];

/// Clap value parser for [`ChainSpec`]s.
///
/// The value parser matches either a known chain, the path
/// to a json file, or a json formatted string in-memory. The json needs to be a Genesis struct.
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<TaikoChainSpec>, eyre::Error> {
    Ok(match s {
        "mainnet" => TAIKO_MAINNET.clone(),
        // Accept dashed and space-separated names;
        "taiko-hoodi" | "taiko hoodi" => TAIKO_HOODI.clone(),
        "devnet" => TAIKO_DEVNET.clone(),
        _ => Arc::new(parse_genesis(s)?.into()),
    })
}

/// Taiko chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct TaikoChainSpecParser;

impl ChainSpecParser for TaikoChainSpecParser {
    /// The chain specification type.
    type ChainSpec = TaikoChainSpec;

    /// List of supported chains.
    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    /// Parses the given string into a chain spec.
    ///
    /// # Arguments
    ///
    /// * `s` - A string slice that holds the chain spec to be parsed.
    ///
    /// # Errors
    ///
    /// This function will return an error if the input string cannot be parsed into a valid
    /// chain spec.
    fn parse(s: &str) -> eyre::Result<Arc<TaikoChainSpec>> {
        chain_value_parser(s)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_parse_chain_value_by_network_name() {
        let devnet =
            TaikoChainSpecParser::parse("devnet").expect("Failed to parse devnet chain spec");
        assert_eq!(devnet.inner.chain, 167001);

        let hoodi = TaikoChainSpecParser::parse("taiko-hoodi")
            .expect("Failed to parse taiko-hoodi chain spec");
        assert_eq!(hoodi.inner.chain, 167013);

        let mainnet =
            TaikoChainSpecParser::parse("mainnet").expect("Failed to parse mainnet chain spec");
        assert_eq!(mainnet.inner.chain, 167000);

        assert!(chain_value_parser("supported_network").is_err());
    }
}
