use std::{convert::Infallible, sync::Arc};

use crate::{
    consensus::builder::TaikoConsensusBuilder,
    factory::{
        assembler::TaikoBlockAssembler, block::TaikoBlockExecutorFactory,
        builder::TaikoExecutorBuilder, config::TaikoNextBlockEnvAttributes,
        factory::TaikoEvmFactory,
    },
    payload::{TaikoPayloadBuilderBuilder, engine::TaikoEngineTypes},
    rpc::{
        builder::TaikoEngineApiBuilder,
        engine::{TaikoEngineValidator, TaikoEngineValidatorBuilder},
        eth::{api::TaikoEthApi, builder::TaikoEthApiBuilder},
    },
};
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
use reth_evm::ConfigureEvm;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{AddOnsContext, NodeAddOns};
use reth_node_builder::{
    NodeAdapter, NodeComponentsBuilder,
    rpc::{
        EngineValidatorAddOn, EngineValidatorBuilder, EthApiBuilder, RethRpcAddOns, RpcAddOns,
        RpcHandle,
    },
};
use reth_node_ethereum::{
    EthereumEthApiBuilder,
    node::{EthereumNetworkBuilder, EthereumPoolBuilder},
};
use reth_trie_db::MerklePatriciaTrie;

pub mod chainspec;
pub mod consensus;
pub mod db;
pub mod evm;
pub mod factory;
pub mod payload;
pub mod rpc;

#[derive(Debug, Clone, Default)]
pub struct TaikoNode;

impl NodeTypes for TaikoNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type StateCommitment = MerklePatriciaTrie;
    type Storage = EthStorage;
    type Payload = TaikoEngineTypes;
}

/// Custom addons configuring RPC types
pub struct TaikoAddOns<
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = ChainSpec,
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
                    Arc<ChainSpec>,
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
                ChainSpec = ChainSpec,
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
                    Arc<ChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: Default,
{
    fn default() -> Self {
        let add_ons = RpcAddOns::new(
            TaikoEthApiBuilder::default(),
            EV::default(),
            TaikoEngineApiBuilder::default(),
        );

        TaikoAddOns(add_ons)
    }
}

impl<N, EV> NodeAddOns<N> for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = ChainSpec,
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
                    Arc<ChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: EngineValidatorBuilder<N>,
{
    type Handle = RpcHandle<N, TaikoEthApi<N::Provider, N::Pool, N::Network, N::Evm>>;

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
                ChainSpec = ChainSpec,
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
                    Arc<ChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: EngineValidatorBuilder<N>,
{
    type EthApi = TaikoEthApi<N::Provider, N::Pool, N::Network, N::Evm>;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.0.hooks_mut()
    }
}

impl<N, EV> EngineValidatorAddOn<N> for TaikoAddOns<N, EV>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = ChainSpec,
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
                    Arc<ChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
    EV: EngineValidatorBuilder<N>,
{
    type Validator = TaikoEngineValidator;

    async fn engine_validator(&self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        TaikoEngineValidatorBuilder::default()
            .build(ctx)
            .await
            .map_err(eyre::Error::from)
    }
}

impl<N> Node<N> for TaikoNode
where
    N: FullNodeTypes<
        Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = ChainSpec,
            StateCommitment = MerklePatriciaTrie,
            Storage = EthStorage,
            Payload = TaikoEngineTypes,
        >,
    >,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<TaikoPayloadBuilderBuilder>,
        EthereumNetworkBuilder,
        TaikoExecutorBuilder,
        TaikoConsensusBuilder,
    >;

    type AddOns = TaikoAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        TaikoEngineValidatorBuilder,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        ComponentsBuilder::default()
            .node_types()
            .pool(EthereumPoolBuilder::default())
            .executor(TaikoExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::new(
                TaikoPayloadBuilderBuilder::default(),
            ))
            .network(EthereumNetworkBuilder::default())
            .consensus(TaikoConsensusBuilder::default())
    }

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
                    Arc<ChainSpec>,
                    TaikoEvmFactory,
                >,
                BlockAssembler = TaikoBlockAssembler,
            >,
        >,
> DebugNode<N> for TaikoNode
{
    type RpcBlock = alloy_rpc_types_eth::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        rpc_block.into_consensus().convert_transactions()
    }
}
