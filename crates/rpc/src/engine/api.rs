//! Taiko engine API RPC methods and persistence hooks.
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

use alethia_reth_primitives::{
    engine::types::TaikoExecutionData, payload::attributes::TaikoPayloadAttributes,
};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{B256, BlockNumber, U256};
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ForkchoiceState, ForkchoiceUpdated, PayloadId, PayloadStatus,
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
use reth_node_api::{EngineTypes, PayloadBuilderError, PayloadTypes};
use reth_payload_primitives::PayloadKind;
use reth_provider::{
    BlockReader, DBProvider, DatabaseProviderFactory, HeaderProvider, StateProviderFactory,
};
use reth_rpc::EngineApi;
use reth_rpc_engine_api::EngineApiError;

use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
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
    /// Taiko chain spec used to detect Uzen payloads when preparing `getPayloadV2` responses.
    chain_spec: Arc<TaikoChainSpec>,
    /// Payload store used to resolve built payloads by payload ID.
    payload_store: PayloadStore<PayloadT>,
    /// Cache of hash-relevant header fields that `ExecutionPayloadV1` does not carry.
    built_payload_headers: Arc<Mutex<HashMap<B256, U256>>>,
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
        chain_spec: Arc<TaikoChainSpec>,
        payload_store: PayloadStore<PayloadT>,
    ) -> Self
    where
        Provider: Clone,
    {
        Self {
            inner: engine_api,
            provider,
            chain_spec,
            payload_store,
            built_payload_headers: Default::default(),
        }
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
            BuiltPayload = EthBuiltPayload,
            ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2,
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

    /// Stores the built block's hash-relevant header fields for later `newPayload` hydration.
    fn cache_built_payload_header(&self, built_payload: &EthBuiltPayload) {
        let mut built_payload_headers = self
            .built_payload_headers
            .lock()
            .expect("built payload header cache mutex should not be poisoned");
        cache_built_payload_header_in_map(&mut built_payload_headers, built_payload);
    }

    /// Converts a built payload into the standard V2 envelope, preserving the builder fee unless
    /// Uzen requires the hash-relevant header difficulty to be carried through `blockValue`.
    fn convert_built_payload_to_execution_payload_envelope_v2(
        &self,
        built_payload: EthBuiltPayload,
    ) -> ExecutionPayloadEnvelopeV2 {
        convert_built_payload_to_execution_payload_envelope_v2(
            self.chain_spec.as_ref(),
            built_payload,
        )
    }

    /// Restores cached header fields that are omitted by the external payload wire format.
    fn hydrate_cached_header_fields(&self, payload: &mut TaikoExecutionData) {
        let built_payload_headers = self
            .built_payload_headers
            .lock()
            .expect("built payload header cache mutex should not be poisoned");
        hydrate_cached_header_fields_from_map(&built_payload_headers, payload);
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
            BuiltPayload = EthBuiltPayload,
            ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2,
        >,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    /// Creates a new execution payload with the given execution data.
    async fn new_payload_v2(&self, mut payload: TaikoExecutionData) -> RpcResult<PayloadStatus> {
        self.hydrate_cached_header_fields(&mut payload);
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
            self.cache_built_payload_header(&built_payload);

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
        let built_payload =
            self.wait_for_built_payload(payload_id).await.map_err(ErrorObjectOwned::from)?;
        self.cache_built_payload_header(&built_payload);
        Ok(self.convert_built_payload_to_execution_payload_envelope_v2(built_payload))
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

/// Converts a built payload into the standard V2 execution payload envelope.
///
/// Uzen reuses `blockValue` to transport the hash-relevant header difficulty through the standard
/// `getPayloadV2` response shape without adding a new wire field.
fn convert_built_payload_to_execution_payload_envelope_v2(
    chain_spec: &TaikoChainSpec,
    built_payload: EthBuiltPayload,
) -> ExecutionPayloadEnvelopeV2 {
    let block = built_payload.block();
    let is_uzen_active = chain_spec.is_uzen_active(block.header().timestamp);
    let header_difficulty = block.header().difficulty;
    let mut envelope = ExecutionPayloadEnvelopeV2::from(built_payload);

    if is_uzen_active {
        // Consensus rule: Taiko Uzen round-trips the header difficulty through `blockValue` so
        // the RPC response can carry the hash-relevant field without introducing a new wire field.
        envelope.block_value = header_difficulty;
    }

    envelope
}

/// Stores the header difficulty for a built payload in the supplied cache.
fn cache_built_payload_header_in_map(
    built_payload_headers: &mut HashMap<B256, U256>,
    built_payload: &EthBuiltPayload,
) {
    let block = built_payload.block();
    built_payload_headers.insert(block.hash(), block.header().difficulty);
}

/// Restores a cached header difficulty for a payload that lost it on the wire.
fn hydrate_cached_header_fields_from_map(
    built_payload_headers: &HashMap<B256, U256>,
    payload: &mut TaikoExecutionData,
) {
    if payload.taiko_sidecar.header_difficulty.is_some() {
        return;
    }

    payload.taiko_sidecar.header_difficulty =
        built_payload_headers.get(&payload.execution_payload.block_hash).copied();
}

#[cfg(test)]
mod tests {
    use super::*;

    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alethia_reth_primitives::engine::types::TaikoExecutionDataSidecar;
    use alloy_consensus::{BlockBody, Header, constants::EMPTY_WITHDRAWALS};
    use alloy_eips::merge::BEACON_NONCE;
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use reth_primitives_traits::{Block as _, BlockBody as _};
    use std::{collections::HashMap, sync::Arc};

    #[test]
    fn uzen_payload_overwrites_block_value_with_header_difficulty() {
        let chain_spec = uzen_chain_spec();
        let built_payload = sample_built_payload(U256::from(7_u64), U256::from(1_u64), 1);

        let envelope = convert_built_payload_to_execution_payload_envelope_v2(
            chain_spec.as_ref(),
            built_payload,
        );

        assert_eq!(envelope.block_value, U256::from(7_u64));
    }

    #[test]
    fn pre_uzen_payload_preserves_original_block_value() {
        let chain_spec = pre_uzen_chain_spec();
        let built_payload = sample_built_payload(U256::from(7_u64), U256::from(1_u64), 1);

        let envelope = convert_built_payload_to_execution_payload_envelope_v2(
            chain_spec.as_ref(),
            built_payload,
        );

        assert_eq!(envelope.block_value, U256::from(1_u64));
    }

    #[test]
    fn get_payload_v2_cache_still_hydrates_new_payload_v2_for_uzen() {
        let chain_spec = uzen_chain_spec();
        let built_payload = sample_built_payload(U256::from(7_u64), U256::from(1_u64), 1);
        let mut built_payload_headers = HashMap::new();

        let envelope = convert_built_payload_to_execution_payload_envelope_v2(
            chain_spec.as_ref(),
            built_payload.clone(),
        );
        assert_eq!(envelope.block_value, U256::from(7_u64));

        cache_built_payload_header_in_map(&mut built_payload_headers, &built_payload);

        let mut payload = sample_execution_data(U256::from(7_u64), 1);
        hydrate_cached_header_fields_from_map(&built_payload_headers, &mut payload);

        assert_eq!(payload.taiko_sidecar.header_difficulty, Some(U256::from(7_u64)));
    }

    fn uzen_chain_spec() -> Arc<alethia_reth_chainspec::spec::TaikoChainSpec> {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Uzen, ForkCondition::Timestamp(0));
        Arc::new(chain_spec)
    }

    fn pre_uzen_chain_spec() -> Arc<alethia_reth_chainspec::spec::TaikoChainSpec> {
        let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Uzen, ForkCondition::Timestamp(10));
        Arc::new(chain_spec)
    }

    fn sample_built_payload(difficulty: U256, fees: U256, timestamp: u64) -> EthBuiltPayload {
        let block = sample_uzen_block(difficulty, timestamp);
        let sealed_block = Arc::new(block.seal_slow());

        EthBuiltPayload::new(sealed_block, fees, None)
    }

    fn sample_execution_data(difficulty: U256, timestamp: u64) -> TaikoExecutionData {
        let block = sample_uzen_block(difficulty, timestamp);
        let block_hash = block.header.hash_slow();
        let execution_payload = ExecutionPayloadV1::from_block_unchecked(block_hash, &block);

        TaikoExecutionData {
            execution_payload: execution_payload.into(),
            taiko_sidecar: TaikoExecutionDataSidecar {
                tx_hash: block.body.calculate_tx_root(),
                withdrawals_hash: Some(EMPTY_WITHDRAWALS),
                header_difficulty: None,
                taiko_block: Some(true),
            },
        }
    }

    fn sample_uzen_block(difficulty: U256, timestamp: u64) -> reth_ethereum::Block {
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
