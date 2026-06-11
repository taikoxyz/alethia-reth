#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko node composition and addon wiring built on top of `reth`.
pub mod components;
pub mod proof_history;

pub use alethia_reth_block as block;
pub use alethia_reth_chainspec as chainspec;
pub use alethia_reth_consensus as consensus;
pub use alethia_reth_db as db;
pub use alethia_reth_evm as evm;
pub use alethia_reth_payload as payload;
pub use alethia_reth_primitives as primitives;
pub use alethia_reth_rpc as rpc;

use chainspec::spec::TaikoChainSpec;
use components::{TaikoConsensusBuilder, TaikoExecutorBuilder, TaikoNetworkBuilder};
use payload::TaikoPayloadBuilderBuilder;
use primitives::engine::TaikoEngineTypes;
use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeTypes},
    builder::{
        DebugNode, Node,
        components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    },
    providers::EthStorage,
};
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_ethereum::EthPrimitives;
use reth_node_api::{PayloadAttributesBuilder, PayloadTypes};
use reth_node_builder::{NodeAdapter, rpc::RpcAddOns};
use reth_node_ethereum::node::EthereumPoolBuilder;
use rpc::{
    engine::{builder::TaikoEngineApiBuilder, validator::TaikoEngineValidatorBuilder},
    eth::builder::TaikoEthApiBuilder,
};
use std::sync::Arc;

/// The main node type for a Taiko network node, implementing the `NodeTypes` trait.
#[derive(Debug, Clone, Default)]
pub struct TaikoNode;

impl NodeTypes for TaikoNode {
    /// The node's primitive types, defining basic operations and structures.
    type Primitives = EthPrimitives;
    /// The type used for configuration of the EVM.
    type ChainSpec = TaikoChainSpec;
    /// The type responsible for writing chain primitives to storage.
    type Storage = EthStorage;
    /// The node's engine types, defining the interaction with the consensus engine.
    type Payload = TaikoEngineTypes;
}

/// Taiko node add-ons: Reth's [`RpcAddOns`] wired with the Taiko eth and engine API builders.
pub type TaikoAddOns<N> = RpcAddOns<
    N,
    TaikoEthApiBuilder,
    TaikoEngineValidatorBuilder,
    TaikoEngineApiBuilder<TaikoEngineValidatorBuilder>,
    TaikoEngineValidatorBuilder,
>;

impl<N> Node<N> for TaikoNode
where
    N: FullNodeTypes<Types = Self>,
{
    /// The type that builds the node's components.
    type ComponentsBuilder = ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<TaikoPayloadBuilderBuilder>,
        TaikoNetworkBuilder,
        TaikoExecutorBuilder,
        TaikoConsensusBuilder,
    >;

    /// Exposes the customizable node add-on types.
    type AddOns = TaikoAddOns<NodeAdapter<N>>;

    /// Returns a [`NodeComponentsBuilder`] for the node.
    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types()
            .pool(EthereumPoolBuilder::default())
            .executor(TaikoExecutorBuilder)
            .payload(BasicPayloadServiceBuilder::new(TaikoPayloadBuilderBuilder))
            .network(TaikoNetworkBuilder)
            .consensus(TaikoConsensusBuilder)
    }

    /// Returns the node add-ons.
    fn add_ons(&self) -> Self::AddOns {
        TaikoAddOns::default()
    }
}

impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for TaikoNode {
    /// RPC block type. Used by [`DebugConsensusClient`] to fetch blocks and submit them to the
    /// engine.
    type RpcBlock = alloy_rpc_types_eth::Block;

    /// Converts an RPC block to a primitive block.
    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        rpc_block.into_consensus().convert_transactions()
    }

    /// Creates a payload attributes builder for local mining in dev mode.
    ///
    ///  It will be used by the `LocalMiner` when dev mode is enabled.
    ///
    /// The builder is responsible for creating the payload attributes that define how blocks should
    /// be constructed during local mining.
    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<
        <<Self as reth_node_api::NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
    > {
        LocalPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taiko_node_implements_expected_core_traits() {
        fn assert_traits<T: Clone + Default + core::fmt::Debug>() {}
        assert_traits::<TaikoNode>();
    }
}
