use std::{convert::Infallible, sync::Arc};

use reth_ethereum::EthPrimitives;
use reth_evm::ConfigureEvm;
use reth_evm_ethereum::RethReceiptBuilder;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx};
use reth_node_ethereum::EthereumEthApiBuilder;
use reth_rpc::eth::core::EthRpcConverterFor;

use crate::{
    block::{assembler::TaikoBlockAssembler, factory::TaikoBlockExecutorFactory},
    chainspec::spec::TaikoChainSpec,
    evm::{config::TaikoNextBlockEnvAttributes, factory::TaikoEvmFactory},
    payload::engine::TaikoEngineTypes,
    rpc::eth::types::TaikoEthApi,
};

/// Builds [`TaikoEthApi`] for the Taiko node.
#[derive(Debug, Default)]
pub struct TaikoEthApiBuilder(EthereumEthApiBuilder);

impl TaikoEthApiBuilder {
    /// Creates a new instance of `TaikoEthApiBuilder`.
    pub fn new() -> Self {
        Self(EthereumEthApiBuilder)
    }
}

impl<N> EthApiBuilder<N> for TaikoEthApiBuilder
where
    N: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec = TaikoChainSpec,
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
{
    /// The Ethapi implementation this builder will build.
    type EthApi = TaikoEthApi<N, EthRpcConverterFor<N>>;

    /// Builds the [`TaikoEthApi`] from the given context.
    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let api = ctx.eth_api_builder().build();

        Ok(TaikoEthApi(api))
    }
}
