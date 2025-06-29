use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{
        Node, NodeAdapter, NodeComponentsBuilder,
        components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    },
    chainspec::ChainSpec,
    providers::EthStorage,
};
use reth_ethereum::EthPrimitives;
use reth_node_ethereum::{
    EthEngineTypes,
    node::{
        EthereumAddOns, EthereumConsensusBuilder, EthereumNetworkBuilder, EthereumPayloadBuilder,
        EthereumPoolBuilder,
    },
};
use reth_trie_db::MerklePatriciaTrie;

use crate::factory::builder::TaikoExecutorBuilder;

pub mod evm;
pub mod factory;

#[derive(Debug, Clone, Default)]
pub struct TaikoNode {}

impl NodeTypes for TaikoNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type StateCommitment = MerklePatriciaTrie;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

impl<N> Node<N> for TaikoNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        TaikoExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns = EthereumAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types()
            .pool(EthereumPoolBuilder::default())
            .executor(TaikoExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }

    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::default()
    }
}
