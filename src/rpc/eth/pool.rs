use alloy_consensus::Transaction as ConsensusTx;
use alloy_json_rpc::RpcObject;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth::{
    revm::primitives::U256,
    rpc::compat::TransactionCompat,
    transaction_pool::{PoolConsensusTx, PoolTransaction, TransactionPool},
};
use reth_node_api::NodePrimitives;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct PreBuiltTxList<T> {
    pub tx_list: Vec<T>,
    pub estimated_gas_used: u64,
    pub bytes_length: u64,
}

impl<T> Default for PreBuiltTxList<T> {
    fn default() -> Self {
        Self {
            tx_list: vec![],
            estimated_gas_used: 0,
            bytes_length: 0,
        }
    }
}

/// trait interface for a custom auth rpc namespace: `taiko`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taiko"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taiko"))]
pub trait TaikoAuthTxPoolExtApi<T: RpcObject> {
    #[method(name = "txPoolContentWithMinTip")]
    async fn tx_pool_content_with_min_tip(&self, min_tip: U256) -> RpcResult<PreBuiltTxList<T>>;
}

#[derive(Clone)]
pub struct TaikoAuthTxPoolExt<Pool, Eth> {
    pool: Pool,
    tx_resp_builder: Eth,
}

impl<Pool, Eth> TaikoAuthTxPoolExt<Pool, Eth> {
    pub fn new(pool: Pool, tx_resp_builder: Eth) -> Self {
        Self {
            pool,
            tx_resp_builder,
        }
    }
}

#[async_trait]
impl<Pool, Eth> TaikoAuthTxPoolExtApiServer<Eth::Transaction> for TaikoAuthTxPoolExt<Pool, Eth>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus: ConsensusTx>> + 'static,
    Eth: TransactionCompat<Primitives: NodePrimitives<SignedTx = PoolConsensusTx<Pool>>> + 'static,
{
    async fn tx_pool_content_with_min_tip(
        &self,
        _: U256,
    ) -> RpcResult<PreBuiltTxList<Eth::Transaction>> {
        let pending = self.pool.all_transactions().pending;

        let mut result = vec![];

        for pending in pending {
            result.push(
                self.tx_resp_builder
                    .fill_pending(pending.transaction.clone_into_consensus())
                    .unwrap(),
            );
        }

        Ok(PreBuiltTxList {
            tx_list: result,
            estimated_gas_used: 0,
            bytes_length: 0,
        })
    }
}
