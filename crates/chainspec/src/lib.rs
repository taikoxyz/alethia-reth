use std::sync::{Arc, LazyLock};

use alloy_genesis::Genesis;
use alloy_primitives::B256;
use reth::{
    chainspec::{ChainSpec, make_genesis_header},
    primitives::SealedHeader,
    revm::primitives::{U256, b256},
};
use reth_ethereum_forks::ChainHardforks;

use crate::{
    hardfork::{
        DEFAULT_DEVNET_SHASTA_TIMESTAMP, TAIKO_HOODI_HARDFORKS, TAIKO_MAINNET_HARDFORKS,
        taiko_devnet_hardforks,
    },
    spec::TaikoChainSpec,
};

pub mod hardfork;
pub mod parser;
pub mod spec;

/// Genesis hash for the Taiko Devnet network.
pub const TAIKO_DEVNET_GENESIS_HASH: B256 =
    b256!("0x80b1e24302410b2a00bc53d75ce8daa89946269083294e94a2cc5cee721e543e");

/// Genesis hash for the Taiko Hoodi network.
pub const TAIKO_HOODI_GENESIS_HASH: B256 =
    b256!("0x8e3d16acf3ecc1fbe80309b04e010b90c9ccb3da14e98536cfe66bb93407d228");

/// Genesis hash for the Taiko Mainnet network.
pub const TAIKO_MAINNET_GENESIS_HASH: B256 =
    b256!("0x90bc60466882de9637e269e87abab53c9108cf9113188bc4f80bcfcb10e489b9");

/// The Taiko Mainnet spec
pub static TAIKO_MAINNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_mainnet_chain_spec().into());

/// The Taiko Devnet spec
pub static TAIKO_DEVNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_devnet_chain_spec().into());

/// The Taiko Hoodi spec
pub static TAIKO_HOODI: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_hoodi_chain_spec().into());

// Creates a new [`ChainSpec`] for the Taiko Devnet network.
fn make_taiko_devnet_chain_spec() -> TaikoChainSpec {
    taiko_devnet_chain_spec_with_shasta_timestamp(DEFAULT_DEVNET_SHASTA_TIMESTAMP)
}

// Creates a new [`ChainSpec`] for the Taiko Hoodi network.
fn make_taiko_hoodi_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/taiko-hoodi.json"),
        TAIKO_HOODI_GENESIS_HASH,
        TAIKO_HOODI_HARDFORKS.clone(),
    )
}

// Creates a new [`ChainSpec`] for the Taiko Mainnet network.
fn make_taiko_mainnet_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/mainnet.json"),
        TAIKO_MAINNET_GENESIS_HASH,
        TAIKO_MAINNET_HARDFORKS.clone(),
    )
}

/// Builds a Taiko Devnet chain specification with a custom Shasta activation timestamp.
pub fn taiko_devnet_chain_spec_with_shasta_timestamp(shasta_timestamp: u64) -> TaikoChainSpec {
    let mut genesis = parse_taiko_genesis(include_str!("genesis/devnet.json"));
    genesis
        .config
        .extra_fields
        .insert_value("shastaTime".to_string(), shasta_timestamp)
        .expect("failed to encode custom shastaTime in devnet genesis");

    make_taiko_chain_spec_from_genesis(
        genesis,
        TAIKO_DEVNET_GENESIS_HASH,
        taiko_devnet_hardforks(shasta_timestamp),
    )
}

// Creates a new [`ChainSpec`] for the Taiko network with the given genesis JSON and double-check
// the given genesis hash.
fn make_taiko_chain_spec(
    genesis_json: &str,
    genesis_hash: B256,
    hardforks: ChainHardforks,
) -> TaikoChainSpec {
    let genesis = parse_taiko_genesis(genesis_json);
    make_taiko_chain_spec_from_genesis(genesis, genesis_hash, hardforks)
}

fn parse_taiko_genesis(genesis_json: &str) -> Genesis {
    serde_json::from_str(genesis_json).expect("Can't deserialize Taiko genesis json")
}

fn make_taiko_chain_spec_from_genesis(
    genesis: Genesis,
    genesis_hash: B256,
    hardforks: ChainHardforks,
) -> TaikoChainSpec {
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
        ];

        for (name, make_spec, expected_hash) in cases {
            let spec = make_spec();
            let computed_hash = spec.inner.genesis_header.hash_slow();
            assert_eq!(expected_hash, computed_hash, "genesis hash mismatch for {name}");
        }
    }
}
