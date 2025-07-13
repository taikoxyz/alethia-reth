use std::time::Duration;

use crate::payload::attributes::TaikoPayloadAttributes;
use crate::rpc::engine::types::TaikoExecutionData;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::BlockNumber;
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus};
use async_trait::async_trait;
use jsonrpsee::RpcModule;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::RpcResult;
use jsonrpsee_types::ErrorCode;
use reth::{
    payload::PayloadStore, rpc::api::IntoEngineApiRpcModule, transaction_pool::TransactionPool,
};
use reth_db::transaction::DbTx;
use reth_db_api::transaction::DbTxMut;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_node_api::{EngineTypes, EngineValidator, PayloadBuilderError, PayloadTypes};
use reth_provider::{BlockReader, HeaderProvider, StateProviderFactory};
use reth_provider::{DBProvider, DatabaseProviderFactory};
use reth_rpc::EngineApi;
use reth_rpc_engine_api::EngineApiError;
use tokio_retry::Retry;
use tokio_retry::strategy::ExponentialBackoff;

use crate::db::model::{
    STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1Origin, StoredL1OriginTable,
};

/// The list of all supported Engine capabilities available over the engine endpoint.
pub const TAIKO_ENGINE_CAPABILITIES: &[&str] = &[
    "engine_forkchoiceUpdatedV2",
    "engine_getPayloadV2",
    "engine_newPayloadV2",
];

/// Extension trait that gives access to Taiko engine API RPC methods.
///
/// Note:
/// > The provider should use a JWT authentication layer.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "engine"), server_bounds(Engine::PayloadAttributes: jsonrpsee::core::DeserializeOwned))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "engine", client_bounds(Engine::PayloadAttributes: jsonrpsee::core::Serialize + Clone), server_bounds(Engine::PayloadAttributes: jsonrpsee::core::DeserializeOwned)))]
pub trait TaikoEngineApi<Engine: EngineTypes> {
    #[method(name = "newPayloadV2")]
    async fn new_payload_v2(&self, payload: TaikoExecutionData) -> RpcResult<PayloadStatus>;

    #[method(name = "forkchoiceUpdatedV2")]
    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<Engine::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated>;

    #[method(name = "getPayloadV2")]
    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<Engine::ExecutionPayloadEnvelopeV2>;
}

/// A concrete implementation of the `TaikoEngineApi` trait.
pub struct TaikoEngineApi<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec> {
    inner: EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>,
    provider: Provider,
    payload_store: PayloadStore<PayloadT>,
}

impl<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec>
    TaikoEngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>
where
    Provider:
        HeaderProvider + BlockReader + DatabaseProviderFactory + StateProviderFactory + 'static,
    PayloadT: PayloadTypes,
    Pool: TransactionPool + 'static,
    Validator: EngineValidator<PayloadT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Creates a new instance of `TaikoEngineApi` with the given parameters.
    pub fn new(
        engine_api: EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>,
        provider: Provider,
        payload_store: PayloadStore<PayloadT>,
    ) -> Self
    where
        Provider: Clone,
    {
        Self {
            inner: engine_api,
            provider,
            payload_store,
        }
    }
}

// This is the concrete ethereum engine API implementation.
#[async_trait]
impl<Provider, EngineT, Pool, Validator, ChainSpec> TaikoEngineApiServer<EngineT>
    for TaikoEngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    Provider:
        HeaderProvider + BlockReader + DatabaseProviderFactory + StateProviderFactory + 'static,
    EngineT: EngineTypes<
            ExecutionData = TaikoExecutionData,
            PayloadAttributes = TaikoPayloadAttributes,
            BuiltPayload = EthBuiltPayload,
        >,
    Pool: TransactionPool + 'static,
    Validator: EngineValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Creates a new execution payload with the given execution data.
    async fn new_payload_v2(&self, payload: TaikoExecutionData) -> RpcResult<PayloadStatus> {
        self.inner
            .new_payload_v2(payload)
            .await
            .map_err(|e| EngineApiError::from(e).into())
    }

    /// Updates the fork choice with the given state and payload attributes.
    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        let status = self
            .inner
            .fork_choice_updated_v2(fork_choice_state, payload_attributes.clone())
            .await?;

        if let Some(payload) = payload_attributes {
            // Wait for the new payload to be built and stored, then we can stroe the
            // coressponding L1 origin into the database.
            let built_payload = Retry::spawn(
                ExponentialBackoff::from_millis(50)
                    .max_delay(Duration::from_secs(12))
                    .take(3 as usize),
                || async {
                    match self
                        .payload_store
                        .best_payload(status.payload_id.expect("payload_id must not be empty"))
                        .await
                    {
                        Some(Ok(value)) => Ok(value),
                        _ => Err(()),
                    }
                },
            )
            .await
            .map_err(|_| EngineApiError::GetPayloadError(PayloadBuilderError::MissingPayload))?;

            let stored_l1_origin = StoredL1Origin {
                block_id: payload.l1_origin.block_id,
                l2_block_hash: built_payload.block().hash_slow(),
                l1_block_hash: payload.l1_origin.l1_block_hash,
                l1_block_height: payload.l1_origin.l1_block_height,
                build_payload_args_id: payload.l1_origin.build_payload_args_id,
            };

            let tx = self
                .provider
                .database_provider_rw()
                .map_err(|_| EngineApiError::Other(ErrorCode::InternalError.into()))?
                .into_tx();

            tx.put::<StoredL1OriginTable>(
                payload.l1_origin.block_id.to::<BlockNumber>(),
                stored_l1_origin.clone(),
            )
            .map_err(|_| EngineApiError::Other(ErrorCode::InternalError.into()))?;

            if !payload.l1_origin.is_preconf_block() {
                tx.put::<StoredL1HeadOriginTable>(
                    STORED_L1_HEAD_ORIGIN_KEY,
                    payload.l1_origin.block_id.to::<BlockNumber>(),
                )
                .map_err(|_| EngineApiError::Other(ErrorCode::InternalError.into()))?;
            }

            tx.commit()
                .map_err(|_| EngineApiError::Other(ErrorCode::InternalError.into()))?;
        };

        Ok(status)
    }

    /// Retrieves the execution payload by its ID.
    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV2> {
        self.inner
            .get_payload_v2(payload_id)
            .await
            .map_err(|e| EngineApiError::from(e).into())
    }
}

impl<Provider, EngineT, Pool, Validator, ChainSpec> IntoEngineApiRpcModule
    for TaikoEngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    EngineT: EngineTypes,
    Self: TaikoEngineApiServer<EngineT>,
{
    /// Consumes the type and returns all the methods and subscriptions defined in the trait and
    /// returns them as a single [`RpcModule`]
    fn into_rpc_module(self) -> RpcModule<()> {
        self.into_rpc().remove_context()
    }
}
