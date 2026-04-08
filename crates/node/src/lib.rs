#![cfg_attr(not(test), deny(missing_docs, clippy::missing_docs_in_private_items))]
#![cfg_attr(test, allow(missing_docs, clippy::missing_docs_in_private_items))]
//! Taiko node composition and addon wiring built on top of `reth`.
pub use alethia_reth_block as block;
pub use alethia_reth_chainspec as chainspec;
pub use alethia_reth_consensus as consensus;
pub use alethia_reth_db as db;
pub use alethia_reth_evm as evm;
pub use alethia_reth_network as network;
pub use alethia_reth_node_builder as node_builder;
pub use alethia_reth_payload as payload;
pub use alethia_reth_primitives as primitives;
pub use alethia_reth_rpc as rpc;

use block::config::TaikoEvmConfig;
use chainspec::spec::TaikoChainSpec;
use network::TaikoNetworkBuilder;
use node_builder::{TaikoConsensusBuilder, TaikoExecutorBuilder};
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
use reth_engine_primitives::{EngineApiValidator, PayloadValidator};
use reth_ethereum::EthPrimitives;
use reth_node_api::{BlockTy, NodeAddOns, PayloadAttributesBuilder, PayloadTypes};
use reth_node_builder::{
    NodeAdapter,
    rpc::{
        EngineApiBuilder, EngineValidatorAddOn, EngineValidatorBuilder, Identity,
        PayloadValidatorBuilder, RethRpcAddOns, RethRpcMiddleware, RpcAddOns, RpcHandle, RpcHooks,
    },
};
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

/// Taiko custom addons which configure RPC types.
#[derive(Debug)]
pub struct TaikoAddOns<
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    EthB: reth_node_builder::rpc::EthApiBuilder<N> = TaikoEthApiBuilder,
    PVB = TaikoEngineValidatorBuilder,
    EB = TaikoEngineApiBuilder<PVB>,
    EVB = TaikoEngineValidatorBuilder,
    RpcMiddleware = Identity,
> {
    /// Inner RPC addon wiring that launches the node servers.
    inner: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>,
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> TaikoAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    EthB: reth_node_builder::rpc::EthApiBuilder<N>,
{
    /// Creates a new Taiko addon wrapper from the underlying RPC addon wiring.
    pub const fn new(inner: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>) -> Self {
        Self { inner }
    }

    /// Replaces the engine API builder.
    pub fn with_engine_api<T>(
        self,
        engine_api_builder: T,
    ) -> TaikoAddOns<N, EthB, PVB, T, EVB, RpcMiddleware>
    where
        T: Send,
    {
        let Self { inner } = self;
        TaikoAddOns::new(inner.with_engine_api(engine_api_builder))
    }

    /// Replaces the payload validator builder.
    pub fn with_payload_validator<T>(
        self,
        payload_validator_builder: T,
    ) -> TaikoAddOns<N, EthB, T, EB, EVB, RpcMiddleware> {
        let Self { inner } = self;
        TaikoAddOns::new(inner.with_payload_validator(payload_validator_builder))
    }

    /// Replaces the engine validator builder.
    pub fn with_engine_validator<T>(
        self,
        engine_validator_builder: T,
    ) -> TaikoAddOns<N, EthB, PVB, EB, T, RpcMiddleware> {
        let Self { inner } = self;
        TaikoAddOns::new(inner.with_engine_validator(engine_validator_builder))
    }

    /// Replaces the RPC middleware stack.
    pub fn with_rpc_middleware<T>(self, rpc_middleware: T) -> TaikoAddOns<N, EthB, PVB, EB, EVB, T>
    where
        T: Send,
    {
        let Self { inner } = self;
        TaikoAddOns::new(inner.with_rpc_middleware(rpc_middleware))
    }

    /// Sets the tokio runtime for the RPC servers.
    pub fn with_tokio_runtime(self, tokio_runtime: Option<tokio::runtime::Handle>) -> Self {
        let Self { inner } = self;
        Self { inner: inner.with_tokio_runtime(tokio_runtime) }
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> Default
    for TaikoAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    EthB: Default + reth_node_builder::rpc::EthApiBuilder<N>,
    PVB: Default,
    EB: Default,
    EVB: Default,
    RpcMiddleware: Default,
{
    /// Creates a new instance of `TaikoAddOns` with default configurations.
    fn default() -> Self {
        Self::new(RpcAddOns::new(
            EthB::default(),
            PVB::default(),
            EB::default(),
            EVB::default(),
            Default::default(),
        ))
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> NodeAddOns<N>
    for TaikoAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    EthB: reth_node_builder::rpc::EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N> + Clone,
    PVB::Validator: PayloadValidator<<N::Types as NodeTypes>::Payload, Block = BlockTy<N::Types>>
        + EngineApiValidator<<N::Types as NodeTypes>::Payload>,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: RethRpcMiddleware,
{
    /// Handle to add-ons.
    type Handle = RpcHandle<N, EthB::EthApi>;

    /// Configures and launches the add-ons.
    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        self.inner.launch_add_ons(ctx).await
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> RethRpcAddOns<N>
    for TaikoAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    EthB: reth_node_builder::rpc::EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N> + Clone,
    PVB::Validator: PayloadValidator<<N::Types as NodeTypes>::Payload, Block = BlockTy<N::Types>>
        + EngineApiValidator<<N::Types as NodeTypes>::Payload>,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: RethRpcMiddleware,
{
    /// eth API implementation.
    type EthApi = EthB::EthApi;

    /// Returns a mutable reference to RPC hooks.
    fn hooks_mut(&mut self) -> &mut RpcHooks<N, Self::EthApi> {
        self.inner.hooks_mut()
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware> EngineValidatorAddOn<N>
    for TaikoAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware>
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
    EthB: reth_node_builder::rpc::EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    RpcMiddleware: Send,
{
    /// The ValidatorBuilder type to use for the engine API.
    type ValidatorBuilder = EVB;

    /// Returns the engine validator builder.
    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        EngineValidatorAddOn::<N>::engine_validator_builder(&self.inner)
    }
}

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
