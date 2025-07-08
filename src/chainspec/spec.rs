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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaikoChainSpec {
    pub inner: ChainSpec,
}

impl Default for TaikoChainSpec {
    fn default() -> Self {
        Self {
            inner: ChainSpec::default(),
        }
    }
}

impl From<Genesis> for TaikoChainSpec {
    fn from(genesis: Genesis) -> Self {
        let chain_spec = ChainSpec::from(genesis);
        Self { inner: chain_spec }
    }
}

impl Hardforks for TaikoChainSpec {
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.inner.hardforks.fork(fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.inner.hardforks.forks_iter()
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        self.inner.fork_id(head)
    }

    fn latest_fork_id(&self) -> ForkId {
        self.inner.latest_fork_id()
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.inner.fork_filter(head)
    }
}

impl EthereumHardforks for TaikoChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.inner.fork(fork)
    }
}

impl EthExecutorSpec for TaikoChainSpec {
    fn deposit_contract_address(&self) -> Option<Address> {
        self.inner
            .deposit_contract
            .map(|deposit_contract| deposit_contract.address)
    }
}

impl EthChainSpec for TaikoChainSpec {
    type Header = Header;

    fn chain(&self) -> Chain {
        self.inner.chain
    }

    fn base_fee_params_at_block(&self, block_number: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_block(block_number)
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_timestamp(timestamp)
    }

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        self.inner.blob_params_at_timestamp(timestamp)
    }

    fn deposit_contract(&self) -> Option<&DepositContract> {
        self.inner.deposit_contract.as_ref()
    }

    fn genesis_hash(&self) -> B256 {
        self.inner.genesis_hash()
    }

    fn prune_delete_limit(&self) -> usize {
        self.inner.prune_delete_limit
    }

    fn display_hardforks(&self) -> Box<dyn Display> {
        Box::new(self.inner.display_hardforks())
    }

    fn genesis_header(&self) -> &Self::Header {
        self.inner.genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.inner.genesis()
    }

    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        self.inner.bootnodes()
    }

    fn is_optimism(&self) -> bool {
        true
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        Some(U256::ZERO)
    }
}
