use std::sync::Arc;

use alloy_hardforks::EthereumHardforks;
use alloy_rpc_types_engine::{
    ClientVersionV1, ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus,
};
use async_trait::async_trait;
use jsonrpsee::RpcModule;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee_core::RpcResult;
use reth::{
    payload::PayloadStore, rpc::api::IntoEngineApiRpcModule, tasks::TaskSpawner,
    transaction_pool::TransactionPool,
};
use reth_db_api::transaction::DbTxMut;
use reth_node_api::{BeaconConsensusEngineHandle, EngineTypes, EngineValidator, PayloadTypes};
use reth_provider::{BlockReader, HeaderProvider, StateProviderFactory};
use reth_provider::{DBProvider, DatabaseProviderFactory};
use reth_rpc::EngineApi;
use reth_rpc_engine_api::{EngineApiError, EngineCapabilities};
use tracing::info;

use crate::db::model::{
    STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1Origin, StoredL1OriginTable,
};
use crate::payload::attributes::TaikoPayloadAttributes;
use crate::rpc::types::TaikoExecutionData;

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

pub struct TaikoEngineApi<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec> {
    inner: EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>,
    provider: Provider,
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
    pub fn new(
        provider: Provider,
        chain_spec: Arc<ChainSpec>,
        beacon_consensus: BeaconConsensusEngineHandle<PayloadT>,
        payload_store: PayloadStore<PayloadT>,
        tx_pool: Pool,
        task_spawner: Box<dyn TaskSpawner>,
        client: ClientVersionV1,
        capabilities: EngineCapabilities,
        validator: Validator,
        accept_execution_requests_hash: bool,
    ) -> Self
    where
        Provider: Clone,
    {
        let inner = EngineApi::new(
            provider.clone(),
            chain_spec,
            beacon_consensus,
            payload_store,
            tx_pool,
            task_spawner,
            client,
            capabilities,
            validator,
            accept_execution_requests_hash,
        );
        Self { inner, provider }
    }
}

// This is the concrete ethereum engine API implementation.
#[async_trait]
impl<Provider, EngineT, Pool, Validator, ChainSpec> TaikoEngineApiServer<EngineT>
    for TaikoEngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    Provider:
        HeaderProvider + BlockReader + DatabaseProviderFactory + StateProviderFactory + 'static,
    EngineT:
        EngineTypes<ExecutionData = TaikoExecutionData, PayloadAttributes = TaikoPayloadAttributes>,
    Pool: TransactionPool + 'static,
    Validator: EngineValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    async fn new_payload_v2(&self, payload: TaikoExecutionData) -> RpcResult<PayloadStatus> {
        self.inner
            .new_payload_v2(payload)
            .await
            .map_err(|e| EngineApiError::from(e).into())
    }

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
            let stored_l1_origin = StoredL1Origin {
                block_id: payload.l1_origin.block_id,
                l2_block_hash: status.payload_status.latest_valid_hash.unwrap(),
                l1_block_hash: payload.l1_origin.l1_block_hash,
                l1_block_height: payload.l1_origin.l1_block_height,
                build_payload_args_id: payload.l1_origin.build_payload_args_id.unwrap(),
            };
            self.provider
                .database_provider_rw()
                .unwrap()
                .into_tx()
                .put::<StoredL1OriginTable>(
                    payload.l1_origin.block_id.to::<u64>(),
                    stored_l1_origin.clone(),
                )
                .unwrap();
            if payload.l1_origin.is_preconf_block() {
                self.provider
                    .database_provider_rw()
                    .unwrap()
                    .into_tx()
                    .put::<StoredL1HeadOriginTable>(
                        STORED_L1_HEAD_ORIGIN_KEY,
                        payload.l1_origin.block_id.to::<u64>(),
                    )
                    .unwrap();
            }
        };

        Ok(status)
    }

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
    fn into_rpc_module(self) -> RpcModule<()> {
        self.into_rpc().remove_context()
    }
}
