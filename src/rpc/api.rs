use std::sync::Arc;

use alloy_eips::{eip4844::BlobAndProofV2, eip7685::RequestsOrHash};
use alloy_hardforks::EthereumHardforks;
use alloy_rpc_types_engine::{
    BlobAndProofV1, ClientVersionV1, ExecutionPayloadBodiesV1, ExecutionPayloadInputV2,
    ExecutionPayloadV1, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadId,
    PayloadStatus,
};
use async_trait::async_trait;
use jsonrpsee::RpcModule;
use jsonrpsee_core::RpcResult;
use reth::{
    payload::PayloadStore,
    revm::primitives::{
        B256,
        alloy_primitives::{BlockHash, U64},
    },
    rpc::api::IntoEngineApiRpcModule,
    tasks::TaskSpawner,
    transaction_pool::TransactionPool,
};
use reth_node_api::{BeaconConsensusEngineHandle, EngineTypes, EngineValidator, PayloadTypes};
use reth_provider::{BlockReader, HeaderProvider, StateProviderFactory};
use reth_rpc::EngineApi;
use reth_rpc_engine_api::{EngineApiServer, EngineCapabilities};

use crate::rpc::types::TaikoExecutionData;

pub struct TaikoEngineApi<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec> {
    inner: EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>,
}
impl<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec>
    TaikoEngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>
where
    Provider: HeaderProvider + BlockReader + StateProviderFactory + 'static,
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
    ) -> Self {
        let inner = EngineApi::new(
            provider,
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
        Self { inner }
    }
}
// This is the concrete ethereum engine API implementation.
#[async_trait]
impl<Provider, EngineT, Pool, Validator, ChainSpec> EngineApiServer<EngineT>
    for TaikoEngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    Provider: HeaderProvider + BlockReader + StateProviderFactory + 'static,
    EngineT: EngineTypes<ExecutionData = TaikoExecutionData>,
    Pool: TransactionPool + 'static,
    Validator: EngineValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    async fn new_payload_v1(&self, payload: ExecutionPayloadV1) -> RpcResult<PayloadStatus> {
        todo!()
    }

    async fn new_payload_v2(&self, payload: ExecutionPayloadInputV2) -> RpcResult<PayloadStatus> {
        todo!()
    }

    async fn new_payload_v3(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> RpcResult<PayloadStatus> {
        todo!()
    }

    async fn new_payload_v4(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        requests: RequestsOrHash,
    ) -> RpcResult<PayloadStatus> {
        todo!()
    }

    async fn fork_choice_updated_v1(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        todo!()
    }

    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        todo!()
    }
    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        todo!()
    }

    async fn get_payload_v1(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV1> {
        todo!()
    }

    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV2> {
        todo!()
    }

    async fn get_payload_v3(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV3> {
        todo!()
    }

    async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV4> {
        todo!()
    }

    async fn get_payload_v5(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV5> {
        todo!()
    }

    async fn get_payload_bodies_by_hash_v1(
        &self,
        block_hashes: Vec<BlockHash>,
    ) -> RpcResult<ExecutionPayloadBodiesV1> {
        todo!()
    }

    async fn get_payload_bodies_by_range_v1(
        &self,
        start: U64,
        count: U64,
    ) -> RpcResult<ExecutionPayloadBodiesV1> {
        todo!()
    }

    async fn get_client_version_v1(
        &self,
        client: ClientVersionV1,
    ) -> RpcResult<Vec<ClientVersionV1>> {
        todo!()
    }

    async fn exchange_capabilities(&self, _capabilities: Vec<String>) -> RpcResult<Vec<String>> {
        todo!()
    }

    async fn get_blobs_v1(
        &self,
        versioned_hashes: Vec<B256>,
    ) -> RpcResult<Vec<Option<BlobAndProofV1>>> {
        todo!()
    }

    async fn get_blobs_v2(
        &self,
        versioned_hashes: Vec<B256>,
    ) -> RpcResult<Option<Vec<BlobAndProofV2>>> {
        todo!()
    }
}

impl<Provider, EngineT, Pool, Validator, ChainSpec> IntoEngineApiRpcModule
    for TaikoEngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    EngineT: EngineTypes,
    Self: EngineApiServer<EngineT>,
{
    fn into_rpc_module(self) -> RpcModule<()> {
        self.into_rpc().remove_context()
    }
}
