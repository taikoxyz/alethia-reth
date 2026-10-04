//! Taiko chain-spec wrapper types and helper traits.
use std::fmt::Display;

use alloy_chains::Chain;
use alloy_consensus::Header;
use alloy_eips::eip7840::BlobParams;
use alloy_genesis::Genesis;
use alloy_hardforks::{EthereumHardfork, ForkCondition, ForkFilter, ForkId, Hardfork, Head};
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::{
    BaseFeeParams, ChainSpec, DepositContract, EthChainSpec, Hardforks, make_genesis_header,
};
use reth_ethereum_forks::EthereumHardforks;
use reth_evm::eth::spec::EthExecutorSpec;
use reth_network_peers::NodeRecord;
use reth_primitives_traits::SealedHeader;

use crate::{
    TAIKO_DEVNET_GENESIS_HASH, TAIKO_DEVNET_GENESIS_HASH_SHANGHAI, hardfork::TaikoHardfork,
};

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

/// Error returned when the configured Etna activation cannot follow Unzen safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtnaForkOrderError {
    /// Etna is enabled but the chain does not register an Unzen activation.
    MissingUnzen,
    /// One of the ordered forks uses an activation condition other than a timestamp.
    UnsupportedActivationCondition {
        /// Fork whose activation condition cannot participate in timestamp ordering.
        fork: TaikoHardfork,
        /// Unsupported condition configured for the fork.
        condition: ForkCondition,
    },
    /// Etna activates before Unzen, which would regress the execution rule ordering.
    EtnaPrecedesUnzen {
        /// Configured Unzen activation timestamp.
        unzen_timestamp: u64,
        /// Configured Etna activation timestamp.
        etna_timestamp: u64,
    },
}

impl Display for EtnaForkOrderError {
    /// Formats a diagnostic describing the invalid Etna fork ordering.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingUnzen => write!(f, "Etna activation requires an Unzen activation"),
            Self::UnsupportedActivationCondition { fork, condition } => {
                write!(f, "{} uses unsupported activation condition {condition:?}", fork.name())
            }
            Self::EtnaPrecedesUnzen { unzen_timestamp, etna_timestamp } => write!(
                f,
                "Etna timestamp {etna_timestamp} precedes Unzen timestamp {unzen_timestamp}"
            ),
        }
    }
}

impl std::error::Error for EtnaForkOrderError {}

impl TaikoChainSpec {
    /// Validates that an enabled Etna timestamp is ordered at or after Unzen.
    ///
    /// A missing or disabled Etna entry is valid. When Etna is enabled, both forks must use
    /// timestamp activation and Unzen must be explicitly registered.
    pub fn validate_etna_fork_order(&self) -> Result<(), EtnaForkOrderError> {
        let hardforks = &self.inner.hardforks;
        let etna_timestamp = match hardforks.fork(TaikoHardfork::Etna) {
            ForkCondition::Never => return Ok(()),
            ForkCondition::Timestamp(timestamp) => timestamp,
            condition => {
                return Err(EtnaForkOrderError::UnsupportedActivationCondition {
                    fork: TaikoHardfork::Etna,
                    condition,
                })
            }
        };

        let unzen_condition =
            hardforks.get(TaikoHardfork::Unzen).ok_or(EtnaForkOrderError::MissingUnzen)?;
        let unzen_timestamp = match unzen_condition {
            ForkCondition::Timestamp(timestamp) => timestamp,
            condition => {
                return Err(EtnaForkOrderError::UnsupportedActivationCondition {
                    fork: TaikoHardfork::Unzen,
                    condition,
                })
            }
        };

        if etna_timestamp < unzen_timestamp {
            return Err(EtnaForkOrderError::EtnaPrecedesUnzen { unzen_timestamp, etna_timestamp })
        }

        Ok(())
    }
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
    /// Returns a cloned [`TaikoChainSpec`] with the Unzen hardfork activation timestamp updated
    /// when the chainspec targets the Taiko devnet. Returns `None` for other networks.
    fn clone_with_devnet_unzen_timestamp(&self, timestamp: u64) -> Option<Self>
    where
        Self: Sized;

