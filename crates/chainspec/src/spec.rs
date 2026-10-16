//! Taiko chain-spec wrapper types and helper traits.
use std::fmt::Display;

use alloy_chains::Chain;
use alloy_consensus::Header;
use alloy_eips::eip7840::BlobParams;
use alloy_genesis::Genesis;
use alloy_hardforks::{EthereumHardfork, ForkCondition, ForkFilter, ForkId, Hardfork, Head};
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::{BaseFeeParams, ChainSpec, DepositContract, EthChainSpec, Hardforks};
use reth_ethereum_forks::EthereumHardforks;
use reth_evm::eth::spec::EthExecutorSpec;
use reth_network_peers::NodeRecord;

use crate::{TAIKO_DEVNET, hardfork::TaikoHardfork};

/// An Taiko chain specification.
///
/// A chain specification describes:
///
/// - Meta-information about the chain (the chain ID)
/// - The genesis block of the chain ([`Genesis`])
/// - What hardforks are activated, and under which conditions
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaikoChainSpec {
    /// Wrapped `reth` chain specification instance.
    pub inner: ChainSpec,
}

impl From<Genesis> for TaikoChainSpec {
    /// Converts the given [`Genesis`] into a [`TaikoChainSpec`].
    fn from(genesis: Genesis) -> Self {
        let chain_spec = ChainSpec::from(genesis);
        Self { inner: chain_spec }
    }
}

impl Hardforks for TaikoChainSpec {
    /// Retrieves [`ForkCondition`] from `fork`. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.inner.hardforks.fork(fork)
    }

    /// Get an iterator of all hardforks with their respective activation conditions.
    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.inner.hardforks.forks_iter()
    }

    /// Compute the [`ForkId`] for the given [`Head`] following eip-6122 spec
    fn fork_id(&self, head: &Head) -> ForkId {
        self.inner.fork_id(head)
    }

    /// Returns the [`ForkId`] for the last fork.
    ///
    /// NOTE: This returns the latest implemented [`ForkId`]. In many cases this will be the future
    /// [`ForkId`] on given network.
    fn latest_fork_id(&self) -> ForkId {
        self.inner.latest_fork_id()
    }

    /// Creates a [`ForkFilter`] for the block described by [Head].
    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.inner.fork_filter(head)
    }
}

impl EthereumHardforks for TaikoChainSpec {
    /// Retrieves [`ForkCondition`] by an [`EthereumHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.inner.fork(fork)
    }
}

impl EthExecutorSpec for TaikoChainSpec {
    /// Address of deposit contract emitting deposit events.
    ///
    /// In Taiko network, the deposit contract is not used, so this method returns `None`.
    fn deposit_contract_address(&self) -> Option<Address> {
        None
    }
}

impl EthChainSpec for TaikoChainSpec {
    /// The header type of the network.
    type Header = Header;

    /// Returns the [`Chain`] object this spec targets.
    fn chain(&self) -> Chain {
        self.inner.chain
    }

    /// Get the [`BaseFeeParams`] for the chain at the given timestamp.
    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_timestamp(timestamp)
    }

    /// Get the [`BlobParams`] for the given timestamp.
    ///
    /// Taiko inherits the wrapped chain spec's blob fee schedule for any active Cancun-or-later
    /// Ethereum fork, even though Taiko consensus still rejects blob transactions.
    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        self.inner.blob_params_at_timestamp(timestamp)
    }

    /// Returns the [`DepositContract`] for the chain, in Taiko network this is always `None`.
    fn deposit_contract(&self) -> Option<&DepositContract> {
        None
    }

    /// The genesis hash.
    fn genesis_hash(&self) -> B256 {
        self.inner.genesis_hash()
    }

    /// The delete limit for pruner, per run.
    fn prune_delete_limit(&self) -> usize {
        self.inner.prune_delete_limit
    }

    /// Returns a string representation of the hardforks.
    fn display_hardforks(&self) -> Box<dyn Display> {
        Box::new(self.inner.display_hardforks())
    }

    /// The genesis header.
    fn genesis_header(&self) -> &Self::Header {
        self.inner.genesis_header()
    }

    /// The genesis block specification.
    fn genesis(&self) -> &Genesis {
        self.inner.genesis()
    }

    /// The bootnodes for the chain, if any.
    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        self.inner.bootnodes()
    }

    /// In Taiko network, we always mark this value as `true` so that we
    /// we can reorg the chain at will.
    /// ref: https://github.com/paradigmxyz/reth/blob/main/crates/engine/tree/src/tree/mod.rs#L898
    fn is_optimism(&self) -> bool {
        true
    }

    /// Returns the block number at which the Paris hardfork is activated.
    /// In Taiko network, this is always `0`.
    fn final_paris_total_difficulty(&self) -> Option<U256> {
        Some(U256::ZERO)
    }
}

