use std::sync::{Arc, LazyLock};

use alloy_primitives::B256;
use reth::{
    chainspec::{ChainSpec, make_genesis_header},
    primitives::SealedHeader,
    revm::primitives::{U256, b256},
};
use reth_ethereum_forks::ChainHardforks;

use crate::{
    hardfork::{TAIKO_DEVNET_HARDFORKS, TAIKO_MAINNET_HARDFORKS, TAIKO_TOLBA_HARDFORKS},
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

/// The Taiko Tolba spec
pub static TAIKO_TOLBA: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_tolba_chain_spec().into());

// Creates a new [`ChainSpec`] for the Taiko Mainnet network.
fn make_taiko_mainnet_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/mainnet.json"),
        b256!("0x90bc60466882de9637e269e87abab53c9108cf9113188bc4f80bcfcb10e489b9"),
        TAIKO_MAINNET_HARDFORKS.clone(),
    )
}

// Creates a new [`ChainSpec`] for the Taiko Devnet network.
fn make_taiko_devnet_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/devnet.json"),
        b256!("0xedb20e1f923f346991b12c96d786e97feb2bb510161fab8b45b598bef8a77876"),
        TAIKO_DEVNET_HARDFORKS.clone(),
    )
}

// Creates a new [`ChainSpec`] for the Taiko Tolba network.
fn make_taiko_tolba_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/tolba.json"),
        b256!("0x9fc37d0b7b80fb9a43a876ab11fa87d822cbe64df558c1158bba731c57dea75a"),
        TAIKO_TOLBA_HARDFORKS.clone(),
    )
}

// Creates a new [`ChainSpec`] for the Taiko network with the given genesis JSON and double-check
// the given genesis hash.
fn make_taiko_chain_spec(
    genesis_json: &str,
    genesis_hash: B256,
    hardforks: ChainHardforks,
) -> TaikoChainSpec {
    // Import the genesis JSON file and deserialize it.
    let genesis = serde_json::from_str(genesis_json).expect("Can't deserialize Taiko genesis json");
    // Ensure the genesis hash matches the expected value.
    let genesis_header = SealedHeader::new(make_genesis_header(&genesis, &hardforks), genesis_hash);

    let inner = ChainSpec {
        chain: genesis.config.chain_id.into(),
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
    fn test_tolba_genesis_json_hash() {
        let genesis_header_hash =
            make_taiko_tolba_chain_spec().inner.genesis_header.hash_slow().to_string();

        assert_eq!(
            "0x9fc37d0b7b80fb9a43a876ab11fa87d822cbe64df558c1158bba731c57dea75a",
            genesis_header_hash
        );
    }
}
