use std::sync::{Arc, LazyLock};

use alloy_primitives::B256;
use reth::{
    chainspec::{ChainSpec, make_genesis_header},
    primitives::SealedHeader,
    revm::primitives::{U256, b256},
};
use reth_ethereum_forks::ChainHardforks;

use crate::{
    hardfork::{TAIKO_DEVNET_HARDFORKS, TAIKO_HOODI_HARDFORKS, TAIKO_MAINNET_HARDFORKS},
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

/// The Taiko Hoodi spec
pub static TAIKO_HOODI: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_hoodi_chain_spec().into());

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
        b256!("0xe08a1549ccd3aeb6cb5e3e1d671af129ac05bc4b5fb6f3d5b6c4cc226aa18a2b"),
        TAIKO_DEVNET_HARDFORKS.clone(),
    )
}

// Creates a new [`ChainSpec`] for the Taiko Hoodi network.
fn make_taiko_hoodi_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/taiko-hoodi.json"),
        b256!("0x8e3d16acf3ecc1fbe80309b04e010b90c9ccb3da14e98536cfe66bb93407d228"),
        TAIKO_HOODI_HARDFORKS.clone(),
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
    fn test_taiko_genesis_json_hashes() {
        let cases = [
            (
                "devnet",
                make_taiko_devnet_chain_spec as fn() -> TaikoChainSpec,
                "0xe08a1549ccd3aeb6cb5e3e1d671af129ac05bc4b5fb6f3d5b6c4cc226aa18a2b",
            ),
            (
                "taiko-hoodi",
                make_taiko_hoodi_chain_spec as fn() -> TaikoChainSpec,
                "0x8e3d16acf3ecc1fbe80309b04e010b90c9ccb3da14e98536cfe66bb93407d228",
            ),
            (
                "mainnet",
                make_taiko_mainnet_chain_spec as fn() -> TaikoChainSpec,
                "0x90bc60466882de9637e269e87abab53c9108cf9113188bc4f80bcfcb10e489b9",
            ),
        ];

        for (name, make_spec, expected_hash) in cases {
            let spec = make_spec();
            let computed_hash = spec.inner.genesis_header.hash_slow().to_string();
            assert_eq!(expected_hash, computed_hash, "genesis hash mismatch for {name}");
        }
    }
}
