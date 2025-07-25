use std::fmt::Display;

use alloy_chains::Chain;
use alloy_consensus::Header;
use alloy_eips::eip7840::BlobParams;
use alloy_genesis::Genesis;
use alloy_hardforks::{
    EthereumHardfork, EthereumHardforks, ForkCondition, ForkFilter, ForkId, Hardfork, Head,
};
use alloy_primitives::{Address, B256, U256};
use reth::chainspec::{BaseFeeParams, ChainSpec, DepositContract, EthChainSpec, Hardforks};
use reth_evm::eth::spec::EthExecutorSpec;
use reth_network_peers::NodeRecord;

use crate::chainspec::hardfork::TaikoHardfork;

/// An Taiko chain specification.
///
/// A chain specification describes:
///
/// - Meta-information about the chain (the chain ID)
/// - The genesis block of the chain ([`Genesis`])
/// - What hardforks are activated, and under which conditions
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaikoChainSpec {
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

    /// Get the [`BaseFeeParams`] for the chain at the given block.
    fn base_fee_params_at_block(&self, block_number: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_block(block_number)
    }

    /// Get the [`BaseFeeParams`] for the chain at the given timestamp.
    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_timestamp(timestamp)
    }

    /// Get the [`BlobParams`] for the given timestamp, in Taiko network this is always `None`.
    fn blob_params_at_timestamp(&self, _timestamp: u64) -> Option<BlobParams> {
        None
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

    /// Checks if the `Shasta` hardfork is active at the given block number.
    fn is_shasta_active_at_block(&self, block_number: u64) -> bool {
        self.is_taiko_fork_active_at_block(TaikoHardfork::Shasta, block_number)
    }
}

#[cfg(test)]
mod test {
    use super::*;

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
}
