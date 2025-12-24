use std::sync::LazyLock;

use alloy_hardforks::{EthereumHardfork, ForkCondition, Hardfork};
use rayon::prelude::ParallelExtend;
use reth_chainspec::ChainHardforks;
use reth_revm::primitives::U256;

pub use alethia_reth_forks::hardfork::{TaikoHardfork, TaikoHardforks};

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
pub static TAIKO_DEVNET_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(extend_with_shared_hardforks(vec![
        (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Shasta.boxed(), ForkCondition::Timestamp(0)),
    ]))
});

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
    fn test_devnet_shasta_uses_timestamp_activation() {
        let shasta = TAIKO_DEVNET_HARDFORKS.fork(TaikoHardfork::Shasta);
        assert!(shasta.is_timestamp(), "shasta activation should be timestamp-based");
        assert_eq!(shasta, ForkCondition::Timestamp(0));
    }
}