impl TaikoExecutorSpec for TaikoChainSpec {
    /// Retrieves [`ForkCondition`] by an [`TaikoHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn taiko_fork_activation(&self, fork: TaikoHardfork) -> ForkCondition {
        self.inner.hardforks.fork(fork)
    }
}

/// Helper trait for applying Taiko devnet specific overrides.
pub trait TaikoDevnetConfigExt {
    /// Returns a devnet spec activating Etna at `timestamp`, preserving custom allocations.
    ///
    /// All Genesis fields outside `alloc` must match canonical devnet; otherwise returns `None`.
    fn clone_with_devnet_etna_timestamp(&self, timestamp: u64) -> Option<Self>
    where
        Self: Sized;
}

impl TaikoDevnetConfigExt for TaikoChainSpec {
    /// Returns a devnet spec activating Etna at `timestamp`, preserving custom allocations.
    ///
    /// All Genesis fields outside `alloc` must match canonical devnet; otherwise returns `None`.
    ///
    /// Zero activates Etna at genesis. The canonical devnet activates Unzen at genesis, so every
    /// timestamp keeps Etna at or after Unzen.
    fn clone_with_devnet_etna_timestamp(&self, timestamp: u64) -> Option<Self>
    where
        Self: Sized,
    {
        let genesis = self.genesis().clone();
        let mut comparable_genesis = genesis.clone();
        comparable_genesis.alloc = TAIKO_DEVNET.genesis().alloc.clone();
        if comparable_genesis != *TAIKO_DEVNET.genesis() {
            return None
        }

        // A genesis hash does not commit to chain config. Validate above even when the cached
        // hash matches devnet, then restore the complete Taiko/Ethereum table in every case.
        let mut hardforks = TAIKO_DEVNET.inner.hardforks.clone();
        hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(timestamp));
        // Generic Genesis parsing caches a header without devnet's Cancun/Prague fields.
        // Rebuild from the final fork table so the header/hash also commit to the custom alloc.
        let mut inner = ChainSpec::builder()
            .chain(genesis.config.chain_id.into())
            .genesis(genesis)
            .with_forks(hardforks)
            .build();
        inner.paris_block_and_final_difficulty = Some((0, U256::ZERO));
        Some(Self { inner })
    }
}

/// Helper methods for Ethereum forks.
#[auto_impl::auto_impl(&, Arc)]
pub trait TaikoExecutorSpec: EthExecutorSpec {
    /// Retrieves [`ForkCondition`] by an [`TaikoHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn taiko_fork_activation(&self, fork: TaikoHardfork) -> ForkCondition;

    /// Convenience method to check if an [`TaikoHardfork`] is active at a given block number.
    fn is_taiko_fork_active_at_block(&self, fork: TaikoHardfork, block_number: u64) -> bool {
        self.taiko_fork_activation(fork).active_at_block(block_number)
    }

    /// Checks if the `Ontake` hardfork is active at the given block number.
    fn is_ontake_active_at_block(&self, block_number: u64) -> bool {
        self.is_taiko_fork_active_at_block(TaikoHardfork::Ontake, block_number)
    }

