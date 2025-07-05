use alloy_consensus::Transaction as ConsensusTx;
use alloy_json_rpc::RpcObject;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth::{
    revm::primitives::Address,
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

/// trait interface for a custom auth rpc namespace: `taikoAuth`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taikoAuth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taikoAuth"))]
pub trait TaikoAuthTxPoolExtApi<T: RpcObject> {
    #[method(name = "txPoolContentWithMinTip")]
    async fn tx_pool_content_with_min_tip(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
        min_tip: u64,
    ) -> RpcResult<PreBuiltTxList<T>>;
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
// method=taikoAuth_txPoolContentWithMinTip result="{\"txList\":[{\"type\":\"0x0\",\"chainId\":\"0x28c59\",\"nonce\":\"0x0\",\"gasPrice\":\"0x2540be400\",\"gas\":\"0x186a0\",\"to\":\"0xb40ae7cc03e65c4cb2484b6399fb542b9a9b206b\",\"value\":\"0x0\",\"input\":\"0x\",\"r\":\"0x6cd0e3bb7b2a70c9062e7498034fc428acf9281359e2bb1a3b5c364bd5783f37\",\"s\":\"0x34fd2c5b70e7ce66b0a2516d381d2f44d3e8e69c3c7a9a2a3071f669e0e70703\",\"v\":\"0x518d6\",\"hash\":\"0x9fdfeaa0a8dc6b2a2404087b040864faab7cde3aef416c1e332017d78491b46a\",\"blockHash\":null,\"blockNumber\":null,\"transactionIndex\":null,\"from\":\"0x90f79bf6eb2c4f870365e785982e1f101e93b906\"}],\"estimatedGasUsed\":0,\"bytesLength\":0}"
#[async_trait]
impl<Pool, Eth> TaikoAuthTxPoolExtApiServer<Eth::Transaction> for TaikoAuthTxPoolExt<Pool, Eth>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus: ConsensusTx>> + 'static,
    Eth: TransactionCompat<Primitives: NodePrimitives<SignedTx = PoolConsensusTx<Pool>>> + 'static,
{
    async fn tx_pool_content_with_min_tip(
        &self,
        beneficiary: Address,
        base_fee: u64,
        block_max_gas_limit: u64,
        max_bytes_per_tx_list: u64,
        locals: Option<Vec<Address>>,
        max_transactions_lists: u64,
        min_tip: u64,
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
