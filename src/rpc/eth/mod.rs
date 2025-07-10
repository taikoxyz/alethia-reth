use alloy_primitives::U256;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_db_api::transaction::DbTx;
use reth_provider::{DBProvider, DatabaseProviderFactory};
use reth_rpc_eth_types::EthApiError;

use crate::{
    db::model::{STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable},
    payload::attributes::L1Origin,
    rpc::eth::error::TaikoApiError,
};

pub mod api;
pub mod auth;
pub mod builder;
pub mod error;
pub mod pool;

/// trait interface for a custom rpc namespace: `taiko`
///
/// This defines the Taiko namespace where all methods are configured as trait functions.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taiko"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taiko"))]
pub trait TaikoExtApi {
    #[method(name = "l1OriginByID")]
    fn l1_origin_by_id(&self, id: U256) -> RpcResult<Option<L1Origin>>;
    #[method(name = "headL1Origin")]
    fn head_l1_origin(&self) -> RpcResult<Option<L1Origin>>;
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
    fn l1_origin_by_id(&self, id: U256) -> RpcResult<Option<L1Origin>> {
        let provider = self
            .provider
            .database_provider_ro()
            .map_err(|_| EthApiError::InternalEthError)?;

        let l1_origin = provider
            .into_tx()
            .get::<StoredL1OriginTable>(id.to())
            .map_err(|_| TaikoApiError::GethNotFound)?;

        if let Some(l1_origin) = l1_origin {
            Ok(Some(L1Origin {
                block_id: l1_origin.block_id,
                l2_block_hash: l1_origin.l2_block_hash,
                l1_block_height: l1_origin.l1_block_height,
                l1_block_hash: l1_origin.l1_block_hash,
                build_payload_args_id: l1_origin.build_payload_args_id,
            }))
        } else {
            Err(TaikoApiError::GethNotFound.into())
        }
    }

    /// Retrieves the head L1 origin from the database.
    fn head_l1_origin(&self) -> RpcResult<Option<L1Origin>> {
        let provider = self
            .provider
            .database_provider_ro()
            .map_err(|_| EthApiError::InternalEthError)?;

        let head_l1_origin = provider
            .into_tx()
            .get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)
            .map_err(|_| TaikoApiError::GethNotFound)?;

        if let Some(l1_origin) = head_l1_origin {
            self.l1_origin_by_id(U256::from(l1_origin))
        } else {
            Err(TaikoApiError::GethNotFound.into())
        }
    }
}
