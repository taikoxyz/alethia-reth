#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko node composition and addon wiring built on top of `reth`.
pub use alethia_reth_block as block;
pub use alethia_reth_chainspec as chainspec;
pub use alethia_reth_consensus as consensus;
use alethia_reth_consensus::transaction::TaikoTxEnvelope;
pub use alethia_reth_db as db;
pub use alethia_reth_evm as evm;
pub use alethia_reth_node_builder as node_builder;
pub use alethia_reth_payload as payload;
pub use alethia_reth_primitives as primitives;
use alethia_reth_primitives::{TaikoBlock, TaikoPrimitives};
pub use alethia_reth_rpc as rpc;

/// Taiko transaction pool builder.
pub mod pool;
pub mod txpool;
use block::config::TaikoEvmConfig;
use chainspec::spec::TaikoChainSpec;
use primitives::engine::TaikoEngineTypes;
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_engine_primitives::{EngineApiValidator, PayloadValidator};
use reth_node_api::{
    BlockTy, FullNodeComponents, FullNodeTypes, NodeAddOns, NodeTypes, PayloadAttributesBuilder,
    PayloadTypes,
};
use reth_node_builder::{
    DebugNode, Node, NodeAdapter,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    rpc::{
        BasicEngineValidatorBuilder, EngineValidatorAddOn, PayloadValidatorBuilder, RethRpcAddOns,
        RpcAddOns, RpcHandle, RpcHooks,
    },
};
use reth_provider::EthStorage;
use rpc::{
    converter::TaikoRpcConverter,
    engine::{builder::TaikoEngineApiBuilder, validator::TaikoEngineValidatorBuilder},
    eth::types::TaikoEthApi,
};
use std::sync::Arc;

use alethia_reth_node_builder::{
    TaikoConsensusBuilder, TaikoEthApiBuilder, TaikoExecutorBuilder, TaikoNetworkBuilder,
    TaikoPayloadBuilderBuilder,
};
use pool::TaikoPoolBuilder;

/// Taiko storage type.
/// Since TaikoTxEnvelope implements Compact (for RLP encoding), we can reuse EthStorage
/// with our custom transaction and header types.
pub type TaikoStorage =
    EthStorage<consensus::transaction::TaikoTxEnvelope, alloy_consensus::Header>;

/// The main node type for a Taiko network node, implementing the `NodeTypes` trait.
#[derive(Debug, Clone, Default)]
pub struct TaikoNode;

impl NodeTypes for TaikoNode {
    /// The node's primitive types, defining basic operations and structures.
    type Primitives = TaikoPrimitives;
    /// The type used for configuration of the EVM.
    type ChainSpec = TaikoChainSpec;
    /// The type responsible for writing chain primitives to storage.
    type Storage = TaikoStorage;
    /// The node's engine types, defining the interaction with the consensus engine.
    type Payload = TaikoEngineTypes;
}

/// Taiko custom addons which configuring RPC types.
pub struct TaikoAddOns<N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>, PVB>(
    RpcAddOns<N, TaikoEthApiBuilder, PVB, TaikoEngineApiBuilder<PVB>>,
);

impl<N, PVB> Default for TaikoAddOns<N, PVB>
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    PVB: Default,
{
    /// Creates a new instance of `TaikoAddOns` with default configurations.
    fn default() -> Self {
        let add_ons = RpcAddOns::new(
            TaikoEthApiBuilder,
            PVB::default(),
            TaikoEngineApiBuilder::default(),
            Default::default(),
            Default::default(),
        );

        TaikoAddOns(add_ons)
    }
}

impl<N, PVB> NodeAddOns<N> for TaikoAddOns<N, PVB>
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    PVB: PayloadValidatorBuilder<N> + Clone,
    PVB::Validator: PayloadValidator<<N::Types as NodeTypes>::Payload, Block = BlockTy<N::Types>>
        + EngineApiValidator<<N::Types as NodeTypes>::Payload>,
{
    /// Handle to add-ons.
    type Handle = RpcHandle<N, TaikoEthApi<N, TaikoRpcConverter<<N as FullNodeComponents>::Evm>>>;

    /// Configures and launches the add-ons.
    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        self.0.launch_add_ons(ctx).await
    }
}

impl<N, PVB> RethRpcAddOns<N> for TaikoAddOns<N, PVB>
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    PVB: PayloadValidatorBuilder<N> + Clone,
    PVB::Validator: PayloadValidator<<N::Types as NodeTypes>::Payload, Block = BlockTy<N::Types>>
        + EngineApiValidator<<N::Types as NodeTypes>::Payload>,
{
    /// eth API implementation.
    type EthApi = TaikoEthApi<N, TaikoRpcConverter<<N as FullNodeComponents>::Evm>>;

    /// Returns a mutable reference to RPC hooks.
    fn hooks_mut(&mut self) -> &mut RpcHooks<N, Self::EthApi> {
        self.0.hooks_mut()
    }
}

impl<N, PVB> EngineValidatorAddOn<N> for TaikoAddOns<N, PVB>
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    PVB: PayloadValidatorBuilder<N> + Send,
    PVB::Validator: PayloadValidator<<N::Types as NodeTypes>::Payload, Block = BlockTy<N::Types>>
        + EngineApiValidator<<N::Types as NodeTypes>::Payload>,
{
    /// The ValidatorBuilder type to use for the engine API.
    type ValidatorBuilder = BasicEngineValidatorBuilder<PVB>;

    /// Returns the engine validator builder.
    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        EngineValidatorAddOn::<N>::engine_validator_builder(&self.0)
    }
}

impl<N> Node<N> for TaikoNode
where
    N: FullNodeTypes<Types = Self>,
{
    /// The type that builds the node's components.
    type ComponentsBuilder = ComponentsBuilder<
        N,
        TaikoPoolBuilder,
        BasicPayloadServiceBuilder<TaikoPayloadBuilderBuilder>,
        TaikoNetworkBuilder,
        TaikoExecutorBuilder,
        TaikoConsensusBuilder,
    >;

    /// Exposes the customizable node add-on types.
    type AddOns = TaikoAddOns<NodeAdapter<N>, TaikoEngineValidatorBuilder>;

    /// Returns a [`NodeComponentsBuilder`] for the node.
    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types()
            .pool(TaikoPoolBuilder::default())
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
    type RpcBlock = alloy_rpc_types_eth::Block<TaikoTxEnvelope>;

    /// Converts an RPC block to a primitive block.
    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> TaikoBlock {
        rpc_block.into_consensus()
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
