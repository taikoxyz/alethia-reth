use std::sync::{Arc, LazyLock};

use alloy_chains::{Chain, NamedChain};
use reth::{
    chainspec::{ChainSpec, make_genesis_header},
    primitives::SealedHeader,
    revm::primitives::{U256, b256},
};

use crate::chainspec::hardfork::TAIKO_MAINNET_HARDFORKS;

pub mod hardfork;
pub mod parser;

/// The Taiko Mainnet spec
pub static TAIKO_MAINNET: LazyLock<Arc<ChainSpec>> = LazyLock::new(|| {
    // genesis contains empty alloc field because state at first bedrock block is imported
    // manually from trusted source
    let genesis = serde_json::from_str(include_str!("genesis/mainnet.json"))
        .expect("Can't deserialize Taiko Mainnet genesis json");
    let hardforks = TAIKO_MAINNET_HARDFORKS.clone();

    ChainSpec {
        chain: Chain::from_named(NamedChain::Taiko),
        genesis_header: SealedHeader::new(
            make_genesis_header(&genesis, &hardforks),
            b256!("0x90bc60466882de9637e269e87abab53c9108cf9113188bc4f80bcfcb10e489b9"),
        ),
        genesis,
        paris_block_and_final_difficulty: Some((0, U256::from(0))),
        hardforks,
        prune_delete_limit: 10000,
        ..Default::default()
    }
    .into()
});
