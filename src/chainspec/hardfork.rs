use std::sync::LazyLock;

use alloy_hardforks::{EthereumHardfork, ForkCondition, Hardfork, hardfork};
use reth::{chainspec::ChainHardforks, revm::primitives::U256};

hardfork!(
  /// The name of a Taiko hardfork.
  ///
  /// When building a list of hardforks for a chain, it's still expected to zip with
  /// [`EthereumHardfork`].
  #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
  TaikoHardfork {
      Ontake,
      Pacaya,
  }
);

impl TaikoHardfork {
    /// Taiko mainnet list of hardforks.
    pub const fn taiko_mainnet() -> [(Self, ForkCondition); 2] {
        [
            (Self::Ontake, ForkCondition::Block(538_304)),
            (Self::Pacaya, ForkCondition::Block(1_166_000)),
        ]
    }
}

/// Taiko mainnet list of hardforks.
pub static TAIKO_MAINNET_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(vec![
        (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
        (
            EthereumHardfork::SpuriousDragon.boxed(),
            ForkCondition::Block(0),
        ),
        (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
        (
            EthereumHardfork::Constantinople.boxed(),
            ForkCondition::Block(0),
        ),
        (
            EthereumHardfork::Petersburg.boxed(),
            ForkCondition::Block(0),
        ),
        (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
        (
            EthereumHardfork::MuirGlacier.boxed(),
            ForkCondition::Block(0),
        ),
        (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
        (
            EthereumHardfork::ArrowGlacier.boxed(),
            ForkCondition::Block(0),
        ),
        (
            EthereumHardfork::GrayGlacier.boxed(),
            ForkCondition::Block(0),
        ),
        (
            EthereumHardfork::Paris.boxed(),
            ForkCondition::TTD {
                activation_block_number: 0,
                fork_block: Some(0),
                total_difficulty: U256::ZERO,
            },
        ),
        (
            EthereumHardfork::Shanghai.boxed(),
            ForkCondition::Timestamp(0),
        ),
        (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(538_304)),
        (
            TaikoHardfork::Pacaya.boxed(),
            ForkCondition::Block(1_166_000),
        ),
    ])
});
