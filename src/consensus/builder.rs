use std::sync::Arc;

use reth::chainspec::ChainSpec;
use reth_ethereum::EthPrimitives;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{BuilderContext, components::ConsensusBuilder};
use reth_provider::EthStorage;
use reth_trie_db::MerklePatriciaTrie;

use crate::{consensus::validation::TaikoBeaconConsensus, payload::engine::TaikoEngineTypes};

/// A basic Taiko consensus builder.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct TaikoConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for TaikoConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = ChainSpec,
            StateCommitment = MerklePatriciaTrie,
            Storage = EthStorage,
            Payload = TaikoEngineTypes,
        >,
    >,
{
    type Consensus = Arc<TaikoBeaconConsensus>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(TaikoBeaconConsensus::new(ctx.chain_spec())))
    }
}
