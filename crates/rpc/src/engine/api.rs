//! Taiko engine API RPC methods and persistence hooks.
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex, MutexGuard},
};

use alethia_reth_primitives::{
    decode_shasta_proposal_id, engine::types::TaikoExecutionData,
    payload::attributes::TaikoPayloadAttributes,
};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{B256, BlockNumber};
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
use tracing::warn;

use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use alethia_reth_db::model::{
    BatchToLastBlock, STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1Origin,
    StoredL1OriginTable,
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
    /// Taiko chain spec used to detect Unzen payloads when preparing `getPayloadV2` responses.
    chain_spec: Arc<TaikoChainSpec>,
    /// Payload store used to resolve built payloads by payload ID.
    payload_store: PayloadStore<PayloadT>,
    /// L1 origins of locally built payloads, buffered until their block is canonically promoted.
    pending_l1_origins: Mutex<PendingL1Origins>,
    /// Serializes `fork_choice_updated_v2` so pending-origin persistence decisions observe the
    /// same order in which the inner engine canonicalizes heads; mirrors go-ethereum's
    /// `forkchoiceLock`.
    fork_choice_lock: tokio::sync::Mutex<()>,
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
        // The custom tables commit durably at promotion time while reth may still hold the
        // promoted block only in memory, so a hard crash can leave the head pointer beyond the
        // head that survives the restart; reconcile before serving any engine call.
        reconcile_head_l1_origin(&provider);

        Self {
            inner: engine_api,
            provider,
            chain_spec,
            payload_store,
            pending_l1_origins: Mutex::new(PendingL1Origins::default()),
            fork_choice_lock: tokio::sync::Mutex::new(()),
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

    /// Lock the pending-origin buffer, recovering the data from a poisoned lock.
    ///
    /// The buffer holds no invariants that a panicking thread could break mid-update badly
    /// enough to justify failing every subsequent engine call, so poison is recovered.
    fn lock_pending_l1_origins(&self) -> MutexGuard<'_, PendingL1Origins> {
        self.pending_l1_origins.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
    async fn new_payload_v2(&self, payload: TaikoExecutionData) -> RpcResult<PayloadStatus> {
        self.inner.new_payload_v2(payload).await.map_err(|e| e.into())
    }

    /// Updates the fork choice with the given state and payload attributes.
    async fn fork_choice_updated_v2(
        &self,
        fork_choice_state: ForkchoiceState,
        payload_attributes: Option<EngineT::PayloadAttributes>,
    ) -> RpcResult<ForkchoiceUpdated> {
        // Serialize the whole update (inner engine call + pending-origin bookkeeping): the
        // inner engine orders canonicalization internally, but without this guard two
        // overlapping updates could run their persistence decisions in the opposite order and
        // regress `head_l1_origin` to a block that is no longer the canonical head.
        let _fork_choice_permit = self.fork_choice_lock.lock().await;

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

        let status =
            self.inner.fork_choice_updated_v2(fork_choice_state, payload_attributes).await?;

        // Buffer the freshly built payload's L1 origin instead of persisting it here: at this
        // point the block has only been built, not imported or canonicalized, so persisting
        // would record custom-table rows for blocks that may never exist on the canonical
        // chain (e.g. build-only previews that are never submitted via `newPayload`).
        if let Some(mut stored_l1_origin) = stored_l1_origin {
            let payload_id = status
                .payload_id
                .ok_or_else(|| Self::internal_error(io::Error::other("missing payload id")))?;

            let built_payload = self
                .wait_for_built_payload(payload_id)
                .await
                .map_err(|e: EngineApiError| ErrorObjectOwned::from(e))?;

            let block_hash = built_payload.block().hash_slow();
            stored_l1_origin.l2_block_hash = block_hash;

            self.lock_pending_l1_origins().stash(
                block_hash,
                PendingL1Origin { stored_l1_origin, is_preconf_block, batch_id },
            );
        }

        // Persist the buffered origin for the block this update just promoted to canonical
        // head, if that block was built locally.
        if status.payload_status.is_valid() {
            let pending = self.lock_pending_l1_origins().take(fork_choice_state.head_block_hash);
            if let Some(pending) = pending &&
                let Err(err) = self.persist_l1_origin(
                    pending.stored_l1_origin.clone(),
                    pending.is_preconf_block,
                    pending.batch_id,
                )
            {
                // Re-buffer the row so an idempotent forkchoice retry can persist it.
                self.lock_pending_l1_origins().stash(fork_choice_state.head_block_hash, pending);
                return Err(ErrorObjectOwned::from(err));
            }
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

/// A single locally built payload's L1 origin awaiting canonical promotion of its block.
#[derive(Debug, Clone)]
struct PendingL1Origin {
    /// The stored L1 origin row, with `l2_block_hash` set to the built block hash.
    stored_l1_origin: StoredL1Origin,
    /// Whether the built block is a preconfirmation block (skips head/batch pointer updates).
    is_preconf_block: bool,
    /// The Shasta proposal id decoded from the payload extra data, when Shasta is active.
    batch_id: Option<u64>,
}

/// L1-origin rows for locally built payloads, buffered until their block becomes canonical.
///
/// Rows are persisted to the custom tables only once a forkchoice update promotes the built
/// block to canonical head with a VALID status. Entries whose block is never promoted (e.g.
/// build-only previews) are evicted once the buffer exceeds capacity and never reach the
/// database.
#[derive(Debug, Default)]
struct PendingL1Origins {
    /// Pending entries in insertion order, keyed by built block hash.
    entries: VecDeque<(B256, PendingL1Origin)>,
}

impl PendingL1Origins {
    /// Maximum number of buffered entries retained while awaiting promotion.
    ///
    /// Locally built blocks are normally promoted within the same insert sequence, so the
    /// buffer only accumulates when payloads are built without being imported; the cap bounds
    /// that growth while retaining plenty of slack for in-flight blocks.
    const CAPACITY: usize = 64;

    /// Buffer a built payload's origin, replacing any entry for the same block hash and
    /// evicting the oldest entry beyond capacity.
    fn stash(&mut self, block_hash: B256, pending: PendingL1Origin) {
        self.entries.retain(|(hash, _)| *hash != block_hash);
        self.entries.push_back((block_hash, pending));
        while self.entries.len() > Self::CAPACITY {
            if let Some((evicted_hash, evicted)) = self.entries.pop_front() {
                warn!(
                    block_number = %evicted.stored_l1_origin.block_id,
                    block_hash = %evicted_hash,
                    "evicting a pending L1 origin that was never canonically promoted; if this \
                     block is promoted later its origin rows will be missing"
                );
            }
        }
    }

    /// Remove and return the buffered entry for the given block hash, if any.
    fn take(&mut self, block_hash: B256) -> Option<PendingL1Origin> {
        let index = self.entries.iter().position(|(hash, _)| *hash == block_hash)?;
        self.entries.remove(index).map(|(_, pending)| pending)
    }
}

/// Number of blocks below the persisted chain head scanned for a promotable origin row when
/// reconciling a stale `head_l1_origin` pointer at startup.
const HEAD_L1_ORIGIN_RECONCILE_LOOKBACK: u64 = 1024;

/// Reconcile the durable `head_l1_origin` pointer with the persisted canonical chain.
///
/// Custom-table rows commit durably at promotion time, while reth keeps the newest canonical
/// blocks in memory until its persistence threshold is reached. A hard crash inside that window
/// leaves `head_l1_origin` pointing past the head that survives the restart, so driver resume
/// paths would anchor on a block that no longer exists. Failures are logged rather than
/// propagated: a reconciliation problem must not prevent the node from starting.
fn reconcile_head_l1_origin<Provider>(provider: &Provider)
where
    Provider: BlockReader + DatabaseProviderFactory,
{
    if let Err(err) = try_reconcile_head_l1_origin(provider) {
        warn!(err, "failed to reconcile head L1 origin with the persisted canonical chain");
    }
}

/// Fallible body of [`reconcile_head_l1_origin`], returning a printable error.
fn try_reconcile_head_l1_origin<Provider>(provider: &Provider) -> Result<(), String>
where
    Provider: BlockReader + DatabaseProviderFactory,
{
    let tx = provider.database_provider_rw().map_err(|err| err.to_string())?.into_tx();

    let Some(stored_head) = tx
        .get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)
        .map_err(|err| err.to_string())?
    else {
        return Ok(());
    };

    let chain_head = provider.last_block_number().map_err(|err| err.to_string())?;

    // A row can carry the head pointer only when it exists, is not a preconfirmation row, and
    // still matches the canonical block hash at its height.
    let is_promotable_row = |number: u64| -> bool {
        let row = match tx.get::<StoredL1OriginTable>(number) {
            Ok(Some(row)) => row,
            _ => return false,
        };
        if row.l1_block_height.is_zero() {
            return false;
        }
        matches!(provider.block_hash(number), Ok(Some(hash)) if hash == row.l2_block_hash)
    };

    let Some(reconciled) = resolve_reconciled_head_l1_origin(
        Some(stored_head),
        chain_head,
        HEAD_L1_ORIGIN_RECONCILE_LOOKBACK,
        is_promotable_row,
    ) else {
        return Ok(());
    };

    warn!(
        stored_head,
        chain_head,
        reconciled,
        "head L1 origin pointed past the persisted canonical head after restart; clamping"
    );

    tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, reconciled)
        .map_err(|err| err.to_string())?;
    tx.commit().map_err(|err| err.to_string())?;

    Ok(())
}

/// Decide the startup-reconciled `head_l1_origin` value.
///
/// Returns `Some(new_value)` when the stored pointer lies beyond the persisted chain head and
/// must be clamped — preferring the highest block within `lookback` of the chain head whose
/// origin row is promotable, falling back to the chain head itself — or `None` when the stored
/// pointer is already consistent.
fn resolve_reconciled_head_l1_origin(
    stored_head: Option<u64>,
    chain_head: u64,
    lookback: u64,
    is_promotable_row: impl Fn(u64) -> bool,
) -> Option<u64> {
    let stored_head = stored_head?;
    if stored_head <= chain_head {
        return None;
    }

    let floor = chain_head.saturating_sub(lookback);
    let mut number = chain_head;
    loop {
        if is_promotable_row(number) {
            return Some(number);
        }
        if number <= floor {
            break;
        }
        number -= 1;
    }

    Some(chain_head)
}

#[cfg(test)]
mod tests {
    use super::*;

    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alloy_consensus::{BlockBody, Header, constants::EMPTY_WITHDRAWALS};
    use alloy_eips::merge::BEACON_NONCE;
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use reth_primitives_traits::Block as _;
    use std::sync::Arc;

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

    #[test]
    fn pending_take_returns_stashed_entry_once() {
        let mut pending = PendingL1Origins::default();
        let hash = B256::with_last_byte(0x01);
        pending.stash(hash, sample_pending(7, Some(3)));

        let taken = pending.take(hash).expect("entry must be present");
        assert_eq!(taken.stored_l1_origin, sample_stored_l1_origin(7));
        assert_eq!(taken.batch_id, Some(3));
        assert!(pending.take(hash).is_none(), "entry must be removed after take");
    }

    #[test]
    fn pending_take_of_unknown_hash_is_none() {
        let mut pending = PendingL1Origins::default();
        assert!(pending.take(B256::with_last_byte(0x01)).is_none());
    }

    #[test]
    fn pending_stash_replaces_entry_for_same_hash() {
        let mut pending = PendingL1Origins::default();
        let hash = B256::with_last_byte(0x01);
        pending.stash(hash, sample_pending(7, Some(1)));
        pending.stash(hash, sample_pending(7, Some(2)));

        let taken = pending.take(hash).expect("entry must be present");
        assert_eq!(taken.batch_id, Some(2));
        assert!(pending.take(hash).is_none(), "replacement must not leave a duplicate");
    }

    #[test]
    fn pending_stash_evicts_oldest_beyond_capacity() {
        let mut pending = PendingL1Origins::default();
        for i in 0..=PendingL1Origins::CAPACITY {
            let hash = B256::from(U256::from(i as u64 + 1));
            pending.stash(hash, sample_pending(i as u64, None));
        }

        assert!(pending.take(B256::from(U256::from(1_u64))).is_none(), "oldest entry is evicted");
        assert!(pending.take(B256::from(U256::from(2_u64))).is_some(), "newer entries survive");
    }

    #[test]
    fn reconcile_leaves_consistent_head_untouched() {
        assert_eq!(resolve_reconciled_head_l1_origin(None, 10, 1024, |_| true), None);
        assert_eq!(resolve_reconciled_head_l1_origin(Some(5), 10, 1024, |_| true), None);
        assert_eq!(resolve_reconciled_head_l1_origin(Some(10), 10, 1024, |_| true), None);
    }

    #[test]
    fn reconcile_clamps_to_highest_promotable_row() {
        let resolved = resolve_reconciled_head_l1_origin(Some(12), 10, 1024, |n| n == 8 || n == 9);
        assert_eq!(resolved, Some(9));
    }

    #[test]
    fn reconcile_falls_back_to_chain_head_without_promotable_rows() {
        assert_eq!(resolve_reconciled_head_l1_origin(Some(12), 10, 1024, |_| false), Some(10));
    }

    #[test]
    fn reconcile_ignores_rows_below_the_lookback_window() {
        let resolved = resolve_reconciled_head_l1_origin(Some(500), 400, 100, |n| n == 250);
        assert_eq!(resolved, Some(400));
    }

    /// Build a deterministic stored L1 origin for pending-buffer tests.
    fn sample_stored_l1_origin(block_id: u64) -> StoredL1Origin {
        StoredL1Origin {
            block_id: U256::from(block_id),
            l2_block_hash: B256::with_last_byte(0xaa),
            l1_block_height: U256::from(100_u64),
            l1_block_hash: B256::with_last_byte(0xbb),
            build_payload_args_id: [0; 8],
            is_forced_inclusion: false,
            signature: [0; 65],
        }
    }

    /// Build a pending entry wrapping [`sample_stored_l1_origin`].
    fn sample_pending(block_id: u64, batch_id: Option<u64>) -> PendingL1Origin {
        PendingL1Origin {
            stored_l1_origin: sample_stored_l1_origin(block_id),
            is_preconf_block: false,
            batch_id,
        }
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
        let sealed_block = Arc::new(block.seal_slow());

        EthBuiltPayload::new(sealed_block, fees, None, None)
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
