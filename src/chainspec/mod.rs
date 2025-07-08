use std::sync::{Arc, LazyLock};

use alloy_chains::{Chain, NamedChain};
use reth::{
    chainspec::{ChainSpec, make_genesis_header},
    primitives::SealedHeader,
    revm::primitives::{U256, b256},
};

use crate::chainspec::{
    hardfork::{TAIKO_DEVNET_HARDFORKS, TAIKO_MAINNET_HARDFORKS},
    spec::TaikoChainSpec,
};

pub mod hardfork;
pub mod parser;
pub mod spec;

/// The Taiko Mainnet spec
pub static TAIKO_MAINNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_mainnet_chain_spec().into());

/// The Taiko Devnet spec
pub static TAIKO_DEVNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_devnet_chain_spec().into());

/// Creates a new [`ChainSpec`] for the Taiko network.
/// TODO: support other networks in the future.
fn make_taiko_mainnet_chain_spec() -> TaikoChainSpec {
    // genesis contains empty alloc field because state at first bedrock block is imported
    // manually from trusted source
    let genesis = serde_json::from_str(include_str!("genesis/mainnet.json"))
        .expect("Can't deserialize Taiko Mainnet genesis json");
    let hardforks = TAIKO_MAINNET_HARDFORKS.clone();
    let genesis_header = SealedHeader::new(
        make_genesis_header(&genesis, &hardforks),
        b256!("0x90bc60466882de9637e269e87abab53c9108cf9113188bc4f80bcfcb10e489b9"),
    );

    let inner = ChainSpec {
        chain: Chain::from_named(NamedChain::Taiko),
        genesis_header,
        genesis,
        paris_block_and_final_difficulty: Some((0, U256::from(0))),
        hardforks,
        prune_delete_limit: 10000,
        ..Default::default()
    };

    TaikoChainSpec { inner }
}

/// Creates a new [`ChainSpec`] for the Taiko network.
/// TODO: support other networks in the future.
fn make_taiko_devnet_chain_spec() -> TaikoChainSpec {
    // genesis contains empty alloc field because state at first bedrock block is imported
    // manually from trusted source
    let genesis = serde_json::from_str(include_str!("genesis/devnet.json"))
        .expect("Can't deserialize Taiko Devnet genesis json");
    let hardforks = TAIKO_DEVNET_HARDFORKS.clone();
    let genesis_header = SealedHeader::new(
        make_genesis_header(&genesis, &hardforks),
        b256!("0xddd9d042b4256c630ebd9106f868dd8fc870721ef2e2722ee426890624db648d"),
    );

    let inner = ChainSpec {
        chain: 167001.into(),
        genesis_header,
        genesis,
        paris_block_and_final_difficulty: Some((0, U256::from(0))),
        hardforks,
        prune_delete_limit: 10000,
        ..Default::default()
    };

    TaikoChainSpec { inner }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_mainnet_genesis_json_hash() {
        let genesis_header_hash = make_taiko_mainnet_chain_spec()
            .inner
            .genesis_header
            .hash_slow()
            .to_string();

        assert_eq!(
            "0x90bc60466882de9637e269e87abab53c9108cf9113188bc4f80bcfcb10e489b9",
            genesis_header_hash
        );
    }
}
