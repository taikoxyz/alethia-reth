use std::sync::{Arc, LazyLock};

use alloy_chains::{Chain, NamedChain};
use alloy_genesis::Genesis;
use alloy_hardforks::{ForkCondition, Hardfork};
use derive_more::From;
use reth::{
    chainspec::{ChainHardforks, ChainSpec, ChainSpecBuilder, make_genesis_header},
    primitives::SealedHeader,
    revm::primitives::{U256, b256},
};

use crate::chainspec::{
    hardfork::{TAIKO_MAINNET_HARDFORKS, TaikoHardfork},
    spec::TaikoChainSpec,
};

pub mod hardfork;
pub mod spec;

/// The Taiko Mainnet spec
pub static TAIKO_MAINNET: LazyLock<Arc<TaikoChainSpec>> = LazyLock::new(|| {
    // genesis contains empty alloc field because state at first bedrock block is imported
    // manually from trusted source
    let genesis = serde_json::from_str(include_str!("genesis/mainnet.json"))
        .expect("Can't deserialize Taiko Mainnet genesis json");
    let hardforks = TAIKO_MAINNET_HARDFORKS.clone();
    TaikoChainSpec {
        inner: ChainSpec {
            chain: Chain::from_named(NamedChain::Taiko),
            genesis_header: SealedHeader::new(
                make_genesis_header(&genesis, &hardforks),
                b256!("0x7ca38a1916c42007829c55e69d3e9a73265554b586a499015373241b8a3fa48b"),
            ),
            genesis,
            paris_block_and_final_difficulty: Some((0, U256::from(0))),
            hardforks,
            prune_delete_limit: 10000,
            ..Default::default()
        },
    }
    .into()
});

/// Chain spec builder for a Taiko chain.
#[derive(Debug, Default, From)]
pub struct TaikoChainSpecBuilder {
    /// [`ChainSpecBuilder`]
    inner: ChainSpecBuilder,
}

impl TaikoChainSpecBuilder {
    /// Construct a new builder from the Taiko mainnet chain spec.
    pub fn mainnet() -> Self {
        let mut inner = ChainSpecBuilder::default()
            .chain(TAIKO_MAINNET.chain)
            .genesis(TAIKO_MAINNET.genesis.clone());
        let forks = TAIKO_MAINNET.hardforks.clone();
        inner = inner.with_forks(forks);

        Self { inner }
    }
}

impl TaikoChainSpecBuilder {
    /// Set the chain ID
    pub fn chain(mut self, chain: Chain) -> Self {
        self.inner = self.inner.chain(chain);
        self
    }

    /// Set the genesis block.
    pub fn genesis(mut self, genesis: Genesis) -> Self {
        self.inner = self.inner.genesis(genesis);
        self
    }

    /// Add the given fork with the given activation condition to the spec.
    pub fn with_fork<H: Hardfork>(mut self, fork: H, condition: ForkCondition) -> Self {
        self.inner = self.inner.with_fork(fork, condition);
        self
    }

    /// Add the given forks with the given activation condition to the spec.
    pub fn with_forks(mut self, forks: ChainHardforks) -> Self {
        self.inner = self.inner.with_forks(forks);
        self
    }

    /// Remove the given fork from the spec.
    pub fn without_fork(mut self, fork: TaikoHardfork) -> Self {
        self.inner = self.inner.without_fork(fork);
        self
    }

    /// Enable Ontake at genesis
    pub fn ontake_activated(mut self) -> Self {
        self.inner = self.inner.paris_activated();
        self.inner = self
            .inner
            .with_fork(TaikoHardfork::Ontake, ForkCondition::Block(0));
        self
    }

    /// Enable Pacaya at genesis
    pub fn pacaya_activated(mut self) -> Self {
        self = self.ontake_activated();
        self.inner = self
            .inner
            .with_fork(TaikoHardfork::Pacaya, ForkCondition::Block(0));
        self
    }

    /// Build the resulting [`TaikoChainSpec`].
    ///
    /// # Panics
    ///
    /// This function panics if the chain ID and genesis is not set ([`Self::chain`] and
    /// [`Self::genesis`])
    pub fn build(self) -> TaikoChainSpec {
        let mut inner = self.inner.build();
        inner.genesis_header =
            SealedHeader::seal_slow(make_genesis_header(&inner.genesis, &inner.hardforks));

        TaikoChainSpec { inner }
    }
}
