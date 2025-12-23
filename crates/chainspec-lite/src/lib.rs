use std::sync::{Arc, LazyLock};

use alloy_primitives::{B256, b256};

pub mod spec {
    use alloy_primitives::B256;

    /// Minimal Taiko chain spec for guest builds.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TaikoChainSpec {
        pub chain_id: u64,
        pub genesis_hash: B256,
    }

    impl TaikoChainSpec {
        pub const fn new(chain_id: u64, genesis_hash: B256) -> Self {
            Self { chain_id, genesis_hash }
        }

        pub const fn chain_id(&self) -> u64 {
            self.chain_id
        }

        pub const fn genesis_hash(&self) -> B256 {
            self.genesis_hash
        }
    }

    impl Default for TaikoChainSpec {
        fn default() -> Self {
            Self { chain_id: 0, genesis_hash: B256::ZERO }
        }
    }
}

pub use spec::TaikoChainSpec;

/// Genesis hash for the Taiko Devnet network.
pub const TAIKO_DEVNET_GENESIS_HASH: B256 =
    b256!("0x4f6dd2f48a8521061441af207cb74a69696310a3f16d289ac1c4aa39fc01c741");

/// Genesis hash for the Taiko Hoodi network.
pub const TAIKO_HOODI_GENESIS_HASH: B256 =
    b256!("0x8e3d16acf3ecc1fbe80309b04e010b90c9ccb3da14e98536cfe66bb93407d228");

/// Genesis hash for the Taiko Mainnet network.
pub const TAIKO_MAINNET_GENESIS_HASH: B256 =
    b256!("0x90bc60466882de9637e269e87abab53c9108cf9113188bc4f80bcfcb10e489b9");

/// The Taiko Mainnet spec.
pub static TAIKO_MAINNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| Arc::new(TaikoChainSpec::new(167000, TAIKO_MAINNET_GENESIS_HASH)));

/// The Taiko Devnet spec.
pub static TAIKO_DEVNET: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| Arc::new(TaikoChainSpec::new(167001, TAIKO_DEVNET_GENESIS_HASH)));

/// The Taiko Hoodi spec.
pub static TAIKO_HOODI: LazyLock<Arc<TaikoChainSpec>> =
    LazyLock::new(|| Arc::new(TaikoChainSpec::new(167013, TAIKO_HOODI_GENESIS_HASH)));
