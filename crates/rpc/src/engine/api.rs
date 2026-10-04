//! Taiko engine API RPC methods and persistence hooks.
use std::{io, sync::Arc};

use alethia_reth_primitives::{
    decode_shasta_proposal_id,
    engine::{osaka::TaikoExecutionPayloadV3, types::TaikoExecutionData},
    payload::attributes::TaikoPayloadAttributes,
};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{B256, BlockNumber, Bytes};
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV5, ForkchoiceState, ForkchoiceUpdated,
    PayloadId, PayloadStatus,
};
use async_trait::async_trait;
use jsonrpsee::{RpcModule, proc_macros::rpc};
use jsonrpsee_core::RpcResult;
use jsonrpsee_types::ErrorObjectOwned;
use reth::{
    payload::PayloadStore, rpc::api::IntoEngineApiRpcModule, transaction_pool::TransactionPool,
};
use reth_db::transaction::DbTx;
use reth_db_api::transaction::DbTxMut;
use reth_engine_primitives::EngineApiValidator;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_node_api::{EngineTypes, PayloadTypes};
use reth_payload_primitives::{EngineApiMessageVersion, EngineObjectValidationError, PayloadKind};
use reth_provider::{
    BalProvider, BlockReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
    StateProviderFactory,
};
use reth_rpc::EngineApi;
use reth_rpc_engine_api::{EngineApiError, EngineCapabilities};

use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_db::model::{
    BatchToLastBlock, STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1Origin,
    StoredL1OriginTable,
};

/// The list of all supported Engine capabilities available over the engine endpoint.
///
/// Per the Engine API spec, `engine_exchangeCapabilities` itself is served but never listed.
pub const TAIKO_ENGINE_CAPABILITIES: &[&str] = &[
    "engine_forkchoiceUpdatedV2",
    "engine_getPayloadV2",
    "engine_newPayloadV2",
    "engine_forkchoiceUpdatedV3",
    "engine_getPayloadV5",
    "engine_newPayloadV4",
];

/// Returns the Engine API capabilities advertised by the Taiko engine endpoint.
pub fn taiko_engine_capabilities() -> EngineCapabilities {
    EngineCapabilities::new(TAIKO_ENGINE_CAPABILITIES.iter().copied())
}

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

    /// Submit an Osaka payload with the standard V4 side parameters.
    #[method(name = "newPayloadV4")]
    async fn new_payload_v4(
        &self,
        payload: TaikoExecutionPayloadV3,
        expected_blob_versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Vec<Bytes>,
    ) -> RpcResult<PayloadStatus>;

    /// Update fork choice and optionally build an Etna payload.
    #[method(name = "forkchoiceUpdatedV3")]
    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<Engine::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated>;

    /// Fetch a previously built Etna payload in an Osaka envelope.
    #[method(name = "getPayloadV5")]
    async fn get_payload_v5(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<Engine::ExecutionPayloadEnvelopeV5>;

    /// Exchange the list of supported Engine API methods with the connected driver.
    #[method(name = "exchangeCapabilities")]
    async fn exchange_capabilities(&self, capabilities: Vec<String>) -> RpcResult<Vec<String>>;
}

/// A concrete implementation of the `TaikoEngineApi` trait.
pub struct TaikoEngineApi<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec> {
    /// Underlying `reth` engine API implementation.
    inner: EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>,
    /// Provider used for DB reads/writes during L1-origin persistence.
    provider: Provider,
    /// Taiko chain spec used to detect Unzen payloads when preparing `getPayloadV2` responses.
    chain_spec: Arc<TaikoChainSpec>,
    /// Payload store used to resolve built payloads by payload ID.
    payload_store: PayloadStore<PayloadT>,
}

impl<Provider, PayloadT: PayloadTypes, Pool, Validator, ChainSpec>
    TaikoEngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>
where
    Provider: HeaderProvider
        + BlockReader
        + DatabaseProviderFactory
        + StateProviderFactory
        + BalProvider
        + 'static,
    PayloadT: PayloadTypes,
    Pool: TransactionPool + 'static,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Creates a new instance of `TaikoEngineApi` with the given parameters.
    pub fn new(
        engine_api: EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>,
        provider: Provider,
        chain_spec: Arc<TaikoChainSpec>,
        payload_store: PayloadStore<PayloadT>,
    ) -> Self
    where
        Provider: Clone,
    {
        Self { inner: engine_api, provider, chain_spec, payload_store }
    }
}

