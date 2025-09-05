use std::sync::Arc;

use reth_ethereum::EthPrimitives;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{BuilderContext, components::ConsensusBuilder};

use crate::{
    chainspec::spec::TaikoChainSpec, consensus::validation::TaikoBeaconConsensus,
    payload::engine::TaikoEngineTypes,
};

/// A basic Taiko consensus builder.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct TaikoConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for TaikoConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
        >,
    >,
{
    /// The consensus implementation to build.
    type Consensus = Arc<TaikoBeaconConsensus<Node::Provider>>;

    /// Creates the TaikoBeaconConsensus implementation.
    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(TaikoBeaconConsensus::new(ctx.chain_spec(), ctx.provider().clone())))
    }
}
