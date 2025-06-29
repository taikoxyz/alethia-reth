use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeTypes},
    builder::{
        DebugNode, Node,
        components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    },
    chainspec::ChainSpec,
    providers::EthStorage,
};
use reth_ethereum::EthPrimitives;
use reth_node_ethereum::node::{
    EthereumConsensusBuilder, EthereumNetworkBuilder, EthereumPoolBuilder,
};
use reth_trie_db::MerklePatriciaTrie;

use crate::{
    factory::builder::TaikoExecutorBuilder,
    payload::{TaikoPayloadBuilderBuilder, payload::TaikoPayloadTypes},
};

pub mod chainspec;
pub mod evm;
pub mod factory;
pub mod payload;

#[derive(Debug, Clone, Default)]
pub struct TaikoNode {}

impl NodeTypes for TaikoNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type StateCommitment = MerklePatriciaTrie;
    type Storage = EthStorage;
    type Payload = TaikoPayloadTypes;
}

impl<N> Node<N> for TaikoNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<TaikoPayloadBuilderBuilder>,
        EthereumNetworkBuilder,
        TaikoExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns = ();

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types()
            .pool(EthereumPoolBuilder::default())
            .executor(TaikoExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::new(
                TaikoPayloadBuilderBuilder::default(),
            ))
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }

    fn add_ons(&self) -> Self::AddOns {}
}

impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for TaikoNode {
    type RpcBlock = alloy_rpc_types_eth::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        rpc_block.into_consensus().convert_transactions()
    }
}