    /// Returns a cloned devnet chain spec with the requested Unzen and optional Etna timestamps.
    ///
    /// Non-devnet specs retain the no-op convention and are validated without being cloned.
    fn clone_with_devnet_fork_timestamps(
        &self,
        unzen_timestamp: u64,
        etna_timestamp: Option<u64>,
    ) -> Result<Option<Self>, EtnaForkOrderError>
    where
        Self: Sized;
}

impl TaikoDevnetConfigExt for TaikoChainSpec {
    /// Returns a cloned [`TaikoChainSpec`] with the Unzen hardfork activation timestamp updated
    /// when the chainspec targets the Taiko devnet. Returns `None` for other networks or when
    /// `timestamp == 0` (the default devnet config already activates Unzen at genesis).
    fn clone_with_devnet_unzen_timestamp(&self, timestamp: u64) -> Option<Self>
    where
        Self: Sized,
    {
        if timestamp == 0 || self.genesis_hash() != TAIKO_DEVNET_GENESIS_HASH {
            return None;
        }

        let mut cloned = self.clone();
        cloned.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(timestamp));
        cloned
            .inner
            .hardforks
            .insert(EthereumHardfork::Cancun, ForkCondition::Timestamp(timestamp));
        cloned
            .inner
            .hardforks
            .insert(EthereumHardfork::Prague, ForkCondition::Timestamp(timestamp));
        cloned.inner.hardforks.insert(EthereumHardfork::Osaka, ForkCondition::Timestamp(timestamp));

        // Pushing Osaka past genesis changes the genesis header schema (no blob/requests fields),
        // so the cached `genesis_header` must be regenerated from the updated hardforks.
        cloned.inner.genesis_header = SealedHeader::seal_slow(make_genesis_header(
            &cloned.inner.genesis,
            &cloned.inner.hardforks,
        ));
        assert_eq!(
            cloned.genesis_hash(),
            TAIKO_DEVNET_GENESIS_HASH_SHANGHAI,
            "unexpected Taiko devnet genesis hash after Unzen timestamp override",
        );

