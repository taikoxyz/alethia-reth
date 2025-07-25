use alloy_primitives::U256;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_db_api::transaction::DbTx;
use reth_provider::{DBProvider, DatabaseProviderFactory};
use reth_rpc_eth_types::EthApiError;

use crate::{
    db::model::{STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable},
    payload::attributes::RpcL1Origin,
    rpc::eth::error::TaikoApiError,
};

/// trait interface for a custom rpc namespace: `taiko`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taiko"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taiko"))]
pub trait TaikoExtApi {
    #[method(name = "l1OriginByID")]
    fn l1_origin_by_id(&self, id: U256) -> RpcResult<Option<RpcL1Origin>>;
    #[method(name = "headL1Origin")]
    fn head_l1_origin(&self) -> RpcResult<Option<RpcL1Origin>>;
}

/// The Taiko RPC extension implementation.
pub struct TaikoExt<Provider: DatabaseProviderFactory> {
    provider: Provider,
}

impl<Provider: DatabaseProviderFactory> TaikoExt<Provider> {
    /// Creates a new instance of `TaikoExt` with the given provider.
    pub fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

impl<Provider: DatabaseProviderFactory + 'static> TaikoExtApiServer for TaikoExt<Provider> {
    /// Retrieves the L1 origin by its ID from the database.
    fn l1_origin_by_id(&self, id: U256) -> RpcResult<Option<RpcL1Origin>> {
        let provider =
            self.provider.database_provider_ro().map_err(|_| EthApiError::InternalEthError)?;

        Ok(Some(
            provider
                .into_tx()
                .get::<StoredL1OriginTable>(id.to())
                .map_err(|_| EthApiError::InternalEthError)?
                .ok_or(TaikoApiError::GethNotFound)?
                .into(),
        ))
    }

    /// Retrieves the head L1 origin from the database.
    fn head_l1_origin(&self) -> RpcResult<Option<RpcL1Origin>> {
        let provider =
            self.provider.database_provider_ro().map_err(|_| EthApiError::InternalEthError)?;

        self.l1_origin_by_id(U256::from(
            provider
                .into_tx()
                .get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)
                .map_err(|_| EthApiError::InternalEthError)?
                .ok_or(TaikoApiError::GethNotFound)?,
        ))
    }
}
