use alethia_reth_chainspec::spec::TaikoChainSpec;
use reth_ethereum::{EthPrimitives, PooledTransactionVariant};
use reth_network::{EthNetworkPrimitives, NetworkHandle, PeersInfo};
use reth_node_api::{FullNodeTypes, NodeTypes, TxTy};
use reth_node_builder::{BuilderContext, components::NetworkBuilder};
use reth_transaction_pool::{PoolTransaction, TransactionPool};
use tracing::info;

/// A basic Taiko network builder service.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaikoNetworkBuilder;

impl<Node, Pool> NetworkBuilder<Node, Pool> for TaikoNetworkBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = TaikoChainSpec, Primitives = EthPrimitives>>,
    Pool: TransactionPool<
            Transaction: PoolTransaction<
                Consensus = TxTy<Node::Types>,
                Pooled = PooledTransactionVariant,
            >,
        > + Unpin
        + 'static,
{
    /// The network built.
    type Network = NetworkHandle<EthNetworkPrimitives>;

    /// Launches the network implementation and returns the handle to it.
    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::Network> {
        let network = ctx.network_builder().await?;
        let handle = ctx.start_network(network, pool);
        info!(target: "reth::taiko::cli", enode=%handle.local_node_record(), "P2P networking initialized");
        Ok(handle)
    }
}