        Some(cloned)
    }

    /// Returns a cloned canonical devnet spec with validated Unzen and Etna timestamp overrides.
    ///
    /// The optional Etna value preserves the distinction between an omitted override and an
    /// explicit genesis activation at timestamp zero.
    fn clone_with_devnet_fork_timestamps(
        &self,
        unzen_timestamp: u64,
        etna_timestamp: Option<u64>,
    ) -> Result<Option<Self>, EtnaForkOrderError>
    where
        Self: Sized,
    {
        if self.genesis_hash() != TAIKO_DEVNET_GENESIS_HASH {
            self.validate_etna_fork_order()?;
            return Ok(None)
        }

        let unzen_overridden = unzen_timestamp != 0;
        let mut cloned =
            self.clone_with_devnet_unzen_timestamp(unzen_timestamp).unwrap_or_else(|| self.clone());
        if let Some(timestamp) = etna_timestamp {
            cloned.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(timestamp));
        }
        cloned.validate_etna_fork_order()?;

        Ok((unzen_overridden || etna_timestamp.is_some()).then_some(cloned))
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
    fn test_clone_with_devnet_unzen_timestamp() {
        let devnet_spec = (*TAIKO_DEVNET).clone();
        let overridden = devnet_spec
            .as_ref()
            .clone_with_devnet_unzen_timestamp(42)
            .expect("devnet override should succeed");
        assert_eq!(
            overridden.taiko_fork_activation(TaikoHardfork::Unzen),
            ForkCondition::Timestamp(42)
        );
        assert_eq!(
            overridden.ethereum_fork_activation(EthereumHardfork::Cancun),
            ForkCondition::Timestamp(42)
        );
        assert_eq!(
            overridden.ethereum_fork_activation(EthereumHardfork::Prague),
            ForkCondition::Timestamp(42)
        );
        assert_eq!(
            overridden.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(42)
        );
        assert_eq!(overridden.genesis_hash(), crate::TAIKO_DEVNET_GENESIS_HASH_SHANGHAI);
        assert_eq!(
            overridden.genesis_header().withdrawals_root,
            Some(alloy_consensus::EMPTY_ROOT_HASH)
        );
        assert_eq!(overridden.genesis_header().blob_gas_used, None);
        assert_eq!(overridden.genesis_header().requests_hash, None);

        assert!(
            devnet_spec.as_ref().clone_with_devnet_unzen_timestamp(0).is_none(),
            "zero timestamp should skip the override"
        );

        let mainnet_spec = (*TAIKO_MAINNET).clone();
        assert!(
            mainnet_spec.as_ref().clone_with_devnet_unzen_timestamp(1).is_none(),
            "non-devnet overrides should be ignored"
        );
    }

    #[test]
    fn test_etna_activation_and_fork_order() {
        let mut spec = (*TAIKO_DEVNET).as_ref().clone();
        assert!(!crate::hardfork::TaikoHardforks::is_etna_active(&spec, u64::MAX));
        spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(100));
        assert!(!crate::hardfork::TaikoHardforks::is_etna_active(&spec, 99));
        assert!(crate::hardfork::TaikoHardforks::is_etna_active(&spec, 100));
        assert!(spec.validate_etna_fork_order().is_ok());
        spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(101));
        assert!(spec.validate_etna_fork_order().is_err());
    }

    #[test]
    fn test_etna_fork_order_rejects_missing_unzen() {
        let mut spec = TaikoChainSpec::default();
        spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(100));

        assert_eq!(spec.validate_etna_fork_order(), Err(EtnaForkOrderError::MissingUnzen));
    }

    #[test]
    fn test_etna_fork_order_rejects_unsupported_activation_condition() {
        let mut spec = (*TAIKO_DEVNET).as_ref().clone();
        spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Block(100));

        assert!(matches!(
            spec.validate_etna_fork_order(),
            Err(EtnaForkOrderError::UnsupportedActivationCondition { .. })
        ));
    }

    #[test]
    fn test_clone_with_devnet_fork_timestamps() {
        let devnet = TAIKO_DEVNET.as_ref();

        let same_timestamp = devnet
            .clone_with_devnet_fork_timestamps(100, Some(100))
            .expect("matching timestamps should be valid")
            .expect("an override should return a clone");
        assert_eq!(
            same_timestamp.taiko_fork_activation(TaikoHardfork::Unzen),
            ForkCondition::Timestamp(100)
        );
        assert_eq!(
            same_timestamp.taiko_fork_activation(TaikoHardfork::Etna),
            ForkCondition::Timestamp(100)
        );
        assert_eq!(same_timestamp.genesis_hash(), crate::TAIKO_DEVNET_GENESIS_HASH_SHANGHAI);

        assert!(matches!(
            devnet.clone_with_devnet_fork_timestamps(100, Some(99)),
            Err(EtnaForkOrderError::EtnaPrecedesUnzen { .. })
        ));

        let unzen_only = devnet
            .clone_with_devnet_fork_timestamps(100, None)
            .expect("an omitted Etna override should be valid")
            .expect("the Unzen override should return a clone");
        assert_eq!(unzen_only.taiko_fork_activation(TaikoHardfork::Etna), ForkCondition::Never);
        assert_eq!(unzen_only.genesis_hash(), crate::TAIKO_DEVNET_GENESIS_HASH_SHANGHAI);

        let genesis = devnet
            .clone_with_devnet_fork_timestamps(0, Some(0))
            .expect("genesis timestamps should be valid")
            .expect("an explicit Etna timestamp should return a clone");
        assert_eq!(genesis.taiko_fork_activation(TaikoHardfork::Etna), ForkCondition::Timestamp(0));
        assert_eq!(genesis.genesis_hash(), crate::TAIKO_DEVNET_GENESIS_HASH);
    }

    #[test]
    fn test_combined_override_validates_non_devnet_fork_order() {
        let mut custom = (*TAIKO_MAINNET).as_ref().clone();
        custom.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(1));

        assert!(matches!(
            custom.clone_with_devnet_fork_timestamps(0, None),
            Err(EtnaForkOrderError::EtnaPrecedesUnzen { .. })
        ));
    }
}
