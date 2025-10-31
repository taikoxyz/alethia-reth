use std::sync::LazyLock;

use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition, Hardfork, hardfork};
use reth::{chainspec::ChainHardforks, revm::primitives::U256};
use reth_trie_common::iter::ParallelExtend;

use crate::spec::TaikoChainSpec;

/// Default activation timestamp used for the Shasta hardfork on Taiko devnet.
pub const DEFAULT_DEVNET_SHASTA_TIMESTAMP: u64 = 0;

hardfork!(
  /// The name of a Taiko hardfork.
  ///
  /// When building a list of hardforks for a chain, it's still expected to zip with
  /// [`EthereumHardfork`].
  #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
  TaikoHardfork {
      Ontake,
      Pacaya,
      Shasta,
  }
);

/// Extends [`EthereumHardforks`] with Taiko network helper methods.
#[auto_impl::auto_impl(&, Arc)]
pub trait TaikoHardforks: EthereumHardforks {
    /// Retrieves [`ForkCondition`] by an [`TaikoHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn taiko_fork_activation(&self, fork: TaikoHardfork) -> ForkCondition;

    /// Convenience method to check if [`TaikoHardfork::Ontake`] is active at a given block
    /// number.
    fn is_ontake_active_at_block(&self, block_number: u64) -> bool {
        self.taiko_fork_activation(TaikoHardfork::Ontake).active_at_block(block_number)
    }

    /// Convenience method to check if [`TaikoHardfork::Pacaya`] is active at a given block
    /// number.
    fn is_pacaya_active_at_block(&self, block_number: u64) -> bool {
        self.taiko_fork_activation(TaikoHardfork::Pacaya).active_at_block(block_number)
    }

    /// Convenience method to check if [`TaikoHardfork::Shasta`] is active at the given timestamp
    /// and block number.
    fn is_shasta_active(&self, timestamp: u64, block_number: u64) -> bool {
        self.taiko_fork_activation(TaikoHardfork::Shasta).active_at_timestamp(timestamp) &&
            self.ethereum_fork_activation(EthereumHardfork::London).active_at_block(block_number)
    }
}

impl TaikoHardforks for TaikoChainSpec {
    /// Retrieves [`ForkCondition`] from `fork`. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn taiko_fork_activation(&self, fork: TaikoHardfork) -> ForkCondition {
        match fork {
            TaikoHardfork::Ontake => self.inner.fork(TaikoHardfork::Ontake),
            TaikoHardfork::Pacaya => self.inner.fork(TaikoHardfork::Pacaya),
            TaikoHardfork::Shasta => self.inner.fork(TaikoHardfork::Shasta),
        }
    }
}

/// Taiko Mainnet list of hardforks.
pub static TAIKO_MAINNET_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(extend_with_shared_hardforks(vec![
        (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(538_304)),
        (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(1_166_000)),
        (TaikoHardfork::Shasta.boxed(), ForkCondition::Never),
    ]))
});

/// Taiko Hoodi list of hardforks.
pub static TAIKO_HOODI_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(extend_with_shared_hardforks(vec![
        (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Shasta.boxed(), ForkCondition::Never),
    ]))
});

/// Taiko Devnet list of hardforks.
pub static TAIKO_DEVNET_HARDFORKS: LazyLock<ChainHardforks> =
    LazyLock::new(|| taiko_devnet_hardforks(DEFAULT_DEVNET_SHASTA_TIMESTAMP));

/// Build the Taiko devnet hardfork schedule with a custom Shasta activation timestamp.
pub fn taiko_devnet_hardforks(shasta_timestamp: u64) -> ChainHardforks {
    ChainHardforks::new(extend_with_shared_hardforks(vec![
        (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Shasta.boxed(), ForkCondition::Timestamp(shasta_timestamp)),
    ]))
}

// Extend the given hardforks with shared common Ethereum hardforks.
fn extend_with_shared_hardforks(
    hardforks: Vec<(Box<dyn Hardfork>, ForkCondition)>,
) -> Vec<(Box<dyn Hardfork>, ForkCondition)> {
    let mut shared_hardforks = vec![
        (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::ArrowGlacier.boxed(), ForkCondition::Block(0)),
        (EthereumHardfork::GrayGlacier.boxed(), ForkCondition::Block(0)),
        (
            EthereumHardfork::Paris.boxed(),
            ForkCondition::TTD {
                activation_block_number: 0,
                fork_block: Some(0),
                total_difficulty: U256::ZERO,
            },
        ),
        (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)),
    ];

    shared_hardforks.par_extend(hardforks);

    shared_hardforks
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_extend_with_shared_hardforks() {
        let extra_forks = vec![
            (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(1)),
            (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(2)),
        ];
        let forks = extend_with_shared_hardforks(extra_forks.clone());
        assert!(forks.len() > extra_forks.len());
    }

    #[test]
    fn test_devnet_hardforks_custom_timestamp() {
        let custom_ts = 42;
        let hardforks = taiko_devnet_hardforks(custom_ts);
        let shasta = hardforks.fork(TaikoHardfork::Shasta);
        assert!(
            matches!(shasta, ForkCondition::Timestamp(ts) if ts == custom_ts),
            "expected shasta fork to use custom timestamp"
        );
    }

    #[test]
    fn test_devnet_shasta_uses_timestamp_activation() {
        let shasta = TAIKO_DEVNET_HARDFORKS.fork(TaikoHardfork::Shasta);
        assert!(shasta.is_timestamp(), "shasta activation should be timestamp-based");
    }
}
