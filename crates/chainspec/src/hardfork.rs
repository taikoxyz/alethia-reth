//! Taiko-specific hardfork identifiers and activation schedules.
use std::sync::LazyLock;

use alloy_hardforks::{EthereumHardfork, ForkCondition, Hardfork, hardfork};
use alloy_primitives::U256;
use reth_ethereum_forks::{ChainHardforks, EthereumHardforks};

use crate::spec::TaikoChainSpec;

hardfork!(
  /// The name of a Taiko hardfork.
  ///
  /// When building a list of hardforks for a chain, it's still expected to zip with
  /// [`EthereumHardfork`].
  #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
  TaikoHardfork {
      /// Ontake protocol upgrade.
      Ontake,
      /// Pacaya protocol upgrade.
      Pacaya,
      /// Shasta protocol upgrade.
      Shasta,
      /// Unzen protocol upgrade.
      Unzen,
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

    /// Convenience method to check if [`TaikoHardfork::Shasta`] is active at the given timestamp.
    ///
    /// Taiko chains always activate London at genesis, so Shasta is effectively gate-kept by the
    /// timestamp condition alone.
    fn is_shasta_active(&self, timestamp: u64) -> bool {
        self.taiko_fork_activation(TaikoHardfork::Shasta).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`TaikoHardfork::Unzen`] is active at the given timestamp.
    fn is_unzen_active(&self, timestamp: u64) -> bool {
        self.taiko_fork_activation(TaikoHardfork::Unzen).active_at_timestamp(timestamp)
    }
}

impl TaikoHardforks for TaikoChainSpec {
    /// Retrieves [`ForkCondition`] from `fork`. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn taiko_fork_activation(&self, fork: TaikoHardfork) -> ForkCondition {
        self.inner.fork(fork)
    }
}

/// Taiko Mainnet list of hardforks.
pub static TAIKO_MAINNET_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(extend_with_shared_hardforks(vec![
        (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(538_304)),
        (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(1_166_000)),
        (TaikoHardfork::Shasta.boxed(), ForkCondition::Timestamp(1_775_135_700)),
        (TaikoHardfork::Unzen.boxed(), ForkCondition::Never),
    ]))
});

/// Taiko Hoodi list of hardforks.
pub static TAIKO_HOODI_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(extend_with_shared_hardforks(vec![
        (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Shasta.boxed(), ForkCondition::Timestamp(1_770_296_400)),
        (TaikoHardfork::Unzen.boxed(), ForkCondition::Timestamp(1_781_787_600)),
    ]))
});

/// Taiko Devnet list of hardforks.
pub static TAIKO_DEVNET_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(extend_with_shared_hardforks(vec![
        (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Shasta.boxed(), ForkCondition::Timestamp(0)),
        (TaikoHardfork::Unzen.boxed(), ForkCondition::Timestamp(0)),
    ]))
});

/// Taiko Masaya list of hardforks.
pub static TAIKO_MASAYA_HARDFORKS: LazyLock<ChainHardforks> = LazyLock::new(|| {
    ChainHardforks::new(extend_with_shared_hardforks(vec![
        (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(0)),
        (TaikoHardfork::Shasta.boxed(), ForkCondition::Timestamp(0)),
        (TaikoHardfork::Unzen.boxed(), ForkCondition::Timestamp(0)),
    ]))
});

/// Extend Taiko hardfork activation tables with shared Ethereum hardfork definitions.
fn extend_with_shared_hardforks(
    hardforks: Vec<(Box<dyn Hardfork>, ForkCondition)>,
) -> Vec<(Box<dyn Hardfork>, ForkCondition)> {
    fn fork_id_activation_key(condition: ForkCondition) -> (u8, u64) {
        match condition {
            ForkCondition::Block(block) | ForkCondition::TTD { fork_block: Some(block), .. } => {
                (0, block)
            }
            ForkCondition::Timestamp(timestamp) => (1, timestamp),
            ForkCondition::TTD { fork_block: None, .. } => (2, 0),
            ForkCondition::Never => (3, 0),
        }
    }

    // Determine the Ethereum fork activations implied by Unzen. Taiko executes Unzen with Osaka
    // semantics, and upstream validators still consult the predecessor Cancun/Prague fork flags
    // for some transaction and payload checks. If Unzen is not present, default to
    // `ForkCondition::Never`.
    let unzen_activation = hardforks
        .iter()
        .find_map(|(fork, condition)| {
            (fork.name() == TaikoHardfork::Unzen.name()).then_some(*condition)
        })
        .unwrap_or(ForkCondition::Never);

    let shared_hardforks = [
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
        (EthereumHardfork::Cancun.boxed(), unzen_activation),
        (EthereumHardfork::Prague.boxed(), unzen_activation),
        (EthereumHardfork::Osaka.boxed(), unzen_activation),
    ];

    let mut ordered_hardforks = shared_hardforks.into_iter().chain(hardforks).collect::<Vec<_>>();

    // Match fork-ID activation order: known TTD fork blocks are block activations, not enum-order
    // TTD activations, so Paris block zero must stay before later Taiko block forks.
    ordered_hardforks.sort_by_key(|(_, condition)| fork_id_activation_key(*condition));

    ordered_hardforks
}

#[cfg(test)]
mod test {
    use super::*;

    use alloy_chains::Chain;
    use alloy_genesis::Genesis;
    use alloy_hardforks::Head;
    use reth_chainspec::ChainSpec;

