#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko chain-spec presets and genesis helpers.
use std::sync::{Arc, LazyLock};

use alloy_genesis::Genesis;
use alloy_primitives::{B256, U256, b256};
use reth_chainspec::ChainSpec;
use reth_ethereum_forks::ChainHardforks;

use crate::{
    hardfork::{
        TAIKO_DEVNET_HARDFORKS, TAIKO_HOODI_HARDFORKS, TAIKO_MAINNET_HARDFORKS,
        TAIKO_MASAYA_HARDFORKS,
    },
    spec::TaikoChainSpec,
};

/// Taiko hardfork identifiers and activation tables.
pub mod hardfork;
/// Taiko chain-spec wrapper traits and helper methods.
pub mod spec;

/// Genesis hash for the Taiko Devnet network.
pub const TAIKO_DEVNET_GENESIS_HASH: B256 =
    b256!("0x181426bdc3a02414b1aa667b273a288575d7dc645f0f8bea3f742e5f9a70f58a");

/// Genesis hash for the Taiko Hoodi network.
pub const TAIKO_HOODI_GENESIS_HASH: B256 =
    b256!("0x8e3d16acf3ecc1fbe80309b04e010b90c9ccb3da14e98536cfe66bb93407d228");

/// Genesis hash for the Taiko Mainnet network.
pub const TAIKO_MAINNET_GENESIS_HASH: B256 =
    b256!("0x90bc60466882de9637e269e87abab53c9108cf9113188bc4f80bcfcb10e489b9");

/// Genesis hash for the Taiko Masaya network.
pub const TAIKO_MASAYA_GENESIS_HASH: B256 =
    b256!("0xeef96dc254e1ac4a0044b116e38b16dface1a153d9299c056552898a43f8513e");

/// The Taiko Mainnet spec
pub static TAIKO_MAINNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_mainnet_chain_spec().into());

/// The Taiko Devnet spec
pub static TAIKO_DEVNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_devnet_chain_spec().into());

/// The Taiko Hoodi spec
pub static TAIKO_HOODI: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_hoodi_chain_spec().into());

/// The Taiko Masaya spec
pub static TAIKO_MASAYA: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_masaya_chain_spec().into());

/// Create a new [`ChainSpec`] for the Taiko Devnet network.
fn make_taiko_devnet_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/devnet.json"),
        TAIKO_DEVNET_GENESIS_HASH,
        TAIKO_DEVNET_HARDFORKS.clone(),
    )
}

/// Create a new [`ChainSpec`] for the Taiko Hoodi network.
fn make_taiko_hoodi_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/taiko-hoodi.json"),
        TAIKO_HOODI_GENESIS_HASH,
        TAIKO_HOODI_HARDFORKS.clone(),
    )
}

/// Create a new [`ChainSpec`] for the Taiko Mainnet network.
fn make_taiko_mainnet_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/mainnet.json"),
        TAIKO_MAINNET_GENESIS_HASH,
        TAIKO_MAINNET_HARDFORKS.clone(),
    )
}

/// Create a new [`ChainSpec`] for the Taiko Masaya network.
fn make_taiko_masaya_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/masaya.json"),
        TAIKO_MASAYA_GENESIS_HASH,
        TAIKO_MASAYA_HARDFORKS.clone(),
    )
}

/// Create a new [`ChainSpec`] for Taiko from JSON genesis data and a known expected hash.
fn make_taiko_chain_spec(
    genesis_json: &str,
    genesis_hash: B256,
    hardforks: ChainHardforks,
) -> TaikoChainSpec {
    // Import the genesis JSON file and deserialize it.
    let genesis: Genesis =
        serde_json::from_str(genesis_json).expect("Can't deserialize Taiko genesis json");
    let mut inner = ChainSpec::builder()
        .chain(genesis.config.chain_id.into())
        .genesis(genesis)
        .with_forks(hardforks)
        .build();
    inner.paris_block_and_final_difficulty = Some((0, U256::ZERO));
    assert_eq!(inner.genesis_hash(), genesis_hash, "unexpected Taiko genesis hash");

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
                TAIKO_DEVNET_GENESIS_HASH,
            ),
            (
                "taiko-hoodi",
                make_taiko_hoodi_chain_spec as fn() -> TaikoChainSpec,
                TAIKO_HOODI_GENESIS_HASH,
            ),
            (
                "mainnet",
                make_taiko_mainnet_chain_spec as fn() -> TaikoChainSpec,
                TAIKO_MAINNET_GENESIS_HASH,
            ),
            (
                "masaya",
                make_taiko_masaya_chain_spec as fn() -> TaikoChainSpec,
                TAIKO_MASAYA_GENESIS_HASH,
            ),
        ];

        for (name, make_spec, expected_hash) in cases {
            let spec = make_spec();
            let computed_hash = spec.inner.genesis_header.hash_slow();
            assert_eq!(expected_hash, computed_hash, "genesis hash mismatch for {name}");
        }
    }
}