    /// Checks if the `Pacaya` hardfork is active at the given block number.
    fn is_pacaya_active_at_block(&self, block_number: u64) -> bool {
        self.is_taiko_fork_active_at_block(TaikoHardfork::Pacaya, block_number)
    }

    /// Checks if the `Shasta` hardfork is active at the given timestamp.
    ///
    /// Taiko chains always run with London enabled from genesis, so the activation reduces to the
    /// timestamp-only condition.
    fn is_shasta_active(&self, timestamp: u64) -> bool {
        self.taiko_fork_activation(TaikoHardfork::Shasta).active_at_timestamp(timestamp)
    }

    /// Checks if the `Unzen` hardfork is active at the given timestamp.
    fn is_unzen_active(&self, timestamp: u64) -> bool {
        self.taiko_fork_activation(TaikoHardfork::Unzen).active_at_timestamp(timestamp)
    }

    /// Checks if the `Etna` hardfork is active at the given timestamp.
    fn is_etna_active(&self, timestamp: u64) -> bool {
        self.taiko_fork_activation(TaikoHardfork::Etna).active_at_timestamp(timestamp)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{TAIKO_DEVNET, TAIKO_MAINNET};
    use alloy_consensus::BlockHeader;

    #[test]
    fn test_chain_spec_is_optimism() {
        let spec = TaikoChainSpec::default();

        assert!(spec.is_optimism());
    }

    #[test]
    fn test_chain_spec_default_none_value() {
        let spec = TaikoChainSpec::default();

        assert_eq!(spec.deposit_contract(), None);
        assert_eq!(spec.blob_params_at_timestamp(0), None);
        assert_eq!(spec.final_paris_total_difficulty(), Some(U256::ZERO));
    }

    #[test]
    fn test_mainnet_blob_params_remain_unset_before_unzen() {
        let spec = TAIKO_MAINNET.as_ref();

        assert_eq!(spec.blob_params_at_timestamp(0), None);
    }

    #[test]
    fn test_devnet_unzen_exposes_osaka_blob_params_and_blob_base_fee() {
        let spec = TAIKO_DEVNET.as_ref();
        let header = Header {
            timestamp: 0,
            base_fee_per_gas: Some(1),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            ..Header::default()
        };

        assert_eq!(spec.blob_params_at_timestamp(0), Some(BlobParams::osaka()));
        assert_eq!(
            header.maybe_next_block_blob_fee(spec.blob_params_at_timestamp(header.timestamp())),
            Some(1)
        );
    }

    #[test]
    fn test_etna_activation_is_timestamp_based() {
        let mut spec = (*TAIKO_DEVNET).as_ref().clone();
        assert!(!crate::hardfork::TaikoHardforks::is_etna_active(&spec, u64::MAX));
        spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(100));
        assert!(!crate::hardfork::TaikoHardforks::is_etna_active(&spec, 99));
        assert!(crate::hardfork::TaikoHardforks::is_etna_active(&spec, 100));
    }

    #[test]
    fn test_clone_with_devnet_etna_timestamp() {
        for timestamp in [0, 100] {
            let overridden = TAIKO_DEVNET
                .clone_with_devnet_etna_timestamp(timestamp)
                .expect("the devnet override should return a clone");
            assert_eq!(
                overridden.taiko_fork_activation(TaikoHardfork::Etna),
                ForkCondition::Timestamp(timestamp)
            );
            assert_eq!(overridden.genesis_hash(), crate::TAIKO_DEVNET_GENESIS_HASH);
            let mut expected = TAIKO_DEVNET.as_ref().clone();
            expected
                .inner
                .hardforks
                .insert(TaikoHardfork::Etna, ForkCondition::Timestamp(timestamp));
            assert_eq!(overridden, expected, "canonical preset metadata must stay unchanged");
            let parsed = TaikoChainSpec::from(
                serde_json::from_str::<Genesis>(include_str!("genesis/devnet.json")).unwrap(),
            );
            assert_eq!(parsed.clone_with_devnet_etna_timestamp(timestamp), Some(expected));
        }
        assert_eq!(
            TAIKO_MAINNET.clone_with_devnet_etna_timestamp(0),
            None,
            "non-devnet chains ignore the devnet override"
        );
    }