    fn shasta_before_unzen_hardforks() -> Vec<(Box<dyn Hardfork>, ForkCondition)> {
        vec![
            (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(1)),
            (TaikoHardfork::Pacaya.boxed(), ForkCondition::Block(2)),
            (TaikoHardfork::Shasta.boxed(), ForkCondition::Timestamp(100)),
            (TaikoHardfork::Unzen.boxed(), ForkCondition::Timestamp(200)),
        ]
    }

    fn shasta_before_unzen_chain_spec() -> ChainSpec {
        ChainSpec::builder()
            .chain(Chain::mainnet())
            .genesis(Genesis::default())
            .with_forks(ChainHardforks::new(extend_with_shared_hardforks(
                shasta_before_unzen_hardforks(),
            )))
            .build()
    }

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
    fn test_extend_with_shared_hardforks_sets_osaka_from_unzen_activation() {
        let forks = extend_with_shared_hardforks(vec![
            (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(1)),
            (TaikoHardfork::Unzen.boxed(), ForkCondition::Timestamp(123)),
        ]);

        let osaka =
            forks.iter().find(|(fork, _)| fork.name() == "Osaka").map(|(_, condition)| *condition);

        assert_eq!(osaka, Some(ForkCondition::Timestamp(123)));
    }

    #[test]
    fn test_extend_with_shared_hardforks_sets_cancun_and_prague_from_unzen_activation() {
        let forks = extend_with_shared_hardforks(vec![
            (TaikoHardfork::Ontake.boxed(), ForkCondition::Block(1)),
            (TaikoHardfork::Unzen.boxed(), ForkCondition::Timestamp(123)),
        ]);

        let cancun =
            forks.iter().find(|(fork, _)| fork.name() == "Cancun").map(|(_, condition)| *condition);
        let prague =
            forks.iter().find(|(fork, _)| fork.name() == "Prague").map(|(_, condition)| *condition);

        assert_eq!(cancun, Some(ForkCondition::Timestamp(123)));
        assert_eq!(prague, Some(ForkCondition::Timestamp(123)));
    }

    #[test]
    fn test_extend_with_shared_hardforks_orders_future_timestamp_forks() {
        let forks = extend_with_shared_hardforks(shasta_before_unzen_hardforks());
        let future_timestamp_forks = forks
            .iter()
            .filter_map(|(fork, condition)| {
                matches!(condition.as_timestamp(), Some(100 | 200)).then_some(fork.name())
            })
            .collect::<Vec<_>>();

        assert_eq!(
            future_timestamp_forks,
            vec!["Shasta", "Cancun", "Prague", "Osaka", "Unzen"],
            "Shasta must be folded into fork hashes before Unzen-derived Ethereum forks"
        );
    }

    #[test]
    fn test_future_unzen_fork_id_matches_sorted_fork_filter() {
        let chain_spec = shasta_before_unzen_chain_spec();

        for timestamp in [150, 250] {
            let head = Head { number: 2, timestamp, ..Default::default() };
            let advertised = chain_spec.fork_id(&head);
            let sorted_filter = chain_spec.fork_filter(head).current();

            assert_eq!(
                advertised, sorted_filter,
                "advertised fork ID should match the sorted fork-filter ID at timestamp {timestamp}"
            );
        }
    }

    #[test]
    fn test_devnet_shasta_uses_timestamp_activation() {
        let shasta = TAIKO_DEVNET_HARDFORKS.fork(TaikoHardfork::Shasta);
        assert!(shasta.is_timestamp(), "shasta activation should be timestamp-based");
        assert_eq!(shasta, ForkCondition::Timestamp(0));
    }

    #[test]
    fn test_mainnet_shasta_timestamp() {
        let shasta = TAIKO_MAINNET_HARDFORKS.fork(TaikoHardfork::Shasta);
        assert!(shasta.is_timestamp(), "shasta activation should be timestamp-based");
        assert_eq!(shasta, ForkCondition::Timestamp(1_775_135_700));
    }

    #[test]
    fn test_hoodi_shasta_timestamp() {
        let shasta = TAIKO_HOODI_HARDFORKS.fork(TaikoHardfork::Shasta);
        assert!(shasta.is_timestamp(), "shasta activation should be timestamp-based");
        assert_eq!(shasta, ForkCondition::Timestamp(1_770_296_400));
    }

    #[test]
    fn test_hoodi_unzen_timestamp() {
        let unzen = TAIKO_HOODI_HARDFORKS.fork(TaikoHardfork::Unzen);
        assert!(unzen.is_timestamp(), "unzen activation should be timestamp-based");
        assert_eq!(unzen, ForkCondition::Timestamp(1_781_787_600));
    }

    #[test]
    fn test_masaya_shasta_uses_timestamp_activation() {
        let shasta = TAIKO_MASAYA_HARDFORKS.fork(TaikoHardfork::Shasta);
        assert!(shasta.is_timestamp(), "shasta activation should be timestamp-based");
        assert_eq!(shasta, ForkCondition::Timestamp(0));
    }

    #[test]
    fn test_masaya_unzen_activates_at_genesis() {
        let unzen = TAIKO_MASAYA_HARDFORKS.fork(TaikoHardfork::Unzen);
        assert_eq!(
            unzen,
            ForkCondition::Timestamp(0),
            "post-reset Masaya forks into Unzen at genesis"
        );
    }
}