/// Internal helper methods for `TaikoEngineApi`.
impl<Provider, EngineT, Pool, Validator, ChainSpec>
    TaikoEngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    Provider: HeaderProvider
        + BlockReader
        + DatabaseProviderFactory
        + StateProviderFactory
        + BalProvider
        + 'static,
    EngineT: EngineTypes<
            ExecutionData = TaikoExecutionData,
            PayloadAttributes = TaikoPayloadAttributes,
            BuiltPayload = EthBuiltPayload,
            ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2,
            ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV5,
        >,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Updates fork choice through the selected validator and atomically persists successful
    /// builds. Null attributes still reach normal forkchoice validation without a build-version
    /// gate.
    async fn fork_choice_updated(
        &self,
        version: EngineApiMessageVersion,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<TaikoPayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        let (stored_l1_origin, is_preconf_block, batch_id) = match payload_attributes.as_ref() {
            Some(payload) => {
                let batch_id = self
                    .chain_spec
                    .is_shasta_active(payload.payload_attributes.timestamp)
                    .then(|| decode_shasta_proposal_id(payload.block_metadata.extra_data.as_ref()))
                    .flatten();
                (
                    Some(StoredL1Origin::from(&payload.l1_origin)),
                    payload.l1_origin.is_preconf_block(),
                    batch_id,
                )
            }
            None => (None, false, None),
        };

        let status = match version {
            EngineApiMessageVersion::V3 => {
                self.inner.fork_choice_updated_v3(fork_choice_state, payload_attributes).await?
            }
            _ => self.inner.fork_choice_updated_v2(fork_choice_state, payload_attributes).await?,
        };

        // Non-VALID forkchoice outcomes do not start a build and therefore carry no ID.
        // Preserve the upstream status without publishing origin/proposal metadata.
        if !status.payload_status.status.is_valid() {
            return Ok(status);
        }

        if let Some(mut stored_l1_origin) = stored_l1_origin {
            let payload_id = status
                .payload_id
                .ok_or_else(|| Self::internal_error(io::Error::other("missing payload id")))?;

            let built_payload =
                self.wait_for_built_payload(payload_id).await.map_err(ErrorObjectOwned::from)?;

            stored_l1_origin.l2_block_hash = built_payload.block().hash_slow();

            self.persist_l1_origin(stored_l1_origin, is_preconf_block, batch_id)
                .map_err(ErrorObjectOwned::from)?;
        }

        Ok(status)
    }

    /// Convenience helper to wrap an internal error, preserving the original message.
    fn internal_error<E>(err: E) -> EngineApiError
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        EngineApiError::Internal(Box::new(err))
    }

    /// Converts a built payload into the standard V2 envelope, preserving the builder fee unless
    /// Unzen requires the hash-relevant header difficulty to be carried through `blockValue`.
    fn convert_built_payload_to_execution_payload_envelope_v2(
        &self,
        built_payload: EthBuiltPayload,
    ) -> ExecutionPayloadEnvelopeV2 {
        convert_built_payload_to_execution_payload_envelope_v2(
            self.chain_spec.as_ref(),
            built_payload,
        )
    }

    /// Resolves a stored payload, preserving builder errors and reporting unknown IDs distinctly.
    async fn wait_for_built_payload(
        &self,
        payload_id: PayloadId,
    ) -> Result<EngineT::BuiltPayload, EngineApiError> {
        // Leverage the payload builder's own resolution path instead of manual polling.
        match self.payload_store.resolve_kind(payload_id, PayloadKind::WaitForPending).await {
            Some(Ok(payload)) => Ok(payload),
            Some(Err(error)) => Err(EngineApiError::GetPayloadError(error)),
            None => Err(EngineApiError::UnknownPayload),
        }
    }

    /// Persists the L1 origin for the given built payload in a single transaction, updating the
    /// head pointer when the block is not pre-confirmation.
    fn persist_l1_origin(
        &self,
        stored_l1_origin: StoredL1Origin,
        is_preconf_block: bool,
        batch_id: Option<u64>,
    ) -> Result<(), EngineApiError> {
        let tx = self.provider.database_provider_rw().map_err(Self::internal_error)?.into_tx();

        let block_number = stored_l1_origin.block_id.to::<BlockNumber>();

        tx.put::<StoredL1OriginTable>(block_number, stored_l1_origin)
            .map_err(Self::internal_error)?;

        if !is_preconf_block {
            tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, block_number)
                .map_err(Self::internal_error)?;

            if let Some(batch_id) = batch_id {
                tx.put::<BatchToLastBlock>(batch_id, block_number).map_err(Self::internal_error)?;
            }
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
    Provider: HeaderProvider
        + BlockReader
        + DatabaseProviderFactory
        + StateProviderFactory
        + BalProvider
        + 'static,
    EngineT: EngineTypes<
            ExecutionData = TaikoExecutionData,
            PayloadAttributes = TaikoPayloadAttributes,
            BuiltPayload = EthBuiltPayload,
            ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2,
            ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV5,
        >,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Creates a new execution payload with the given execution data.
    async fn new_payload_v2(&self, payload: TaikoExecutionData) -> RpcResult<PayloadStatus> {
        validate_taiko_api_fork(
            self.chain_spec.is_etna_active(payload.execution_payload.timestamp),
            false,
        )?;
        self.inner.new_payload_v2(payload).await.map_err(|e| e.into())
    }

    /// Updates the fork choice with the given state and payload attributes.
    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        self.fork_choice_updated(EngineApiMessageVersion::V2, fork_choice_state, payload_attributes)
            .await
    }

    /// Updates fork choice with Etna build attributes while retaining the shared origin
    /// transaction.
    async fn fork_choice_updated_v3(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        self.fork_choice_updated(EngineApiMessageVersion::V3, fork_choice_state, payload_attributes)
            .await
    }

    /// Normalizes all four wire arguments before forwarding the internal data to Reth.
    async fn new_payload_v4(
        &self,
        payload: TaikoExecutionPayloadV3,
        expected_blob_versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
        execution_requests: Vec<Bytes>,
    ) -> RpcResult<PayloadStatus> {
        validate_taiko_api_fork(
            self.chain_spec
                .is_etna_active(payload.execution_payload.payload_inner.payload_inner.timestamp),
            true,
        )?;
        let data = payload
            .into_execution_data(
                expected_blob_versioned_hashes,
                parent_beacon_block_root,
                execution_requests,
            )
            .map_err(|error| {
                EngineApiError::from(EngineObjectValidationError::InvalidParams(Box::new(error)))
            })?;
        self.inner.new_payload_v4(data).await.map_err(Into::into)
    }

    /// Resolves the stored job before gating its timestamp and converting the RPC envelope.
    async fn get_payload_v5(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV5> {
        let built_payload =
            self.wait_for_built_payload(payload_id).await.map_err(ErrorObjectOwned::from)?;
        validate_taiko_api_fork(
            self.chain_spec.is_etna_active(built_payload.block().timestamp),
            true,
        )?;
        convert_built_payload_to_execution_payload_envelope_v5(built_payload).map_err(Into::into)
    }

    /// Retrieves the execution payload by its ID.
    async fn get_payload_v2(
        &self,
        payload_id: PayloadId,
    ) -> RpcResult<EngineT::ExecutionPayloadEnvelopeV2> {
        let built_payload =
            self.wait_for_built_payload(payload_id).await.map_err(ErrorObjectOwned::from)?;
        validate_taiko_api_fork(
            self.chain_spec.is_etna_active(built_payload.block().timestamp),
            false,
        )?;
        Ok(self.convert_built_payload_to_execution_payload_envelope_v2(built_payload))
    }

    /// Exchanges supported Engine API methods with the driver, returning this node's list.
    async fn exchange_capabilities(&self, capabilities: Vec<String>) -> RpcResult<Vec<String>> {
        let el_capabilities = self.inner.capabilities();
        el_capabilities.log_capability_mismatches(&capabilities);
        Ok(el_capabilities.list())
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

/// Rejects Engine method families that do not match Taiko Etna at the target timestamp.
fn validate_taiko_api_fork(is_etna_active: bool, wants_etna: bool) -> Result<(), EngineApiError> {
    if is_etna_active != wants_etna {
        return Err(EngineObjectValidationError::UnsupportedFork.into());
    }
    Ok(())
}

/// Converts an Osaka payload while exposing finalized zk-gas as blockValue and preserving fees
/// in the builder's stored payload. Unsupported sidecar conversions propagate without panicking.
fn convert_built_payload_to_execution_payload_envelope_v5(
    built_payload: EthBuiltPayload,
) -> Result<ExecutionPayloadEnvelopeV5, EngineApiError> {
    let difficulty = built_payload.block().difficulty;
    let mut envelope =
        built_payload.try_into_v5().map_err(|error| EngineApiError::Internal(Box::new(error)))?;
    envelope.block_value = difficulty;
    envelope.should_override_builder = false;
    Ok(envelope)
}

/// Converts a built payload into the standard V2 execution payload envelope.
///
/// Unzen reuses `blockValue` to transport the hash-relevant header difficulty through the standard
/// `getPayloadV2` response shape without adding a new wire field.
fn convert_built_payload_to_execution_payload_envelope_v2(
    chain_spec: &TaikoChainSpec,
    built_payload: EthBuiltPayload,
) -> ExecutionPayloadEnvelopeV2 {
    let block = built_payload.block();
    let is_unzen_active = chain_spec.is_unzen_active(block.header().timestamp);
    let header_difficulty = block.header().difficulty;
    let mut envelope = ExecutionPayloadEnvelopeV2::from(built_payload);

    if is_unzen_active {
        // Consensus rule: Taiko Unzen round-trips the header difficulty through `blockValue` so
        // the RPC response can carry the hash-relevant field without introducing a new wire field.
        envelope.block_value = header_difficulty;
    }

    envelope
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_node_api::PayloadBuilderError;

    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alloy_consensus::{BlockBody, Header, constants::EMPTY_WITHDRAWALS};
    use alloy_eips::merge::BEACON_NONCE;
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use std::sync::Arc;

    fn test_provider()
    -> reth_provider::ProviderFactory<reth_provider::test_utils::MockNodeTypesWithDB> {
        use reth_db::{
            ClientVersion, TableSet, Tables,
            mdbx::{DatabaseArguments, init_db_for},
            table::TableInfo,
            test_utils::{
                TempDatabase, create_test_rocksdb_dir, create_test_static_files_dir, tempdir_path,
            },
        };
        use reth_provider::providers::{RocksDBBuilder, StaticFileProvider};
        struct TaikoTables;
        impl TableSet for TaikoTables {
            fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
                Box::new(
                    Tables::ALL.iter().map(|t| Box::new(*t) as Box<dyn TableInfo>).chain(
                        alethia_reth_db::model::Tables::ALL
                            .iter()
                            .map(|t| Box::new(*t) as Box<dyn TableInfo>),
                    ),
                )
            }
        }
        let (static_dir, _) = create_test_static_files_dir();
        let (rocks_dir, _) = create_test_rocksdb_dir();
        let path = tempdir_path();
        let db = init_db_for::<_, TaikoTables>(
            path.clone(),
            DatabaseArguments::new(ClientVersion::default()),
        )
        .unwrap();
        reth_provider::ProviderFactory::new(
            Arc::new(TempDatabase::new(db, path)),
            reth_ethereum::chainspec::MAINNET.clone(),
            StaticFileProvider::read_write(static_dir.keep()).unwrap(),
            RocksDBBuilder::new(&rocks_dir).with_default_tables().build().unwrap(),
            reth::tasks::Runtime::test(),
        )
        .unwrap()
    }

    fn rpc_fixture() -> (
        RpcModule<()>,
        reth_provider::ProviderFactory<reth_provider::test_utils::MockNodeTypesWithDB>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use alethia_reth_primitives::engine::TaikoEngineTypes;
        use alloy_rpc_types_engine::{ClientCode, ClientVersionV1, PayloadStatusEnum};
        use reth::payload::{PayloadBuilderHandle, PayloadServiceCommand};
        use reth_engine_primitives::{
            BeaconEngineMessage, ConsensusEngineHandle, OnForkChoiceUpdated,
        };
        let provider = test_provider();
        let mut spec = (*unzen_chain_spec()).clone();
        spec.inner.hardforks.insert(TaikoHardfork::Etna, ForkCondition::Timestamp(100));
        spec.inner.hardforks.insert(TaikoHardfork::Shasta, ForkCondition::Timestamp(0));
        let spec = Arc::new(spec);
        let api_spec = spec.clone();
        let (payload_tx, mut payload_rx) =
            tokio::sync::mpsc::unbounded_channel::<PayloadServiceCommand<TaikoEngineTypes>>();
        // The store adapter supplies deterministic already-built jobs. Real wrapper routing,
        // normalization and database transactions are exercised around the adapter.
        tokio::spawn(async move {
            while let Some(command) = payload_rx.recv().await {
                match command {
                    PayloadServiceCommand::Resolve(id, _, tx) => {
                        let timestamp = if id == PayloadId::new([99; 8]) {
                            Some(99)
                        } else if id == PayloadId::new([100; 8]) {
                            Some(100)
                        } else {
                            None
                        };
                        let future = timestamp.map(|timestamp| {
                            let mut block = sample_unzen_block(U256::from(7), timestamp);
                            block.header.blob_gas_used = Some(0);
                            block.header.excess_blob_gas = Some(0);
                            block.header.requests_hash =
                                Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH);
                            if timestamp == 100 {
                                block.header.parent_beacon_block_root =
                                    Some(B256::with_last_byte(42));
                            }
                            let built = EthBuiltPayload::new(
                                Arc::new(reth_primitives_traits::RecoveredBlock::new_unhashed(
                                    block,
                                    vec![],
                                )),
                                U256::from(999),
                                None,
                                None,
                            );
                            Box::pin(async move { Ok(built) })
                                as std::pin::Pin<
                                    Box<
                                        dyn std::future::Future<
                                                Output = Result<
                                                    EthBuiltPayload,
                                                    PayloadBuilderError,
                                                >,
                                            > + Send,
                                    >,
                                >
                        });
                        tx.send(future).ok();
                    }
                    _ => panic!("unexpected payload service operation"),
                }
            }
        });
        let payload_handle = PayloadBuilderHandle::new(payload_tx);
        let store = PayloadStore::new(payload_handle.clone());
        let (engine_tx, mut engine_rx) =
            tokio::sync::mpsc::unbounded_channel::<BeaconEngineMessage<TaikoEngineTypes>>();
        let jobs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let jobs_clone = jobs.clone();
        tokio::spawn(async move {
            while let Some(message) = engine_rx.recv().await {
                match message {
                    BeaconEngineMessage::ForkchoiceUpdated { state, payload_attrs, tx } => {
                        let status = PayloadStatus::from_status(PayloadStatusEnum::Valid);
                        let response = if state.head_block_hash.is_zero() {
                            OnForkChoiceUpdated::invalid_state()
                        } else if state.head_block_hash == B256::with_last_byte(200) {
                            OnForkChoiceUpdated::valid(PayloadStatus::from_status(
                                PayloadStatusEnum::Syncing,
                            ))
                        } else if state.head_block_hash == B256::with_last_byte(201) {
                            OnForkChoiceUpdated::valid(PayloadStatus::from_status(
                                PayloadStatusEnum::Invalid {
                                    validation_error: "invalid ancestor".into(),
                                },
                            ))
                        } else if state.head_block_hash == B256::with_last_byte(202) {
                            OnForkChoiceUpdated::valid(status)
                        } else if let Some(attrs) = payload_attrs {
                            jobs_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let (id_tx, id_rx) = tokio::sync::oneshot::channel();
                            id_tx
                                .send(Ok(PayloadId::new(
                                    [attrs.payload_attributes.timestamp as u8; 8],
                                )))
                                .unwrap();
                            OnForkChoiceUpdated::updated_with_pending_payload_id(status, id_rx)
                        } else {
                            OnForkChoiceUpdated::valid(status)
                        };
                        tx.send(Ok(response)).ok();
                    }
                    BeaconEngineMessage::NewPayload { payload, tx } => {
                        let result = <crate::engine::validator::TaikoEngineValidator as reth_node_api::PayloadValidator<TaikoEngineTypes>>::convert_payload_to_block(
                            &crate::engine::validator::TaikoEngineValidator::new(spec.clone()), payload,
                        );
                        let status = match result {
                            Ok(_) => PayloadStatusEnum::Valid,
                            Err(err) => {
                                PayloadStatusEnum::Invalid { validation_error: err.to_string() }
                            }
                        };
                        tx.send(Ok(PayloadStatus::from_status(status))).ok();
                    }
                    _ => panic!("unexpected engine operation"),
                }
            }
        });
        let blockchain = reth_provider::providers::BlockchainProvider::with_latest(
            provider.clone(),
            reth_primitives_traits::SealedHeader::seal_slow(Header::default()),
        )
        .unwrap();
        let inner = EngineApi::new(
            blockchain.clone(),
            api_spec.clone(),
            ConsensusEngineHandle::new(engine_tx),
            PayloadStore::new(payload_handle),
            reth::transaction_pool::noop::NoopTransactionPool::default(),
            reth::tasks::Runtime::test(),
            ClientVersionV1 {
                code: ClientCode::RH,
                name: "test".into(),
                version: "test".into(),
                commit: "test".into(),
            },
            taiko_engine_capabilities(),
            crate::engine::validator::TaikoEngineValidator::new(api_spec.clone()),
            false,
            reth::network::noop::NoopNetwork::default(),
        );
        (TaikoEngineApi::new(inner, blockchain, api_spec, store).into_rpc_module(), provider, jobs)
    }

    async fn rpc_call(
        module: &RpcModule<()>,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let request =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
                .to_string();
        let (response, _) = module.raw_json_request(&request, 1).await.unwrap();
        serde_json::from_str(response.get()).unwrap()
    }

    fn fcu_state(head: u8) -> ForkchoiceState {
        ForkchoiceState {
            head_block_hash: B256::with_last_byte(head),
            safe_block_hash: B256::ZERO,
            finalized_block_hash: B256::ZERO,
        }
    }

    #[tokio::test]
    async fn registered_methods_and_get_payload_fork_matrix() {
        let (module, _, _) = rpc_fixture();
        let mut methods = module.method_names().collect::<Vec<_>>();
        methods.sort();
        assert_eq!(
            methods,
            vec![
                "engine_exchangeCapabilities",
                "engine_forkchoiceUpdatedV2",
                "engine_forkchoiceUpdatedV3",
                "engine_getPayloadV2",
                "engine_getPayloadV5",
                "engine_newPayloadV2",
                "engine_newPayloadV4"
            ]
        );
        for timestamp in [99, 100] {
            // Simulate a changed head/reorg; routing must continue using the stored job.
            let changed = rpc_call(
                &module,
                "engine_forkchoiceUpdatedV2",
                serde_json::json!([fcu_state(200 - timestamp), null]),
            )
            .await;
            assert!(changed.get("result").is_some(), "{changed}");
            for method in ["engine_getPayloadV2", "engine_getPayloadV5"] {
                let response =
                    rpc_call(&module, method, serde_json::json!([PayloadId::new([timestamp; 8])]))
                        .await;
                if (method == "engine_getPayloadV2") == (timestamp == 99) {
                    assert_eq!(
                        response["result"]["executionPayload"]["timestamp"],
                        format!("0x{timestamp:x}")
                    );
                    assert_eq!(response["result"]["blockValue"], "0x7");
                    if timestamp == 100 {
                        assert_eq!(response["result"]["executionPayload"]["blobGasUsed"], "0x0");
                        assert_eq!(response["result"]["executionPayload"]["excessBlobGas"], "0x0");
                        assert_eq!(
                            response["result"]["executionPayload"]["withdrawals"],
                            serde_json::json!([])
                        );
                        assert_eq!(response["result"]["executionRequests"], serde_json::json!([]));
                        assert_eq!(
                            response["result"]["blobsBundle"],
                            serde_json::json!({"commitments": [], "proofs": [], "blobs": []})
                        );
                        assert_eq!(response["result"]["shouldOverrideBuilder"], false);
                    }
                } else {
                    assert_eq!(response["error"]["code"], -38005, "{response}");
                }
            }
        }
        for method in ["engine_getPayloadV2", "engine_getPayloadV5"] {
            let response =
                rpc_call(&module, method, serde_json::json!([PayloadId::new([0; 8])])).await;
            assert_eq!(response["error"]["code"], -38001);
            assert_eq!(response["error"]["message"], "Unknown payload");
        }
    }

    #[tokio::test]
    async fn both_fcu_versions_allow_null_attributes_but_validate_forkchoice() {
        let (module, _, jobs) = rpc_fixture();
        for method in ["engine_forkchoiceUpdatedV2", "engine_forkchoiceUpdatedV3"] {
            let response = rpc_call(&module, method, serde_json::json!([fcu_state(1), null])).await;
            assert_eq!(response["result"]["payloadStatus"]["status"], "VALID", "{response}");
            let response = rpc_call(&module, method, serde_json::json!([fcu_state(0), null])).await;
            assert_eq!(response["error"]["code"], -38002, "{response}");
        }
        assert_eq!(jobs.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    fn fcu_attributes(timestamp: u64, preconf: bool) -> TaikoPayloadAttributes {
        use alethia_reth_primitives::payload::attributes::{RpcL1Origin, TaikoBlockMetadata};
        TaikoPayloadAttributes {
            payload_attributes: alloy_rpc_types_engine::PayloadAttributes {
                timestamp,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Some(vec![]),
                parent_beacon_block_root: Some(if timestamp >= 100 {
                    B256::with_last_byte(42)
                } else {
                    B256::ZERO
                }),
                slot_number: None,
                target_gas_limit: None,
            },
            base_fee_per_gas: U256::from(1),
            block_metadata: TaikoBlockMetadata {
                timestamp: U256::from(timestamp),
                gas_limit: 30_000_000,
                extra_data: if timestamp >= 100 {
                    Bytes::from(vec![0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 1])
                } else {
                    Bytes::from(vec![0, 0, 0, 0, 0, 0, 9])
                },
                ..Default::default()
            },
            l1_origin: RpcL1Origin {
                block_id: U256::from(timestamp),
                l2_block_hash: B256::ZERO,
                l1_block_height: (!preconf).then_some(U256::from(1)),
                l1_block_hash: (!preconf).then_some(B256::with_last_byte(1)),
                build_payload_args_id: [0; 8],
                is_forced_inclusion: false,
                signature: [0; 65],
            },
            anchor_transaction: None,
        }
    }

    #[tokio::test]
    async fn fcu_fork_matrix_and_origin_persistence_parity() {
        for timestamp in [99, 100] {
            for method in ["engine_forkchoiceUpdatedV2", "engine_forkchoiceUpdatedV3"] {
                for preconf in [false, true] {
                    let (module, provider, jobs) = rpc_fixture();
                    let attrs = fcu_attributes(timestamp, preconf);
                    let response =
                        rpc_call(&module, method, serde_json::json!([fcu_state(1), attrs])).await;
                    let valid = (timestamp == 99) == (method == "engine_forkchoiceUpdatedV2");
                    if valid {
                        assert!(response.get("result").is_some(), "{response}");
                    } else {
                        assert_eq!(response["error"]["code"], -38005, "{response}");
                    }
                    assert_eq!(jobs.load(std::sync::atomic::Ordering::SeqCst), usize::from(valid));
                    let db = provider.provider().unwrap();
                    let origin = db.tx_ref().get::<StoredL1OriginTable>(timestamp).unwrap();
                    assert_eq!(origin.is_some(), valid);
                    if let Some(origin) = origin {
                        assert_eq!(origin.block_id, U256::from(timestamp));
                        assert!(!origin.l2_block_hash.is_zero());
                    }
                    assert_eq!(
                        db.tx_ref()
                            .get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)
                            .unwrap(),
                        (valid && !preconf).then_some(timestamp)
                    );
                    let proposal_id = decode_shasta_proposal_id(
                        fcu_attributes(timestamp, preconf).block_metadata.extra_data.as_ref(),
                    )
                    .unwrap();
                    assert_eq!(
                        db.tx_ref().get::<BatchToLastBlock>(proposal_id).unwrap(),
                        (valid && !preconf).then_some(timestamp)
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn nonvalid_fcu_statuses_do_not_require_payload_ids_or_write_origins() {
        for (method, timestamp) in
            [("engine_forkchoiceUpdatedV2", 99), ("engine_forkchoiceUpdatedV3", 100)]
        {
            for (head, expected) in [(200, "SYNCING"), (201, "INVALID"), (202, "VALID")] {
                let (module, provider, jobs) = rpc_fixture();
                let response = rpc_call(
                    &module,
                    method,
                    serde_json::json!([fcu_state(head), fcu_attributes(timestamp, false)]),
                )
                .await;
                if expected == "VALID" {
                    assert_eq!(response["error"]["code"], -32603, "{response}");
                } else {
                    assert_eq!(
                        response["result"]["payloadStatus"]["status"], expected,
                        "{response}"
                    );
                    assert!(response["result"]["payloadId"].is_null());
                }
                assert_eq!(jobs.load(std::sync::atomic::Ordering::SeqCst), 0);
                let db = provider.provider().unwrap();
                assert_eq!(db.tx_ref().get::<StoredL1OriginTable>(timestamp).unwrap(), None);
                assert_eq!(
                    db.tx_ref().get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY).unwrap(),
                    None
                );
                assert_eq!(db.tx_ref().get::<BatchToLastBlock>(9).unwrap(), None);
            }
        }
    }

    #[tokio::test]
    async fn invalid_etna_attributes_never_start_jobs_or_publish_origins() {
        let (module, provider, jobs) = rpc_fixture();
        for case in 0..7 {
            let mut attrs = fcu_attributes(100, false);
            match case {
                0 => attrs.payload_attributes.parent_beacon_block_root = None,
                1 => attrs.payload_attributes.parent_beacon_block_root = Some(B256::ZERO),
                2 => attrs.anchor_transaction = Some(Bytes::new()),
                3 => attrs.block_metadata.timestamp = U256::from(99),
                4 => attrs.block_metadata.extra_data = Bytes::from(vec![0, 0, 0, 0, 0, 0, 9]),
                5 => attrs.payload_attributes.slot_number = Some(1),
                _ => attrs.payload_attributes.target_gas_limit = Some(1),
            }
            let response = rpc_call(
                &module,
                "engine_forkchoiceUpdatedV3",
                serde_json::json!([fcu_state(1), attrs]),
            )
            .await;
            assert!(response.get("error").is_some(), "case {case}: {response}");
        }
        assert_eq!(jobs.load(std::sync::atomic::Ordering::SeqCst), 0);
        let db = provider.provider().unwrap();
        assert_eq!(db.tx_ref().get::<StoredL1OriginTable>(100).unwrap(), None);
        assert_eq!(
            db.tx_ref().get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY).unwrap(),
            None
        );
        assert_eq!(db.tx_ref().get::<BatchToLastBlock>(9).unwrap(), None);
    }

    #[tokio::test]
    async fn new_payload_four_argument_normalization_and_fork_matrix() {
        use alethia_reth_primitives::engine::{TaikoEngineTypes, osaka::TaikoExecutionPayloadV3};
        let (module, _, _) = rpc_fixture();
        for timestamp in [99, 100] {
            let mut block = sample_unzen_block(U256::ZERO, timestamp);
            block.header.blob_gas_used = Some(0);
            block.header.excess_blob_gas = Some(0);
            block.header.requests_hash = Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH);
            let root = if timestamp == 100 { B256::with_last_byte(42) } else { B256::ZERO };
            block.header.parent_beacon_block_root = Some(root);
            let wire = TaikoExecutionPayloadV3 {
                execution_payload: alloy_rpc_types_engine::ExecutionPayloadV3::from_block_unchecked(
                    block.header.hash_slow(),
                    &block,
                ),
                header_difficulty: 0,
                extra_fields: Default::default(),
            };
            let legacy = TaikoEngineTypes::block_to_payload(
                reth_primitives_traits::SealedBlock::new_unhashed(block),
                None,
            );
            for method in ["engine_newPayloadV2", "engine_newPayloadV4"] {
                let params = if method == "engine_newPayloadV2" {
                    serde_json::json!([legacy])
                } else {
                    serde_json::json!([wire, [], root, []])
                };
                let response = rpc_call(&module, method, params).await;
                if (timestamp == 99) == (method == "engine_newPayloadV2") {
                    assert_eq!(response["result"]["status"], "VALID", "{response}");
                } else {
                    assert_eq!(response["error"]["code"], -38005, "{response}");
                }
            }
            if timestamp == 100 {
                for side in 0..3 {
                    let params = match side {
                        0 => serde_json::json!([wire, [B256::with_last_byte(1)], root, []]),
                        1 => serde_json::json!([wire, [], B256::ZERO, []]),
                        _ => serde_json::json!([wire, [], root, ["0x01"]]),
                    };
                    assert!(
                        rpc_call(&module, "engine_newPayloadV4", params)
                            .await
                            .get("error")
                            .is_some()
                    );
                }
                for field in ["headerDifficulty", "transactions"] {
                    for null in [true, false] {
                        let mut json = serde_json::to_value(&wire).unwrap();
                        if null {
                            json[field] = serde_json::Value::Null;
                        } else {
                            json.as_object_mut().unwrap().remove(field);
                        }
                        let response = rpc_call(
                            &module,
                            "engine_newPayloadV4",
                            serde_json::json!([json, [], root, []]),
                        )
                        .await;
                        assert_eq!(response["error"]["code"], -32602, "{response}");
                    }
                }
                for field in
                    ["txHash", "withdrawalsHash", "blockAccessList", "slotNumber", "targetGasLimit"]
                {
                    let mut json = serde_json::to_value(&wire).unwrap();
                    json[field] = serde_json::json!("0x01");
                    assert!(
                        rpc_call(
                            &module,
                            "engine_newPayloadV4",
                            serde_json::json!([json, [], root, []])
                        )
                        .await
                        .get("error")
                        .is_some()
                    );
                }
            }
        }
    }

    #[test]
    fn v5_conversion_preserves_builder_fees_and_propagates_sidecar_errors() {
        let built = sample_built_payload(U256::from(7), U256::from(999), 100);
        let envelope =
            convert_built_payload_to_execution_payload_envelope_v5(built.clone()).unwrap();
        assert_eq!(envelope.block_value, U256::from(7));
        assert_eq!(built.fees(), U256::from(999));
        // EIP-4844 sidecars cannot convert to Osaka V2 blobs bundles.
        let unsupported =
            built.with_sidecars(vec![alloy_eips::eip4844::BlobTransactionSidecar::default()]);
        assert!(convert_built_payload_to_execution_payload_envelope_v5(unsupported).is_err());
    }

    #[test]
    fn engine_capabilities_advertise_exactly_the_served_methods() {
        let mut capabilities = taiko_engine_capabilities().list();
        capabilities.sort();

        // Exactly the methods routed by `TaikoEngineApiServer::into_rpc`; per the Engine API
        // spec, `engine_exchangeCapabilities` itself must not be part of the list.
        assert_eq!(
            capabilities,
            vec![
                "engine_forkchoiceUpdatedV2",
                "engine_forkchoiceUpdatedV3",
                "engine_getPayloadV2",
                "engine_getPayloadV5",
                "engine_newPayloadV2",
                "engine_newPayloadV4"
            ]
        );
    }

    #[test]
    fn unzen_payload_overwrites_block_value_with_header_difficulty() {
        let chain_spec = unzen_chain_spec();
        let built_payload = sample_built_payload(U256::from(7_u64), U256::from(1_u64), 1);

        let envelope = convert_built_payload_to_execution_payload_envelope_v2(
            chain_spec.as_ref(),
            built_payload,
        );

        assert_eq!(envelope.block_value, U256::from(7_u64));
    }

    #[test]
    fn pre_unzen_payload_preserves_original_block_value() {
        let chain_spec = pre_unzen_chain_spec();
        let built_payload = sample_built_payload(U256::from(7_u64), U256::from(1_u64), 1);

        let envelope = convert_built_payload_to_execution_payload_envelope_v2(
            chain_spec.as_ref(),
            built_payload,
        );

        assert_eq!(envelope.block_value, U256::from(1_u64));
    }

    fn unzen_chain_spec() -> Arc<alethia_reth_chainspec::spec::TaikoChainSpec> {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(0));
        Arc::new(chain_spec)
    }

    fn pre_unzen_chain_spec() -> Arc<alethia_reth_chainspec::spec::TaikoChainSpec> {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(10));
        Arc::new(chain_spec)
    }

    fn sample_built_payload(difficulty: U256, fees: U256, timestamp: u64) -> EthBuiltPayload {
        let block = sample_unzen_block(difficulty, timestamp);
        let recovered_block =
            Arc::new(reth_primitives_traits::RecoveredBlock::new_unhashed(block, Vec::new()));

        EthBuiltPayload::new(recovered_block, fees, None, None)
    }

    fn sample_unzen_block(difficulty: U256, timestamp: u64) -> reth_ethereum::Block {
        reth_ethereum::Block {
            header: Header {
                parent_hash: B256::with_last_byte(0x11),
                beneficiary: Address::with_last_byte(0x22),
                state_root: B256::with_last_byte(0x33),
                transactions_root: alloy_consensus::proofs::calculate_transaction_root(&Vec::<
                    reth_ethereum::TransactionSigned,
                >::new(
                )),
                receipts_root: B256::with_last_byte(0x44),
                withdrawals_root: Some(EMPTY_WITHDRAWALS),
                logs_bloom: Default::default(),
                number: 1,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp,
                mix_hash: B256::with_last_byte(0x55),
                nonce: BEACON_NONCE.into(),
                base_fee_per_gas: Some(1),
                extra_data: Bytes::default(),
                difficulty,
                parent_beacon_block_root: Some(B256::ZERO),
                requests_hash: None,
                ..Default::default()
            },
            body: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
        }
    }
}
