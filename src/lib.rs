use eyre::Ok;
use reth::{
    api::{FullNodeComponents, FullNodeTypes, NodeAddOns, NodeTypes},
    builder::{
        DebugNode, Node, NodeAdapter, NodeComponentsBuilder,
        components::{BasicPayloadServiceBuilder, ComponentsBuilder},
        rpc::{RpcAddOns, RpcHandle},
    },
    chainspec::ChainSpec,
    providers::EthStorage,
    revm::context::TxEnv,
    rpc::{
        eth::{EthApiFor, FullEthApiServer},
        server_types::eth::EthApiError,
    },
};
use reth_ethereum::EthPrimitives;
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor, NextBlockEnvAttributes};
use reth_node_ethereum::{
    EthereumEthApiBuilder,
    node::{
        EthereumConsensusBuilder, EthereumEngineValidatorBuilder, EthereumNetworkBuilder,
        EthereumPoolBuilder,
    },
};
use reth_rpc_eth_types::error::FromEvmError;
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

    type AddOns = TaikoAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
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
            .consensus(EthereumConsensusBuilder::default())
    }

    fn add_ons(&self) -> Self::AddOns {
        TaikoAddOns::default()
    }
}

impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for TaikoNode {
    type RpcBlock = alloy_rpc_types_eth::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        rpc_block.into_consensus().convert_transactions()
    }
}

/// Add-ons w.r.t. taiko.
#[derive(Debug)]
pub struct TaikoAddOns<N: FullNodeComponents>
where
    EthApiFor<N>: FullEthApiServer<Provider = N::Provider, Pool = N::Pool>,
{
    /// Rpc add-ons responsible for launching the RPC servers and instantiating the RPC handlers
    /// and eth-api.
    pub inner: RpcAddOns<N, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>,
}

impl<N: FullNodeComponents> Default for TaikoAddOns<N>
where
    EthApiFor<N>: FullEthApiServer<Provider = N::Provider, Pool = N::Pool>,
{
    fn default() -> Self {
        Self {
            inner: Default::default(),
        }
    }
}

impl<N> NodeAddOns<N> for TaikoAddOns<N>
where
    N: FullNodeComponents<
            Types: NodeTypes<
                ChainSpec = ChainSpec,
                Primitives = EthPrimitives,
                Payload = TaikoPayloadTypes,
            >,
            Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
        >,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
{
    type Handle = RpcHandle<N, EthApiFor<N>>;

    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        todo!()
    }
}
