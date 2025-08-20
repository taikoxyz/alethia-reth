use std::sync::{Arc, LazyLock};

use reth_chainspec::{ChainSpec, make_genesis_header};
use reth_ethereum_forks::ChainHardforks;
use reth_primitives::SealedHeader;
use reth_revm::primitives::{B256, U256, b256};

use crate::chainspec::{
    hardfork::{TAIKO_DEVNET_HARDFORKS, TAIKO_HEKLA_HARDFORKS, TAIKO_MAINNET_HARDFORKS},
    spec::TaikoChainSpec,
};

pub mod hardfork;
#[cfg(feature = "node")]
pub mod parser;
pub mod spec;

/// The Taiko Mainnet spec
pub static TAIKO_MAINNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_mainnet_chain_spec().into());

/// The Taiko Devnet spec
pub static TAIKO_DEVNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_devnet_chain_spec().into());

/// The Taiko Hekla spec
pub static TAIKO_HEKLA: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| make_taiko_hekla_chain_spec().into());

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
        b256!("0xddd9d042b4256c630ebd9106f868dd8fc870721ef2e2722ee426890624db648d"),
        TAIKO_DEVNET_HARDFORKS.clone(),
    )
}

// Creates a new [`ChainSpec`] for the Taiko Hekla network.
fn make_taiko_hekla_chain_spec() -> TaikoChainSpec {
    make_taiko_chain_spec(
        include_str!("genesis/hekla.json"),
        b256!("0x1f5554042aa50dc0712936ae234d8803b80b84251f85d074756a2f391896e109"),
        TAIKO_HEKLA_HARDFORKS.clone(),
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
    fn test_mainnet_genesis_json_hash() {
        let genesis_header_hash =
            make_taiko_hekla_chain_spec().inner.genesis_header.hash_slow().to_string();

        assert_eq!(
            "0x1f5554042aa50dc0712936ae234d8803b80b84251f85d074756a2f391896e109",
            genesis_header_hash
        );
    }
}
