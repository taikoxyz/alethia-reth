use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_chainspec::spec::TaikoChainSpec;
use alethia_reth_primitives::engine::TaikoEngineTypes;
use alethia_reth_rpc::eth::types::TaikoEthApi;
use reth_ethereum::EthPrimitives;
use reth_node_api::{FullNodeComponents, NodeTypes};
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
    N: FullNodeComponents<Evm = TaikoEvmConfig>,
    N::Types: NodeTypes<
            Primitives = EthPrimitives,
            ChainSpec = TaikoChainSpec,
            Payload = TaikoEngineTypes,
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
