use crate::{TaikoNode, evm::config::TaikoEvmConfig, rpc::eth::types::TaikoEthApi};
use reth_node_api::FullNodeComponents;
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx};
use reth_node_ethereum::EthereumEthApiBuilder;
use reth_rpc::eth::core::EthRpcConverterFor;

/// Builds [`TaikoEthApi`] for the Taiko node.
#[derive(Debug, Default)]
pub struct TaikoEthApiBuilder(EthereumEthApiBuilder);

impl TaikoEthApiBuilder {
    /// Creates a new instance of `TaikoEthApiBuilder`.
    pub fn new() -> Self {
        Self(EthereumEthApiBuilder::default())
    }
}

impl<N> EthApiBuilder<N> for TaikoEthApiBuilder
where
    N: FullNodeComponents<Types = TaikoNode, Evm = TaikoEvmConfig>,
{
    /// The Ethapi implementation this builder will build.
    type EthApi = TaikoEthApi<N, EthRpcConverterFor<N>>;

    /// Builds the [`TaikoEthApi`] from the given context.
    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let api = ctx.eth_api_builder().build();

        Ok(TaikoEthApi(api))
    }
}
