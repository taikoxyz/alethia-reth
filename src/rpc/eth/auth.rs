use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth::revm::primitives::U256;
use reth_db_api::transaction::DbTxMut;
use reth_provider::{DBProvider, DatabaseProviderFactory};
use reth_rpc_eth_types::EthApiError;

use crate::{
    db::model::{
        STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1Origin, StoredL1OriginTable,
    },
    payload::attributes::L1Origin,
};

/// trait interface for a custom auth rpc namespace: `taiko`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(test), rpc(server, namespace = "taiko"))]
#[cfg_attr(test, rpc(server, client, namespace = "taiko"))]
pub trait TaikoAuthExtApi {
    #[method(name = "setHeadL1Origin")]
    fn set_head_l1_origin(&self, id: U256) -> RpcResult<U256>;
    #[method(name = "updateL1Origin")]
    fn update_l1_origin(&self, l1_origin: L1Origin) -> RpcResult<Option<L1Origin>>;
}

pub struct TaikoAuthExt<Provider: DatabaseProviderFactory> {
    provider: Provider,
}

impl<Provider: DatabaseProviderFactory> TaikoAuthExt<Provider> {
    pub fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

impl<Provider: DatabaseProviderFactory + 'static> TaikoAuthExtApiServer for TaikoAuthExt<Provider> {
    fn set_head_l1_origin(&self, id: U256) -> RpcResult<U256> {
        let provider = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?;

        provider
            .into_tx()
            .put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, id.to::<u64>())
            .map_err(|_| EthApiError::InternalEthError)?;

        Ok(id)
    }

    fn update_l1_origin(&self, l1_origin: L1Origin) -> RpcResult<Option<L1Origin>> {
        let provider = self
            .provider
            .database_provider_rw()
            .map_err(|_| EthApiError::InternalEthError)?;

        provider
            .into_tx()
            .put::<StoredL1OriginTable>(
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

        Ok(Some(l1_origin))
    }
}
