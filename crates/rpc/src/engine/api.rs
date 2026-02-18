//! Taiko engine API RPC methods and persistence hooks.
use std::io;

use alethia_reth_primitives::{
    engine::types::TaikoExecutionData,
    payload::{attributes::TaikoPayloadAttributes, built_payload::TaikoBuiltPayload},
};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::BlockNumber;
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus};
use async_trait::async_trait;
use jsonrpsee::{RpcModule, proc_macros::rpc};
use jsonrpsee_core::RpcResult;
use jsonrpsee_types::ErrorObjectOwned;
use reth_db::transaction::DbTx;
use reth_db_api::transaction::DbTxMut;
use reth_engine_primitives::EngineApiValidator;
use reth_node_api::{EngineTypes, PayloadBuilderError, PayloadKind, PayloadTypes};
use reth_payload_builder::PayloadStore;
use reth_provider::{
    BlockReader, DBProvider, DatabaseProviderFactory, HeaderProvider, StateProviderFactory,
};
use reth_rpc::EngineApi;
use reth_rpc_api::IntoEngineApiRpcModule;
use reth_rpc_engine_api::EngineApiError;
use reth_transaction_pool::TransactionPool;

use alethia_reth_db::model::{
    STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1Origin, StoredL1OriginTable,
};

/// The list of all supported Engine capabilities available over the engine endpoint.
pub const TAIKO_ENGINE_CAPABILITIES: &[&str] =
    &["engine_forkchoiceUpdatedV2", "engine_getPayloadV2", "engine_newPayloadV2"];

/// Extension trait that gives access to Taiko engine API RPC methods.
///
/// Note:
/// > The provider should use a JWT authentication layer.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "engine"), server_bounds(Engine::PayloadAttributes: jsonrpsee::core::DeserializeOwned))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "engine", client_bounds(Engine::PayloadAttributes: jsonrpsee::core::Serialize + Clone), server_bounds(Engine::PayloadAttributes: jsonrpsee::core::DeserializeOwned)))]
pub trait TaikoEngineApi<Engine: EngineTypes> {
    /// Submit a new execution payload and return validation status.
    #[method(name = "newPayloadV2")]
    async fn new_payload_v2(&self, payload: TaikoExecutionData) -> RpcResult<PayloadStatus>;

    /// Update fork choice and optionally start payload building.
    #[method(name = "forkchoiceUpdatedV2")]
    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<Engine::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated>;

    /// Fetch a previously built payload by ID.
    #[method(name = "getPayloadV2")]
    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<Engine::ExecutionPayloadEnvelopeV2>;
}

/// A concrete implementation of the `TaikoEngineApi` trait.
pub struct TaikoEngineApi<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec> {
    /// Underlying `reth` engine API implementation.
    inner: EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>,
    /// Provider used for DB reads/writes during L1-origin persistence.
    provider: Provider,
    /// Payload store used to resolve built payloads by payload ID.
    payload_store: PayloadStore<PayloadT>,
}

impl<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec>
    TaikoEngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>
where
    Provider:
        HeaderProvider + BlockReader + DatabaseProviderFactory + StateProviderFactory + 'static,
    PayloadT: PayloadTypes,
    Pool: TransactionPool + 'static,
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
        Self { inner: engine_api, provider, payload_store }
    }
}

/// Internal helper methods for `TaikoEngineApi`.
impl<Provider, EngineT, Pool, Validator, ChainSpec>
    TaikoEngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    Provider:
        HeaderProvider + BlockReader + DatabaseProviderFactory + StateProviderFactory + 'static,
    EngineT: EngineTypes<
            ExecutionData = TaikoExecutionData,
            PayloadAttributes = TaikoPayloadAttributes,
            BuiltPayload = TaikoBuiltPayload,
        >,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Convenience helper to wrap an internal error, preserving the original message.
    fn internal_error<E>(err: E) -> EngineApiError
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        EngineApiError::Internal(Box::new(err))
    }

    /// Waits for a built payload to appear in the payload store; maps absence to `MissingPayload`.
    async fn wait_for_built_payload(
        &self,
        payload_id: PayloadId,
    ) -> Result<EngineT::BuiltPayload, EngineApiError> {
        // Leverage the payload builder's own resolution path instead of manual polling.
        match self.payload_store.resolve_kind(payload_id, PayloadKind::WaitForPending).await {
            Some(Ok(payload)) => Ok(payload),
            _ => Err(EngineApiError::GetPayloadError(PayloadBuilderError::MissingPayload)),
        }
    }

    /// Persists the L1 origin for the given built payload in a single transaction, updating the
    /// head pointer when the block is not pre-confirmation.
    fn persist_l1_origin(
        &self,
        stored_l1_origin: StoredL1Origin,
        is_preconf_block: bool,
    ) -> Result<(), EngineApiError> {
        let tx = self.provider.database_provider_rw().map_err(Self::internal_error)?.into_tx();

        let block_number = stored_l1_origin.block_id.to::<BlockNumber>();

        tx.put::<StoredL1OriginTable>(block_number, stored_l1_origin)
            .map_err(Self::internal_error)?;

        if !is_preconf_block {
            tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, block_number)
                .map_err(Self::internal_error)?;
        }

        tx.commit().map_err(Self::internal_error)?;

        Ok(())
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
            BuiltPayload = TaikoBuiltPayload,
        >,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Creates a new execution payload with the given execution data.
    async fn new_payload_v2(&self, payload: TaikoExecutionData) -> RpcResult<PayloadStatus> {
        self.inner.new_payload_v2(payload).await.map_err(|e| e.into())
    }

    /// Updates the fork choice with the given state and payload attributes.
    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        let (stored_l1_origin, is_preconf_block) = match payload_attributes.as_ref() {
            Some(payload) => (
                Some(StoredL1Origin::from(&payload.l1_origin)),
                payload.l1_origin.is_preconf_block(),
            ),
            None => (None, false),
        };

        let status =
            self.inner.fork_choice_updated_v2(fork_choice_state, payload_attributes).await?;

        if let Some(mut stored_l1_origin) = stored_l1_origin {
            let payload_id = status
                .payload_id
                .ok_or_else(|| Self::internal_error(io::Error::other("missing payload id")))?;

            let built_payload = self
                .wait_for_built_payload(payload_id)
                .await
                .map_err(|e: EngineApiError| ErrorObjectOwned::from(e))?;

            stored_l1_origin.l2_block_hash = built_payload.block().hash_slow();

            self.persist_l1_origin(stored_l1_origin, is_preconf_block)
                .map_err(|e: EngineApiError| ErrorObjectOwned::from(e))?;
        }

        Ok(status)
    }

    /// Retrieves the execution payload by its ID.
    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV2> {
        self.inner.get_payload_v2(payload_id).await.map_err(|e| e.into())
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
