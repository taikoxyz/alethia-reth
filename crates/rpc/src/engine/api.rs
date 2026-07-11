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
use reth_db_api::{cursor::DbCursorRO, transaction::DbTxMut};
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
    ///
    /// Returns an error when the durable L1-origin tables cannot be reconciled with the persisted
    /// canonical chain; callers must not expose the engine endpoint in that state.
    pub fn new(
        engine_api: EngineApi<Provider, PayloadT, Pool, Validator, ChainSpec>,
        provider: Provider,
        chain_spec: Arc<TaikoChainSpec>,
        payload_store: PayloadStore<PayloadT>,
    ) -> Result<Self, EngineApiError>
    where
        Provider: Clone,
    {
        // The custom tables commit durably at promotion time while reth may still hold the
        // promoted block only in memory, so a hard crash can leave rows describing blocks that
        // do not survive the restart; reconcile all three tables before serving engine calls.
        try_reconcile_l1_origin_tables(&provider)
            .map_err(|err| EngineApiError::Internal(Box::new(io::Error::other(err))))?;

        Ok(Self {
            inner: engine_api,
            provider,
            chain_spec,
            payload_store,
            pending_l1_origins: Mutex::new(PendingL1Origins::default()),
            fork_choice_lock: tokio::sync::Mutex::new(()),
        })
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

    /// Persist and remove the pending origin for `head_block_hash`, re-buffering it on failure so
    /// an idempotent forkchoice retry can complete the write.
    fn persist_pending_l1_origin(&self, head_block_hash: B256) -> Result<(), EngineApiError> {
        let Some(pending) = self.lock_pending_l1_origins().take(head_block_hash) else {
            return Ok(())
        };

        if let Err(err) = self.persist_l1_origin(
            pending.stored_l1_origin.clone(),
            pending.is_preconf_block,
            pending.batch_id,
        ) {
            self.lock_pending_l1_origins().stash(head_block_hash, pending);
            return Err(err)
        }

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

        let result = self.inner.fork_choice_updated_v2(fork_choice_state, payload_attributes).await;

        // Buffer the freshly built payload's L1 origin instead of persisting it here: at this
        // point the block has only been built, not imported or canonicalized, so persisting
        // would record custom-table rows for blocks that may never exist on the canonical
        // chain (e.g. build-only previews that are never submitted via `newPayload`).
        if let Some(mut stored_l1_origin) = stored_l1_origin &&
            let Ok(status) = result.as_ref()
        {
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

        // Persist the buffered origin whenever reth applied this forkchoice, including the Engine
        // API-mandated path that applies the state before rejecting invalid payload attributes.
        if forkchoice_applied(&result) {
            self.persist_pending_l1_origin(fork_choice_state.head_block_hash)
                .map_err(ErrorObjectOwned::from)?;
        }

        result.map_err(ErrorObjectOwned::from)
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

/// Return whether reth applied the requested forkchoice state.
///
/// Invalid payload attributes are reported after reth applies a valid forkchoice state without
/// starting a payload build, so that specific error still requires promotion bookkeeping.
fn forkchoice_applied(result: &Result<ForkchoiceUpdated, EngineApiError>) -> bool {
    match result {
        Ok(status) => status.payload_status.is_valid(),
        Err(EngineApiError::ForkChoiceUpdate(
            reth_engine_primitives::BeaconForkChoiceUpdateError::ForkchoiceUpdateError(
                alloy_rpc_types_engine::ForkchoiceUpdateError::UpdatedInvalidPayloadAttributes,
            ),
        )) => true,
        _ => false,
    }
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
    /// Locally built blocks are promoted within the same insert sequence, so a promotable
    /// entry is pending for milliseconds; entries that linger belong to builds that were never
    /// imported (previews), whose rows must not be persisted anyway. Eviction is oldest-first,
    /// so losing a promotable entry requires this many newer unimported builds to arrive
    /// inside one build-to-promote window on the JWT-authenticated engine endpoint.
    const CAPACITY: usize = 1024;

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

/// Number of blocks below the persisted chain head swept for stale origin rows (and scanned for
/// a promotable row when repairing the head pointer) during startup reconciliation.
const HEAD_L1_ORIGIN_RECONCILE_LOOKBACK: u64 = 1024;

/// Reconcile the custom L1-origin tables with the persisted canonical chain at startup.
///
/// Custom-table rows commit durably at promotion time, while reth keeps the newest canonical
/// blocks in memory until its persistence threshold is reached. A hard crash inside that window
/// leaves durable rows describing blocks that do not survive the restart. Any read or write error
/// aborts the transaction so engine construction cannot proceed over known-unreconciled tables.
fn try_reconcile_l1_origin_tables<Provider>(provider: &Provider) -> Result<(), String>
where
    Provider: BlockReader + DatabaseProviderFactory,
{
    let tx = provider.database_provider_rw().map_err(|err| err.to_string())?.into_tx();
    let chain_head = provider.last_block_number().map_err(|err| err.to_string())?;

    // Drop origin rows above the persisted head: their blocks do not exist after the restart,
    // and the driver rewrites them if it re-derives those heights.
    let mut ghost_rows: u64 = 0;
    {
        let mut cursor = tx.cursor_write::<StoredL1OriginTable>().map_err(|err| err.to_string())?;
        let mut walker =
            cursor.walk_range(chain_head.saturating_add(1)..).map_err(|err| err.to_string())?;
        while let Some(entry) = walker.next() {
            entry.map_err(|err| err.to_string())?;
            walker.delete_current().map_err(|err| err.to_string())?;
            ghost_rows += 1;
        }
    }

    // Drop recent rows whose hash no longer matches the canonical block at their height: a
    // crash can preserve the row of a same-height block that was reorged out before the
    // replacement's promotion-time write reached the database.
    let mut mismatched_rows: u64 = 0;
    let sweep_floor = chain_head.saturating_sub(HEAD_L1_ORIGIN_RECONCILE_LOOKBACK);
    for number in sweep_floor..=chain_head {
        let Some(row) = tx.get::<StoredL1OriginTable>(number).map_err(|err| err.to_string())?
        else {
            continue;
        };
        let canonical = provider.block_hash(number).map_err(|err| err.to_string())?;
        if canonical != Some(row.l2_block_hash) {
            tx.delete::<StoredL1OriginTable>(number, None).map_err(|err| err.to_string())?;
            mismatched_rows += 1;
        }
    }

    // Drop batch mappings that point above the persisted head: the real last block of those
    // batches is unknown until the driver re-derives them, and a stale mapping would resolve a
    // proposal to a nonexistent block during checkpoint resume.
    let mut ghost_batches: u64 = 0;
    {
        let mut cursor = tx.cursor_write::<BatchToLastBlock>().map_err(|err| err.to_string())?;
        let mut walker = cursor.walk_range(..).map_err(|err| err.to_string())?;
        while let Some(entry) = walker.next() {
            let (_, last_block) = entry.map_err(|err| err.to_string())?;
            if last_block > chain_head {
                walker.delete_current().map_err(|err| err.to_string())?;
                ghost_batches += 1;
            }
        }
    }

    // Repair the head pointer against the swept table. A row can carry the head pointer only
    // when it exists, is not a preconfirmation row, and matches the canonical block hash.
    let stored_head = tx
        .get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)
        .map_err(|err| err.to_string())?;
    let is_promotable_row = |number: u64| -> Result<bool, String> {
        let Some(row) = tx.get::<StoredL1OriginTable>(number).map_err(|err| err.to_string())?
        else {
            return Ok(false)
        };
        if row.l1_block_height.is_zero() {
            return Ok(false)
        }
        let canonical = provider.block_hash(number).map_err(|err| err.to_string())?;
        Ok(canonical == Some(row.l2_block_hash))
    };

    let resolution = resolve_reconciled_head_l1_origin(
        stored_head,
        chain_head,
        HEAD_L1_ORIGIN_RECONCILE_LOOKBACK,
        is_promotable_row,
    )?;
    match resolution {
        HeadL1OriginReconciliation::Keep => {}
        HeadL1OriginReconciliation::ClampTo(reconciled) => {
            tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, reconciled)
                .map_err(|err| err.to_string())?;
        }
        HeadL1OriginReconciliation::Clear => {
            // Removing the pointer makes consumers see "unknown" (and fall back to checkpoint
            // resume) instead of a fabricated confirmed height with no origin row behind it.
            tx.delete::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, None)
                .map_err(|err| err.to_string())?;
        }
    }

    let repaired_head = resolution != HeadL1OriginReconciliation::Keep;
    if ghost_rows > 0 || mismatched_rows > 0 || ghost_batches > 0 || repaired_head {
        warn!(
            chain_head,
            ghost_rows,
            mismatched_rows,
            ghost_batches,
            ?stored_head,
            ?resolution,
            "reconciled L1 origin tables with the persisted canonical chain after restart"
        );
    }

    tx.commit().map_err(|err| err.to_string())?;

    Ok(())
}

/// Outcome of reconciling the stored `head_l1_origin` pointer at startup.
#[derive(Debug, PartialEq, Eq)]
enum HeadL1OriginReconciliation {
    /// The stored pointer is consistent with the persisted canonical chain.
    Keep,
    /// The pointer must be rewritten to the given block number.
    ClampTo(u64),
    /// No promotable origin row exists near the persisted head; the pointer must be removed so
    /// consumers see "unknown" instead of a fabricated confirmed height without a row.
    Clear,
}

/// Decide the startup-reconciled `head_l1_origin` value.
///
/// The stored pointer is kept only when it sits at or below the persisted chain head *and* its
/// row is promotable (present, non-preconfirmation, canonical-hash match) — a same-height reorg
/// preserved by a crash otherwise leaves a pointer whose row describes a noncanonical block.
/// When the pointer must move, the highest promotable row within `lookback` of (and never
/// above) the stored pointer or chain head is preferred; without one the pointer is cleared.
/// Predicate errors are propagated so callers never mistake a failed read for an invalid row.
fn resolve_reconciled_head_l1_origin(
    stored_head: Option<u64>,
    chain_head: u64,
    lookback: u64,
    is_promotable_row: impl Fn(u64) -> Result<bool, String>,
) -> Result<HeadL1OriginReconciliation, String> {
    let Some(stored_head) = stored_head else { return Ok(HeadL1OriginReconciliation::Keep) };

    if stored_head <= chain_head && is_promotable_row(stored_head)? {
        return Ok(HeadL1OriginReconciliation::Keep)
    }

    // Never advance the pointer during repair: scan downward from the lower of the stored
    // pointer and the persisted head.
    let top = stored_head.min(chain_head);
    let floor = top.saturating_sub(lookback);
    let mut number = top;
    loop {
        if is_promotable_row(number)? {
            return Ok(HeadL1OriginReconciliation::ClampTo(number))
        }
        if number <= floor {
            break;
        }
        number -= 1;
    }

    Ok(HeadL1OriginReconciliation::Clear)
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
    fn invalid_payload_attributes_error_reports_an_applied_forkchoice() {
        let result = Err(EngineApiError::ForkChoiceUpdate(
            reth_engine_primitives::BeaconForkChoiceUpdateError::ForkchoiceUpdateError(
                alloy_rpc_types_engine::ForkchoiceUpdateError::UpdatedInvalidPayloadAttributes,
            ),
        ));

        assert!(forkchoice_applied(&result));
    }

    #[test]
    fn unrelated_forkchoice_error_does_not_report_an_applied_forkchoice() {
        let result = Err(EngineApiError::Internal(Box::new(io::Error::other("boom"))));

        assert!(!forkchoice_applied(&result));
    }

    #[test]
    fn reconcile_keeps_consistent_head_untouched() {
        use HeadL1OriginReconciliation::Keep;
        assert_eq!(resolve_reconciled_head_l1_origin(None, 10, 1024, |_| Ok(true)).unwrap(), Keep);
        assert_eq!(
            resolve_reconciled_head_l1_origin(Some(5), 10, 1024, |_| Ok(true)).unwrap(),
            Keep
        );
        assert_eq!(
            resolve_reconciled_head_l1_origin(Some(10), 10, 1024, |_| Ok(true)).unwrap(),
            Keep
        );
    }

    #[test]
    fn reconcile_clamps_a_pointer_past_the_head_to_the_highest_promotable_row() {
        let resolved =
            resolve_reconciled_head_l1_origin(Some(12), 10, 1024, |n| Ok(n == 8 || n == 9))
                .unwrap();
        assert_eq!(resolved, HeadL1OriginReconciliation::ClampTo(9));
    }

    #[test]
    fn reconcile_repairs_a_same_height_mismatch_at_or_below_the_head() {
        // The stored pointer is within the chain, but its row no longer matches the canonical
        // block (same-height reorg preserved by a crash): clamp to the closest promotable row
        // at or below the stored pointer, never advancing it.
        let resolved =
            resolve_reconciled_head_l1_origin(Some(5), 10, 1024, |n| Ok(n == 4)).unwrap();
        assert_eq!(resolved, HeadL1OriginReconciliation::ClampTo(4));
    }

    #[test]
    fn reconcile_clears_the_pointer_without_promotable_rows() {
        // Fabricating a confirmed head at a height with no origin row would make consumers
        // dereference a nonexistent row; the pointer must be removed instead.
        assert_eq!(
            resolve_reconciled_head_l1_origin(Some(12), 10, 1024, |_| Ok(false)).unwrap(),
            HeadL1OriginReconciliation::Clear
        );
    }

    #[test]
    fn reconcile_clears_when_rows_exist_only_below_the_lookback_window() {
        let resolved =
            resolve_reconciled_head_l1_origin(Some(500), 400, 100, |n| Ok(n == 250)).unwrap();
        assert_eq!(resolved, HeadL1OriginReconciliation::Clear);
    }

    #[test]
    fn reconcile_propagates_promotable_row_read_errors() {
        let resolved = resolve_reconciled_head_l1_origin(Some(5), 10, 1024, |_| {
            Err("origin read failed".to_string())
        });

        assert_eq!(resolved, Err("origin read failed".to_string()));
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
