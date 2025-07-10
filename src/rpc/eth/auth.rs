use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth::revm::primitives::U256;
use reth_db::transaction::DbTx;
use reth_db_api::transaction::DbTxMut;
use reth_provider::{DBProvider, DatabaseProviderFactory};
use reth_rpc_eth_types::EthApiError;

use crate::{
    db::model::{
        STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1Origin, StoredL1OriginTable,
    },
    payload::attributes::L1Origin,
};

/// trait interface for a custom auth rpc namespace: `taikoAuth`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taikoAuth"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taikoAuth"))]
pub trait TaikoAuthExtApi {
    #[method(name = "setHeadL1Origin")]
    async fn set_head_l1_origin(&self, id: U256) -> RpcResult<u64>;
    #[method(name = "updateL1Origin")]
    async fn update_l1_origin(&self, l1_origin: L1Origin) -> RpcResult<Option<L1Origin>>;
}

/// A concrete implementation of the `TaikoAuthExtApi` trait.
#[derive(Clone)]
pub struct TaikoAuthExt<Provider: DatabaseProviderFactory> {
    provider: Provider,
}

impl<Provider: DatabaseProviderFactory> TaikoAuthExt<Provider> {
    /// Creates a new instance of `TaikoAuthExt` with the given provider.
    pub fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl<Provider> TaikoAuthExtApiServer for TaikoAuthExt<Provider>
where
    Provider: DatabaseProviderFactory + 'static,
{
    /// Sets the L1 head origin in the database.
    async fn set_head_l1_origin(&self, id: U256) -> RpcResult<u64> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();

        tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, id.to::<u64>())
            .map_err(|_| EthApiError::InternalEthError)?;

        tx.commit().map_err(|_| EthApiError::InternalEthError)?;

        Ok(id.to())
    }

    /// Updates the L1 origin in the database.
    async fn update_l1_origin(&self, l1_origin: L1Origin) -> RpcResult<Option<L1Origin>> {
        let tx = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?
            .into_tx();

        tx.put::<StoredL1OriginTable>(
            l1_origin.block_id.to(),
            StoredL1Origin {
                block_id: l1_origin.block_id,
                l2_block_hash: l1_origin.l2_block_hash,
                l1_block_height: l1_origin.l1_block_height,
                l1_block_hash: l1_origin.l1_block_hash,
                build_payload_args_id: l1_origin.build_payload_args_id,
            },
        )
        .map_err(|_| EthApiError::InternalEthError)?;

        tx.commit().map_err(|_| EthApiError::InternalEthError)?;

        Ok(Some(l1_origin))
    }
}
