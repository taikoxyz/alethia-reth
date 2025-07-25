use std::{convert::Infallible, sync::Arc};

use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeTypes},
    builder::{
        DebugNode, Node,
        components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    },
    providers::EthStorage,
};
use reth_ethereum::EthPrimitives;
use reth_evm::ConfigureEvm;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{AddOnsContext, NodeAddOns};
use reth_node_builder::{
    NodeAdapter, NodeComponentsBuilder,
    rpc::{EngineValidatorAddOn, EngineValidatorBuilder, RethRpcAddOns, RpcAddOns, RpcHandle},
};
use reth_node_ethereum::node::EthereumPoolBuilder;
use reth_trie_db::MerklePatriciaTrie;

pub mod block;
pub mod chainspec;
pub mod cli;
pub mod consensus;
pub mod db;
pub mod evm;
pub mod network;
pub mod payload;
pub mod rpc;

use crate::{
    block::{
        assembler::TaikoBlockAssembler,
        factory::{TaikoBlockExecutorFactory, TaikoExecutorBuilder},
    },
    chainspec::spec::TaikoChainSpec,
    consensus::builder::TaikoConsensusBuilder,
    evm::{config::TaikoNextBlockEnvAttributes, factory::TaikoEvmFactory},
    network::TaikoNetworkBuilder,
    payload::{TaikoPayloadBuilderBuilder, engine::TaikoEngineTypes},
    rpc::{
        engine::{
            builder::TaikoEngineApiBuilder,
            validator::{TaikoEngineValidator, TaikoEngineValidatorBuilder},
        },
        eth::{builder::TaikoEthApiBuilder, types::TaikoEthApi},
    },
};

/// The main node type for a Taiko network node, implementing the `NodeTypes` trait.
#[derive(Debug, Clone, Default)]
pub struct TaikoNode;

impl NodeTypes for TaikoNode {
    /// The node's primitive types, defining basic operations and structures.
    type Primitives = EthPrimitives;
    /// The type used for configuration of the EVM.
    type ChainSpec = TaikoChainSpec;
    /// The type used to perform state commitment operations.
    type StateCommitment = MerklePatriciaTrie;
    /// The type responsible for writing chain primitives to storage.
    type Storage = EthStorage;
    /// The node's engine types, defining the interaction with the consensus engine.
    type Payload = TaikoEngineTypes;
}

/// Taiko custom addons which configuring RPC types.
pub struct TaikoAddOns<
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV,
>(pub RpcAddOns<N, TaikoEthApiBuilder, EV, TaikoEngineApiBuilder<EV>>);

impl<N, EV> Default for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: Default,
{
    /// Creates a new instance of `TaikoAddOns` with default configurations.
    fn default() -> Self {
        let add_ons = RpcAddOns::new(
            TaikoEthApiBuilder::default(),
            EV::default(),
            TaikoEngineApiBuilder::default(),
            Default::default(),
        );

        TaikoAddOns(add_ons)
    }
}

impl<N, EV> NodeAddOns<N> for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: EngineValidatorBuilder<N>,
{
    /// Handle to add-ons.
    type Handle = RpcHandle<N, TaikoEthApi<N::Provider, N::Pool, N::Network, N::Evm>>;

    /// Configures and launches the add-ons.
    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        self.0.launch_add_ons(ctx).await
    }
}

impl<N, EV> RethRpcAddOns<N> for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: EngineValidatorBuilder<N>,
{
    /// eth API implementation.
    type EthApi = TaikoEthApi<N::Provider, N::Pool, N::Network, N::Evm>;

    /// Returns a mutable reference to RPC hooks.
    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.0.hooks_mut()
    }
}

impl<N, EV> EngineValidatorAddOn<N> for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
                StateCommitment = MerklePatriciaTrie,
                Storage = EthStorage,
                Payload = TaikoEngineTypes,
            >,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: EngineValidatorBuilder<N>,
{
    /// The Validator type to use for the engine API.
    type Validator = TaikoEngineValidator;

    /// Creates the engine validator for an engine API based node.
    async fn engine_validator(&self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        TaikoEngineValidatorBuilder::default().build(ctx).await
    }
}

impl<N> Node<N> for TaikoNode
where
    N: FullNodeTypes<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            StateCommitment = MerklePatriciaTrie,
            Storage = EthStorage,
            Payload = TaikoEngineTypes,
        >,
    >,
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
    type AddOns = TaikoAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        TaikoEngineValidatorBuilder,
    >;

    /// Returns a [`NodeComponentsBuilder`] for the node.
    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types()
            .pool(EthereumPoolBuilder::default())
            .executor(TaikoExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::new(TaikoPayloadBuilderBuilder))
            .network(TaikoNetworkBuilder)
            .consensus(TaikoConsensusBuilder::default())
    }

    /// Returns the node add-ons.
    fn add_ons(&self) -> Self::AddOns {
        TaikoAddOns::default()
    }
}

impl<
    N: FullNodeComponents<
            Types = Self,
            Evm: ConfigureEvm<
                Primitives = EthPrimitives,
                Error = Infallible,
                NextBlockEnvCtx = TaikoNextBlockEnvAttributes,
                BlockExecutorFactory = TaikoBlockExecutorFactory<
                    RethReceiptBuilder,
                    Arc<TaikoChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
> DebugNode<N> for TaikoNode
{
    /// RPC block type. Used by [`DebugConsensusClient`] to fetch blocks and submit them to the
    /// engine.
    type RpcBlock = alloy_rpc_types_eth::Block;

    /// Converts an RPC block to a primitive block.
    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        rpc_block.into_consensus().convert_transactions()
    }
}