    fn alloc_only_devnet_genesis() -> Genesis {
        let mut genesis: Genesis =
            serde_json::from_str(include_str!("genesis/devnet.json")).unwrap();
        let system_contracts: Genesis =
            serde_json::from_str(include_str!("../../node/tests/fixtures/etna-genesis.json"))
                .unwrap();
        for address in [
            alloy_primitives::address!("0000f90827f1c53a10cb7a02335b175320002935"),
            alloy_primitives::address!("000f3df6d732807ef1319fb7b8bb8522d0beac02"),
        ] {
            genesis.alloc.insert(address, system_contracts.alloc[&address].clone());
        }
        genesis.alloc.insert(
            alloy_primitives::address!("1670010000000000000000000000000000010001"),
            alloy_genesis::GenesisAccount {
                nonce: Some(1),
                balance: U256::from(42),
                code: Some(alloy_primitives::bytes!("60015f5500")),
                storage: Some([(B256::ZERO, B256::with_last_byte(1))].into()),
                ..Default::default()
            },
        );
        genesis
    }

    #[test]
    fn alloc_only_devnet_override_rebuilds_the_parsed_genesis_header() {
        let genesis = alloc_only_devnet_genesis();
        let parsed = TaikoChainSpec::from(genesis.clone());
        // The JSON parser cached a pre-Cancun/Prague header. Replacing forks cannot fix it.
        assert_eq!(parsed.genesis_header().parent_beacon_block_root, None);
        assert_eq!(parsed.genesis_header().requests_hash, None);
        assert_ne!(parsed.genesis_header().state_root, TAIKO_DEVNET.genesis_header().state_root);
        assert_ne!(parsed.genesis_hash(), TAIKO_DEVNET.genesis_hash());

        for timestamp in [0, 100] {
            let overridden = parsed
                .clone_with_devnet_etna_timestamp(timestamp)
                .expect("alloc-only devnet JSON must accept the explicit override");
            assert_eq!(overridden.genesis(), &genesis);
            let expected_header = Header {
                state_root: parsed.genesis_header().state_root,
                ..TAIKO_DEVNET.genesis_header().clone()
            };
            assert_eq!(overridden.genesis_header(), &expected_header);
            assert_eq!(overridden.genesis_hash(), expected_header.hash_slow());
            assert_ne!(overridden.genesis_hash(), parsed.genesis_hash());
            assert_ne!(overridden.genesis_hash(), TAIKO_DEVNET.genesis_hash());
            assert_eq!(overridden.genesis_header().base_fee_per_gas, Some(10_000_000));
            assert_eq!(overridden.genesis_header().blob_gas_used, Some(0));
            assert_eq!(overridden.genesis_header().excess_blob_gas, Some(0));
            assert_eq!(overridden.genesis_header().parent_beacon_block_root, Some(B256::ZERO));
            assert_eq!(
                overridden.genesis_header().requests_hash,
                Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH)
            );
            assert_eq!(overridden.inner.paris_block_and_final_difficulty, Some((0, U256::ZERO)));
            assert_eq!(overridden.blob_params_at_timestamp(0), Some(BlobParams::osaka()));
            let mut expected_forks = TAIKO_DEVNET.inner.hardforks.clone();
            expected_forks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(timestamp));
            assert_eq!(overridden.inner.hardforks, expected_forks);
            if timestamp > 0 {
                assert!(!overridden.is_etna_active(timestamp - 1));
            }
            assert!(overridden.is_etna_active(timestamp));
        }
        assert!(!parsed.is_etna_active(u64::MAX));
    }

    #[test]
    fn devnet_override_rejects_every_non_alloc_genesis_change() {
        let changes: &[(&str, fn(&mut Genesis))] = &[
            ("chainId", |g| g.config.chain_id += 1),
            ("fork config", |g| g.config.shanghai_time = Some(1)),
            ("extra field change", |g| {
                g.config.extra_fields.insert("shastaTime".into(), 0.into());
            }),
            ("extra field addition", |g| {
                g.config.extra_fields.insert("etnaTime".into(), 0.into());
            }),
            ("extra field removal", |g| {
                g.config.extra_fields.remove("ChainName");
            }),
            ("nonce", |g| g.nonce += 1),
            ("timestamp", |g| g.timestamp += 1),
            ("extraData", |g| g.extra_data = alloy_primitives::bytes!("01")),
            ("gasLimit", |g| g.gas_limit += 1),
            ("difficulty", |g| g.difficulty += U256::from(1)),
            ("mixHash", |g| g.mix_hash = B256::with_last_byte(1)),
            ("coinbase", |g| g.coinbase = Address::with_last_byte(1)),
            ("baseFeePerGas", |g| g.base_fee_per_gas = Some(1)),
            ("excessBlobGas", |g| g.excess_blob_gas = Some(0)),
            ("blobGasUsed", |g| g.blob_gas_used = Some(0)),
            ("number", |g| g.number = Some(0)),
            ("parentHash", |g| g.parent_hash = Some(B256::ZERO)),
        ];
        for (name, change) in changes {
            let mut genesis = alloc_only_devnet_genesis();
            change(&mut genesis);
            assert!(
                TaikoChainSpec::from(genesis).clone_with_devnet_etna_timestamp(0).is_none(),
                "must reject changed {name}"
            );
        }
        assert!(crate::TAIKO_HOODI.clone_with_devnet_etna_timestamp(0).is_none());
    }

    #[test]
    fn canonical_hash_does_not_bypass_genesis_config_validation() {
        // Chain config is not a header commitment: the cached hash still matches devnet.
        for change in
            [(|g: &mut Genesis| g.config.chain_id += 1) as fn(&mut Genesis), |g: &mut Genesis| {
                g.config.extra_fields.insert("etnaTime".into(), 0.into());
            }]
        {
            let mut spec = TAIKO_DEVNET.as_ref().clone();
            change(&mut spec.inner.genesis);
            assert_eq!(spec.genesis_hash(), crate::TAIKO_DEVNET_GENESIS_HASH);
            assert!(spec.clone_with_devnet_etna_timestamp(0).is_none());
        }
    }

    #[test]
    fn devnet_override_restores_forks_even_with_a_canonical_cached_hash() {
        let mut parsed = TaikoChainSpec::from(TAIKO_DEVNET.genesis().clone());
        parsed.inner.genesis_header = TAIKO_DEVNET.inner.genesis_header.clone();
        assert!(!parsed.is_unzen_active(0));
        assert_eq!(parsed.genesis_hash(), crate::TAIKO_DEVNET_GENESIS_HASH);
        let overridden = parsed.clone_with_devnet_etna_timestamp(0).unwrap();
        assert!(overridden.is_unzen_active(0));
        assert!(overridden.is_shasta_active(0));
        assert_eq!(overridden.genesis_hash(), crate::TAIKO_DEVNET_GENESIS_HASH);
    }

    #[test]
    fn built_in_and_overridden_chains_have_a_latest_fork_id() {
        // reth's `latest_fork_id` unwraps the last fork's ID, so a trailing `Never` row panics.
        for spec in [&*TAIKO_MAINNET, &*crate::TAIKO_HOODI, &*TAIKO_DEVNET] {
            spec.inner.latest_fork_id();
        }
        TAIKO_DEVNET.clone_with_devnet_etna_timestamp(0).unwrap().inner.latest_fork_id();
    }
}
