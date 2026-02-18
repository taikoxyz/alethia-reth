use std::{fmt::Debug, sync::Arc};

use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_consensus::validation::{TaikoBeaconConsensus, TaikoBlockReader};
use alethia_reth_primitives::{TaikoPrimitives, engine::TaikoEngineTypes};
use alloy_primitives::B256;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{BuilderContext, components::ConsensusBuilder};
use reth_primitives_traits::{AlloyBlockHeader, Block};
use reth_provider::BlockReader;

/// Adapter that exposes a `reth_provider::BlockReader` as a Taiko block reader.
#[derive(Debug)]
pub struct ProviderTaikoBlockReader<T>(pub T);

impl<T> TaikoBlockReader for ProviderTaikoBlockReader<T>
where
    T: BlockReader + Debug + Send + Sync,
    T::Block: Block,
{
    fn block_timestamp_by_hash(&self, hash: B256) -> Option<u64> {
        self.0.block_by_hash(hash).ok().flatten().map(|block| block.header().timestamp())
    }
}

/// A basic Taiko consensus builder.
#[derive(Debug, Default, Clone)]
pub struct TaikoConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for TaikoConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Primitives = TaikoPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
    >,
{
    /// The consensus implementation to build.
    type Consensus = Arc<TaikoBeaconConsensus>;

    /// Creates the TaikoBeaconConsensus implementation.
    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        let block_reader = Arc::new(ProviderTaikoBlockReader(ctx.provider().clone()));
        Ok(Arc::new(TaikoBeaconConsensus::new(ctx.chain_spec(), block_reader)))
    }
}
