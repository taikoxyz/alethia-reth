pub use alethia_reth_chainspec_core::{
    TAIKO_DEVNET, TAIKO_DEVNET_GENESIS_HASH, TAIKO_HOODI, TAIKO_HOODI_GENESIS_HASH, TAIKO_MAINNET,
    TAIKO_MAINNET_GENESIS_HASH,
};

pub mod hardfork;

pub mod spec {
    pub use alethia_reth_chainspec_core::spec::*;
}

pub mod parser;
