use alloy_chains::Chain;
use alloy_consensus::Header;
use alloy_eips::eip7840::BlobParams;
use alloy_genesis::Genesis;
use derive_more::{Constructor, Deref, Into};
use reth::{
    chainspec::{
        BaseFeeParams, ChainHardforks, ChainSpec, DepositContract, DisplayHardforks, EthChainSpec,
        EthereumHardfork, EthereumHardforks, ForkCondition, ForkFilter, ForkId, Hardfork,
        Hardforks, Head, make_genesis_header,
    },
    primitives::SealedHeader,
    revm::primitives::{B256, U256},
};
use reth_network_peers::NodeRecord;

use crate::chainspec::{TAIKO_MAINNET, hardfork::TaikoHardfork};

/// Taiko chain spec type.
#[derive(Debug, Clone, Deref, Into, Constructor, PartialEq, Eq)]
pub struct TaikoChainSpec {
    /// [`ChainSpec`].
    pub inner: ChainSpec,
}

impl TaikoChainSpec {
    /// Converts the given [`Genesis`] into a [`TaikoChainSpec`].
    pub fn from_genesis(genesis: Genesis) -> Self {
        genesis.into()
    }
}

impl EthChainSpec for TaikoChainSpec {
    type Header = Header;

    fn chain(&self) -> Chain {
        self.inner.chain()
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
        self.inner.deposit_contract()
    }

    fn genesis_hash(&self) -> B256 {
        self.inner.genesis_hash()
    }

    fn prune_delete_limit(&self) -> usize {
        self.inner.prune_delete_limit()
    }

    fn display_hardforks(&self) -> Box<dyn core::fmt::Display> {
        // filter only Taiko hardforks
        let taiko_forks = self.inner.hardforks.forks_iter().filter(|(fork, _)| {
            !EthereumHardfork::VARIANTS
                .iter()
                .any(|h| h.name() == (*fork).name())
        });

        Box::new(DisplayHardforks::new(taiko_forks))
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
        self.inner.final_paris_total_difficulty()
    }
}

impl Hardforks for TaikoChainSpec {
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.inner.fork(fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.inner.forks_iter()
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
        self.fork(fork)
    }
}

impl From<Genesis> for TaikoChainSpec {
    fn from(genesis: Genesis) -> Self {
        let taiko_hardforks = TaikoHardfork::taiko_mainnet();

        // Block-based hardforks
        let hardfork_opts = [
            (EthereumHardfork::Frontier.boxed(), Some(0)),
            (
                EthereumHardfork::Homestead.boxed(),
                genesis.config.homestead_block,
            ),
            (
                EthereumHardfork::Tangerine.boxed(),
                genesis.config.eip150_block,
            ),
            (
                EthereumHardfork::SpuriousDragon.boxed(),
                genesis.config.eip155_block,
            ),
            (
                EthereumHardfork::Byzantium.boxed(),
                genesis.config.byzantium_block,
            ),
            (
                EthereumHardfork::Constantinople.boxed(),
                genesis.config.constantinople_block,
            ),
            (
                EthereumHardfork::Petersburg.boxed(),
                genesis.config.petersburg_block,
            ),
            (
                EthereumHardfork::Istanbul.boxed(),
                genesis.config.istanbul_block,
            ),
            (
                EthereumHardfork::MuirGlacier.boxed(),
                genesis.config.muir_glacier_block,
            ),
            (
                EthereumHardfork::Berlin.boxed(),
                genesis.config.berlin_block,
            ),
            (
                EthereumHardfork::London.boxed(),
                genesis.config.london_block,
            ),
            (
                EthereumHardfork::ArrowGlacier.boxed(),
                genesis.config.arrow_glacier_block,
            ),
            (
                EthereumHardfork::GrayGlacier.boxed(),
                genesis.config.gray_glacier_block,
            ),
            (
                TaikoHardfork::Ontake.boxed(),
                taiko_hardforks[0].1.block_number(),
            ),
            (
                TaikoHardfork::Pacaya.boxed(),
                taiko_hardforks[1].1.block_number(),
            ),
        ];
        let mut block_hardforks = hardfork_opts
            .into_iter()
            .filter_map(|(hardfork, opt)| opt.map(|block| (hardfork, ForkCondition::Block(block))))
            .collect::<Vec<_>>();

        // We set the paris hardfork for Taiko networks to zero
        block_hardforks.push((
            EthereumHardfork::Paris.boxed(),
            ForkCondition::TTD {
                activation_block_number: 0,
                total_difficulty: U256::ZERO,
                fork_block: genesis.config.merge_netsplit_block,
            },
        ));

        // Ordered Hardforks
        let mainnet_hardforks = TAIKO_MAINNET.clone();
        let mainnet_order = mainnet_hardforks.forks_iter();

        let mut ordered_hardforks = Vec::with_capacity(block_hardforks.len());
        for (hardfork, _) in mainnet_order {
            if let Some(pos) = block_hardforks.iter().position(|(e, _)| **e == *hardfork) {
                ordered_hardforks.push(block_hardforks.remove(pos));
            }
        }

        // Append the remaining unknown hardforks to ensure we don't filter any out
        ordered_hardforks.append(&mut block_hardforks);

        let hardforks = ChainHardforks::new(ordered_hardforks);
        let genesis_header = SealedHeader::seal_slow(make_genesis_header(&genesis, &hardforks));

        Self {
            inner: ChainSpec {
                chain: genesis.config.chain_id.into(),
                genesis_header,
                genesis,
                hardforks,
                // We assume no Taiko network merges, and set the paris block and total difficulty to
                // zero
                paris_block_and_final_difficulty: Some((0, U256::ZERO)),
                ..Default::default()
            },
        }
    }
}
