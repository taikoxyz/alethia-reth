//! Proof-history backed overrides for selected `debug_` RPC methods.

use crate::proof_state::{
    ProofHistoryReadiness, ProofHistoryStateProviderFactory, ProofStateProvider,
    ResolvedBlockState, canonical_state_changed, complete_guarded_work, run_proof_task,
};
use alethia_reth_block::{
    executor::{
        is_recoverable_non_anchor_tx_error, is_zk_gas_difficulty_mismatch, is_zk_gas_limit_exceeded,
    },
    tx_selection::zlib_compressed_len,
};
use alethia_reth_primitives::{
    payload::builder::decode_recovered_transactions, transaction::is_allowed_tx_type,
};
use alloy_consensus::{BlockHeader, transaction::Recovered};
use alloy_eips::{BlockId, BlockNumHash, BlockNumberOrTag, eip4844::BYTES_PER_BLOB};
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_debug::ExecutionWitness;
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_ethereum::{EthPrimitives, TransactionSigned};
use reth_ethereum_primitives::Block;
use reth_evm::{
    ConfigureEvm,
    execute::{BlockExecutionError, BlockExecutor, Executor},
};
use reth_optimism_trie::{OpProofsStorage, OpProofsStore};
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{HeaderProvider, ProviderResult};
use reth_revm::{
    State, database::StateProviderDatabase, db::states::bundle_state::BundleRetention,
    witness::ExecutionWitnessRecord,
};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use reth_trie_common::ExecutionWitnessMode;
use serde::{Deserialize, Serialize};
use std::{future::Future, sync::Arc};

/// Block-level tx-list DA limit: the zlib-compressed transaction list must fit in one blob.
///
/// This mirrors the limit the proposer and [`alethia_reth_block::tx_selection`] enforce per block
/// (`max_da_bytes_per_list`), so a tx list that would never have been a valid block is rejected.
const MAX_TX_LIST_COMPRESSED_BYTES: usize = BYTES_PER_BLOB;

/// Generous upper bound on the raw (decompressed) tx-list bytes accepted before compression.
///
/// This is not a protocol limit; it only bounds the work done by the compressed-size check so an
/// oversized request (the auth RPC server accepts bodies up to 128 MiB) cannot be handed to zlib.
/// A valid single block's decompressed tx list is far smaller — a 45M-gas block holds at most
/// ~11 MiB of zero-byte calldata — so this never rejects a legitimately proposed block.
const MAX_TX_LIST_RAW_BYTES: usize = 16 * 1024 * 1024;

/// Exact original witness target and the canonical parent state it must execute against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WitnessTarget {
    /// Number-and-hash identity recovered before any synthetic transaction replacement.
    block: BlockNumHash,
    /// Expected parent number paired with the target header's original parent hash.
    parent: BlockNumHash,
}

impl WitnessTarget {
    /// Captures an exact non-genesis target and derives its required parent identity.
    fn from_parts(number: u64, hash: B256, parent_hash: B256) -> Result<Self, EthApiError> {
        let parent_number = number.checked_sub(1).ok_or_else(|| {
            EthApiError::EvmCustom("genesis block has no parent state".to_string())
        })?;
        Ok(Self {
            block: BlockNumHash::new(number, hash),
            parent: BlockNumHash::new(parent_number, parent_hash),
        })
    }

    /// Captures target and parent identities from the original recovered canonical block.
    fn from_block(block: &RecoveredBlock<Block>) -> Result<Self, EthApiError> {
        Self::from_parts(block.header().number(), block.hash(), block.parent_hash())
    }
}

/// Prevalidates an exact witness target, then resolves and verifies its original parent identity.
async fn prepare_witness_parent_with<ValidateTarget, ResolveParent, ResolveFuture>(
    target: WitnessTarget,
    mut validate_target: ValidateTarget,
    resolve_parent: ResolveParent,
) -> ProviderResult<ResolvedBlockState>
where
    ValidateTarget: FnMut(BlockNumHash) -> ProviderResult<()>,
    ResolveParent: FnOnce(BlockId) -> ResolveFuture,
    ResolveFuture: Future<Output = ProviderResult<ResolvedBlockState>>,
{
    validate_target(target.block)?;
    let resolved = match resolve_parent(BlockId::Hash(target.parent.hash.into())).await {
        Ok(resolved) => resolved,
        Err(error) => {
            // Prefer the stable reorg signal when the target moved while its parent was resolving;
            // otherwise preserve the underlying provider failure.
            validate_target(target.block)?;
            return Err(error)
        }
    };
    match &resolved {
        ResolvedBlockState::Canonical { block, .. } if *block == target.parent => Ok(resolved),
        ResolvedBlockState::Pending(_) | ResolvedBlockState::Canonical { .. } => {
            Err(canonical_state_changed())
        }
    }
}

/// Completes a witness and validates the captured original target before exposing the result.
fn complete_witness_work<Work, Validate, Output, Error>(
    target: BlockNumHash,
    work: Work,
    validate: Validate,
) -> Result<Output, Error>
where
    Work: FnOnce() -> Result<Output, Error>,
    Validate: FnOnce(BlockNumHash) -> Result<(), Error>,
{
    complete_guarded_work(work, || validate(target))
}

/// RPC server trait for Taiko proof-history backed `debug_` witness methods.
#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait TaikoDebugWitnessApi {
    /// Returns an execution witness for a canonical block number or tag.
    #[method(name = "executionWitness")]
    async fn execution_witness(
        &self,
        block: BlockNumberOrTag,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness>;

    /// Returns an execution witness for a block hash.
    #[method(name = "executionWitnessByBlockHash")]
    async fn execution_witness_by_block_hash(
        &self,
        hash: B256,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness>;

    /// Replays an explicit transaction list on top of the requested block's parent state and
    /// returns the generated execution witness.
    #[method(name = "executionWitnessForTxList")]
    async fn execution_witness_for_tx_list(
        &self,
        block: BlockId,
        tx_list: Bytes,
        mode: Option<ExecutionWitnessMode>,
        options: Option<TxListWitnessOptions>,
    ) -> RpcResult<ExecutionWitness>;
}

/// Options for `debug_executionWitnessForTxList`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxListWitnessOptions {
    /// Skip matching recomputed zk gas against `header.difficulty`.
    ///
    /// This is only useful for debug experiments that replay non-canonical transaction lists on
    /// top of a canonical parent state.
    #[serde(default)]
    pub skip_zk_gas_difficulty_check: bool,
}

/// `debug_` namespace overrides that use proof-history state for witness generation.
#[derive(Debug)]
pub struct TaikoDebugWitnessExt<Eth, Storage, Provider> {
    /// Provider used to fetch ancestor block headers for the returned witness.
    provider: Provider,
    /// Ethereum RPC API used to load blocks, state, and the EVM configuration.
    eth_api: Eth,
    /// Factory for sidecar-backed state providers.
    state_provider_factory: ProofHistoryStateProviderFactory<Eth, Storage>,
}

impl<Eth, Storage, Provider> TaikoDebugWitnessExt<Eth, Storage, Provider>
where
    Eth: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'static,
    Provider: HeaderProvider + Clone + Send + Sync + 'static,
    Provider::Header: BlockHeader + alloy_rlp::Encodable,
{
    /// Creates a new proof-history backed debug witness override.
    pub fn new(
        provider: Provider,
        eth_api: Eth,
        storage: OpProofsStorage<Storage>,
        readiness: ProofHistoryReadiness,
    ) -> Self {
        Self {
            provider,
            state_provider_factory: ProofHistoryStateProviderFactory::new(
                eth_api.clone(),
                storage,
                readiness,
            ),
            eth_api,
        }
    }

    /// Re-executes the requested canonical block and returns the generated witness.
    async fn execution_witness_for_id(
        &self,
        block_id: BlockId,
        mode: ExecutionWitnessMode,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block = self
            .eth_api
            .recovered_block(block_id)
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        let target = WitnessTarget::from_block(&block)?;
        self.execution_witness_for_block(block, target, mode).await
    }

    /// Replays the explicit transaction list on top of the requested block's parent state.
    async fn execution_witness_for_tx_list_for_id(
        &self,
        block_id: BlockId,
        tx_list: Bytes,
        mode: ExecutionWitnessMode,
        options: TxListWitnessOptions,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block = self
            .eth_api
            .recovered_block(block_id)
            .await?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        let target = WitnessTarget::from_block(&block)?;
        self.execution_witness_for_tx_list_block(block, target, tx_list, mode, options).await
    }

    /// Re-executes the provided block against proof-history backed parent state and returns the
    /// generated witness.
    async fn execution_witness_for_block(
        &self,
        block: Arc<RecoveredBlock<Block>>,
        target: WitnessTarget,
        mode: ExecutionWitnessMode,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let resolved_parent = prepare_witness_parent_with(
            target,
            |block| self.state_provider_factory.validate_canonical_block(block),
            |parent_id| self.state_provider_factory.resolve_block_state(parent_id),
        )
        .await
        .map_err(EthApiError::from)?;
        let factory = self.state_provider_factory.clone();
        let evm_config = self.eth_api.evm_config().clone();
        let header_provider = self.provider.clone();
        // Block re-execution and witness assembly are CPU/I/O heavy; keep them off the async
        // RPC workers and share Reth's configured proof execution limit.
        run_proof_task(&self.eth_api, move || {
            let selected = factory.state_provider_at(resolved_parent).map_err(EthApiError::from)?;
            let (state_provider, guard) = match selected {
                ProofStateProvider::Pending(_) => {
                    return Err(EthApiError::from(canonical_state_changed()).into())
                }
                ProofStateProvider::Canonical { state, guard } => (state, guard),
            };
            complete_witness_work(
                target.block,
                || {
                    let db = StateProviderDatabase::new(&*state_provider);
                    let block_executor = evm_config.executor(db);
                    let mut witness_record = ExecutionWitnessRecord::default();

                    block_executor
                        .execute_with_state_closure(&*block, |statedb: &State<_>| {
                            witness_record.record_executed_state(statedb, mode);
                        })
                        .map_err(EthApiError::from)?;

                    witness_record
                        .into_execution_witness(
                            &*state_provider,
                            &header_provider,
                            target.block.number,
                            mode,
                        )
                        .map_err(EthApiError::from)
                        .map_err(Into::into)
                },
                |target| {
                    factory
                        .validate_after_work(guard, Some(target))
                        .map_err(EthApiError::from)
                        .map_err(Into::into)
                },
            )
        })
        .await
    }

    /// Replays the explicit transaction list on the canonical block's parent state with
    /// prover-style filtering, without enabling the block crate's global `prover` feature for
    /// normal node execution.
    ///
    /// The raw transaction list is validated and decoded inside the permit-guarded blocking
    /// closure: the size check zlib-compresses up to 16 MiB and decoding recovers every
    /// transaction signer, which is far too much CPU for the async RPC workers and must stay
    /// under Reth's shared proof execution limit.
    async fn execution_witness_for_tx_list_block(
        &self,
        canonical_block: Arc<RecoveredBlock<Block>>,
        target: WitnessTarget,
        tx_list: Bytes,
        mode: ExecutionWitnessMode,
        options: TxListWitnessOptions,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let resolved_parent = prepare_witness_parent_with(
            target,
            |block| self.state_provider_factory.validate_canonical_block(block),
            |parent_id| self.state_provider_factory.resolve_block_state(parent_id),
        )
        .await
        .map_err(EthApiError::from)?;
        let factory = self.state_provider_factory.clone();
        let evm_config = self.eth_api.evm_config().clone();
        let header_provider = self.provider.clone();
        // Tx-list validation/decode, transaction replay, and witness assembly are CPU/I/O
        // heavy; keep them off the async RPC workers and under Reth's shared proof limit.
        run_proof_task(&self.eth_api, move || {
            let selected = factory.state_provider_at(resolved_parent).map_err(EthApiError::from)?;
            let (state_provider, guard) = match selected {
                ProofStateProvider::Pending(_) => {
                    return Err(EthApiError::from(canonical_state_changed()).into())
                }
                ProofStateProvider::Canonical { state, guard } => (state, guard),
            };
            complete_witness_work(
                target.block,
                || {
                    let txs = decode_recovered_tx_list(tx_list)?;
                    let block = block_with_tx_list(canonical_block.as_ref().clone(), txs);
                    let db = StateProviderDatabase::new(&*state_provider);
                    let mut state = State::builder().with_database(db).with_bundle_update().build();
                    let mut witness_record = ExecutionWitnessRecord::default();

                    {
                        let mut block_executor = evm_config
                            .executor_for_block(&mut state, block.sealed_block())
                            .map_err(|err| EthApiError::EvmCustom(err.to_string()))?;

                        block_executor.apply_pre_execution_changes().map_err(EthApiError::from)?;

                        for (idx, tx) in block.transactions_recovered().enumerate() {
                            let is_anchor_transaction = idx == 0;

                            // Taiko blocks never contain blob transactions: the build paths skip
                            // non-anchor ones (crates/payload/src/builder/execution.rs) and
                            // consensus validation rejects any block
                            // that includes one. Keep the same
                            // filtering, but never silently discard the mandatory anchor
                            // transaction.
                            match should_skip_disallowed_tx_type(tx.inner(), is_anchor_transaction)
                            {
                                Ok(true) => {
                                    continue;
                                }
                                Ok(false) => {}
                                Err(err) => return Err(err.into()),
                            }

                            match block_executor.execute_transaction(tx) {
                                Ok(_) => {}
                                Err(err)
                                    if is_recoverable_tx_list_error(
                                        &err,
                                        is_anchor_transaction,
                                    ) =>
                                {
                                    if is_zk_gas_limit_exceeded(&err) {
                                        break
                                    }
                                }
                                Err(err) => return Err(EthApiError::from(err).into()),
                            }
                        }

                        match block_executor.apply_post_execution_changes() {
                            Ok(_) => {}
                            Err(err)
                                if options.skip_zk_gas_difficulty_check &&
                                    is_zk_gas_difficulty_mismatch(&err) => {}
                            Err(err) => return Err(EthApiError::from(err).into()),
                        }
                    }

                    state.merge_transitions(BundleRetention::Reverts);
                    witness_record.record_executed_state(&state, mode);

                    witness_record
                        .into_execution_witness(
                            &*state_provider,
                            &header_provider,
                            target.block.number,
                            mode,
                        )
                        .map_err(EthApiError::from)
                        .map_err(Into::into)
                },
                |target| {
                    factory
                        .validate_after_work(guard, Some(target))
                        .map_err(EthApiError::from)
                        .map_err(Into::into)
                },
            )
        })
        .await
    }
}

#[async_trait]
impl<Eth, Storage, Provider> TaikoDebugWitnessApiServer
    for TaikoDebugWitnessExt<Eth, Storage, Provider>
where
    Eth: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'static,
    Provider: HeaderProvider + Clone + Send + Sync + 'static,
    Provider::Header: BlockHeader + alloy_rlp::Encodable,
{
    /// Handles `debug_executionWitness` with proof-history backed state.
    async fn execution_witness(
        &self,
        block: BlockNumberOrTag,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        self.execution_witness_for_id(block.into(), mode.unwrap_or_default())
            .await
            .map_err(Into::into)
    }

    /// Handles `debug_executionWitnessByBlockHash` with proof-history backed state.
    async fn execution_witness_by_block_hash(
        &self,
        hash: B256,
        mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        self.execution_witness_for_id(hash.into(), mode.unwrap_or_default())
            .await
            .map_err(Into::into)
    }

    /// Handles `debug_executionWitnessForTxList` with proof-history backed parent state.
    async fn execution_witness_for_tx_list(
        &self,
        block: BlockId,
        tx_list: Bytes,
        mode: Option<ExecutionWitnessMode>,
        options: Option<TxListWitnessOptions>,
    ) -> RpcResult<ExecutionWitness> {
        self.execution_witness_for_tx_list_for_id(
            block,
            tx_list,
            mode.unwrap_or_default(),
            options.unwrap_or_default(),
        )
        .await
        .map_err(Into::into)
    }
}

/// Decode an RLP transaction list and recover each transaction signer for EVM execution.
///
/// Rejects oversized inputs (see [`check_tx_list_size`]) before decoding. Uses the same lenient
/// ingestion as the payload builder ([`decode_recovered_transactions`]): transactions whose signer
/// cannot be recovered are skipped rather than failing the request, so the replayed witness matches
/// what the node would have executed for the same tx list. A malformed top-level RLP list is still
/// reported as an error.
fn decode_recovered_tx_list(
    tx_list: Bytes,
) -> Result<Vec<Recovered<TransactionSigned>>, EthApiError> {
    let raw = tx_list.as_ref();
    check_tx_list_size(raw, MAX_TX_LIST_RAW_BYTES, MAX_TX_LIST_COMPRESSED_BYTES)?;
    decode_recovered_transactions(raw)
        .map_err(|err| EthApiError::EvmCustom(format!("failed to decode tx list: {err}")))
}

/// Reject a tx list that is too large to be a valid block-level transaction list.
///
/// `raw` is the decompressed RLP tx list. The cheap `max_raw` byte check runs first to bound the
/// work of the compression check; the authoritative limit is `max_compressed`, the zlib-compressed
/// size that must fit the block DA budget (one blob).
fn check_tx_list_size(
    raw: &[u8],
    max_raw: usize,
    max_compressed: usize,
) -> Result<(), EthApiError> {
    if raw.len() > max_raw {
        return Err(EthApiError::EvmCustom(format!(
            "tx list size {} exceeds raw limit {max_raw}",
            raw.len(),
        )));
    }

    let compressed = zlib_compressed_len(raw);
    if compressed > max_compressed as u64 {
        return Err(EthApiError::EvmCustom(format!(
            "tx list compressed size {compressed} exceeds block DA limit {max_compressed}",
        )));
    }

    Ok(())
}

/// Replace a canonical block's transactions with the explicit replay transaction list.
fn block_with_tx_list(
    block: RecoveredBlock<Block>,
    txs: Vec<Recovered<TransactionSigned>>,
) -> RecoveredBlock<Block> {
    let block_hash = block.hash();
    let mut block = block.into_block();
    let mut senders = Vec::with_capacity(txs.len());
    let mut transactions = Vec::with_capacity(txs.len());

    for tx in txs {
        let (tx, sender) = tx.into_parts();
        transactions.push(tx);
        senders.push(sender);
    }

    block.body.transactions = transactions;
    RecoveredBlock::new(block, senders, block_hash)
}

/// Return whether a transaction-list replay error should be tolerated for prover witness parity.
///
/// Anchor (index 0) failures are always fatal; non-anchor failures defer to the shared
/// [`is_recoverable_non_anchor_tx_error`] classifier so the recoverable error set stays in sync
/// with the prover executor and cannot drift.
fn is_recoverable_tx_list_error(err: &BlockExecutionError, is_anchor_transaction: bool) -> bool {
    !is_anchor_transaction && is_recoverable_non_anchor_tx_error(err)
}

/// Return whether a disallowed transaction type should be skipped in tx-list replay.
///
/// Non-anchor transactions match the payload-builder filtering behavior. The anchor transaction is
/// mandatory block setup, so silently skipping it would produce a witness for a block the node
/// would never execute.
fn should_skip_disallowed_tx_type(
    tx: &TransactionSigned,
    is_anchor_transaction: bool,
) -> Result<bool, EthApiError> {
    if is_allowed_tx_type(tx) {
        return Ok(false);
    }

    if is_anchor_transaction {
        return Err(EthApiError::EvmCustom("anchor transaction type is not allowed".to_string()));
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof_state::ResolvedBlockState;
    use alloy_consensus::{Signed, TxEip4844, TxLegacy};
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use alloy_rlp::Encodable;
    use reth_evm::execute::BlockValidationError;
    use reth_provider::{ProviderError, test_utils::MockEthProvider};
    use serde_json::json;
    use std::{
        cell::{Cell, RefCell},
        sync::{Arc, Mutex},
    };

    #[test]
    fn witness_target_rejects_genesis_without_a_parent() {
        let err = WitnessTarget::from_parts(0, B256::repeat_byte(0x10), B256::ZERO)
            .expect_err("genesis has no parent state to execute against");

        assert!(err.to_string().contains("genesis block has no parent state"), "{err}");
    }

    #[tokio::test]
    async fn witness_target_prevalidation_happens_before_parent_resolution() {
        let target =
            WitnessTarget::from_parts(10, B256::repeat_byte(0x10), B256::repeat_byte(0x09))
                .expect("non-genesis target has an exact parent");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let validation_calls = calls.clone();
        let parent_calls = calls.clone();

        let resolved = prepare_witness_parent_with(
            target,
            |block| {
                validation_calls.lock().unwrap().push("target");
                assert_eq!(block, target.block);
                Ok(())
            },
            |block_id| async move {
                parent_calls.lock().unwrap().push("parent");
                assert_eq!(block_id, BlockId::Hash(target.parent.hash.into()));
                Ok(ResolvedBlockState::Canonical {
                    block: target.parent,
                    state: Box::new(MockEthProvider::default()),
                })
            },
        )
        .await
        .expect("an exact canonical parent resolves");
        calls.lock().unwrap().push("execution");

        assert!(
            matches!(resolved, ResolvedBlockState::Canonical { block, .. } if block == target.parent)
        );
        assert_eq!(*calls.lock().unwrap(), vec!["target", "parent", "execution"]);
    }

    #[tokio::test]
    async fn noncanonical_by_hash_target_is_rejected_before_execution() {
        let target =
            WitnessTarget::from_parts(10, B256::repeat_byte(0x10), B256::repeat_byte(0x09))
                .expect("non-genesis target has an exact parent");
        let parent_called = Cell::new(false);

        let err = prepare_witness_parent_with(
            target,
            |_| Err(canonical_state_changed()),
            |_| async {
                parent_called.set(true);
                unreachable!("parent resolution must follow successful target validation")
            },
        )
        .await
        .expect_err("a noncanonical target fails before parent resolution");

        assert_eq!(err.to_string(), "canonical state changed; retry");
        assert!(!parent_called.get());
    }

    #[tokio::test]
    async fn target_reorg_while_parent_disappears_returns_retry_error() {
        let target =
            WitnessTarget::from_parts(10, B256::repeat_byte(0x10), B256::repeat_byte(0x09))
                .expect("non-genesis target has an exact parent");
        let validations = Cell::new(0);

        let err = prepare_witness_parent_with(
            target,
            |_| {
                validations.set(validations.get() + 1);
                if validations.get() == 1 { Ok(()) } else { Err(canonical_state_changed()) }
            },
            |_| async { Err(ProviderError::other(std::io::Error::other("parent disappeared"))) },
        )
        .await
        .expect_err("a target reorg must supersede the transient parent lookup error");

        assert_eq!(err.to_string(), "canonical state changed; retry");
        assert_eq!(validations.get(), 2);
    }

    #[tokio::test]
    async fn wrong_or_pending_witness_parent_is_rejected() {
        let target =
            WitnessTarget::from_parts(10, B256::repeat_byte(0x10), B256::repeat_byte(0x09))
                .expect("non-genesis target has an exact parent");

        for wrong_parent in [
            BlockNumHash::new(target.parent.number, B256::repeat_byte(0xee)),
            BlockNumHash::new(target.parent.number - 1, target.parent.hash),
        ] {
            let wrong_err = prepare_witness_parent_with(
                target,
                |_| Ok(()),
                |_| async {
                    Ok(ResolvedBlockState::Canonical {
                        block: wrong_parent,
                        state: Box::new(MockEthProvider::default()),
                    })
                },
            )
            .await
            .expect_err("a different parent identity must fail closed");
            assert_eq!(wrong_err.to_string(), "canonical state changed; retry");
        }

        let pending_err = prepare_witness_parent_with(
            target,
            |_| Ok(()),
            |_| async { Ok(ResolvedBlockState::Pending(Box::new(MockEthProvider::default()))) },
        )
        .await
        .expect_err("pending state cannot back a canonical witness target");
        assert_eq!(pending_err.to_string(), "canonical state changed; retry");
    }

    #[test]
    fn witness_target_reorg_is_rejected_after_parent_work() {
        let target = BlockNumHash::new(10, B256::repeat_byte(0x10));
        let calls = RefCell::new(Vec::new());

        let err = complete_witness_work(
            target,
            || {
                calls.borrow_mut().push("parent-work");
                Ok::<_, ProviderError>(())
            },
            |validated| {
                calls.borrow_mut().push("target-validation");
                assert_eq!(validated, target);
                Err(canonical_state_changed())
            },
        )
        .expect_err("a target reorg after witness assembly must reject the result");

        assert_eq!(err.to_string(), "canonical state changed; retry");
        assert_eq!(*calls.borrow(), vec!["parent-work", "target-validation"]);
    }

    #[test]
    fn tx_list_witness_target_reorg_is_rejected_after_parent_work() {
        let original_target = BlockNumHash::new(10, B256::repeat_byte(0x10));
        let synthetic_output = BlockNumHash::new(10, B256::repeat_byte(0xee));
        let assembled = Cell::new(false);
        let validated = Cell::new(None);

        let err = complete_witness_work(
            original_target,
            || {
                assembled.set(true);
                Ok::<_, ProviderError>(synthetic_output)
            },
            |target| {
                validated.set(Some(target));
                Err(canonical_state_changed())
            },
        )
        .expect_err("a target reorg after tx-list assembly must reject the result");

        assert_eq!(err.to_string(), "canonical state changed; retry");
        assert!(assembled.get());
        assert_eq!(validated.get(), Some(original_target));
    }

    #[test]
    fn tx_list_witness_validates_the_original_target_not_synthetic_output() {
        let original_target = BlockNumHash::new(10, B256::repeat_byte(0x10));
        let synthetic_output = BlockNumHash::new(10, B256::repeat_byte(0xee));
        let validated = Cell::new(None);

        let output = complete_witness_work(
            original_target,
            || Ok::<_, ProviderError>(synthetic_output),
            |target| {
                validated.set(Some(target));
                Ok(())
            },
        )
        .expect("completed witness validates");

        assert_eq!(output, synthetic_output);
        assert_eq!(validated.get(), Some(original_target));
    }

    #[test]
    fn decode_recovered_tx_list_accepts_empty_rlp_list() {
        let txs = decode_recovered_tx_list(Bytes::from_static(&[0xc0])).unwrap();

        assert!(txs.is_empty());
    }

    #[test]
    fn blob_transactions_are_not_an_allowed_tx_type() {
        // The replay loop skips any tx for which `is_allowed_tx_type` is false, matching the build
        // paths. Blob (EIP-4844) transactions are never allowed in a Taiko block; legacy ones are.
        let signature = Signature::new(U256::from(1u64), U256::from(1u64), false);

        let legacy: TransactionSigned = Signed::new_unchecked(
            TxLegacy {
                chain_id: Some(1),
                nonce: 0,
                gas_price: 1,
                gas_limit: 21_000,
                to: TxKind::Call(Address::ZERO),
                value: U256::ZERO,
                input: Bytes::new(),
            },
            signature,
            B256::ZERO,
        )
        .into();
        assert!(is_allowed_tx_type(&legacy));

        let blob = blob_transaction();
        assert!(!is_allowed_tx_type(&blob));
    }

    #[test]
    fn decode_recovered_tx_list_skips_unrecoverable_transactions() {
        // A legacy transaction with `r = 0` has an unrecoverable signature. Previously this aborted
        // the whole request; it must now be skipped, matching the payload builder's lenient tx-list
        // ingestion so the replay reflects what the node would have executed.
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 1,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signature = Signature::new(U256::ZERO, U256::from(1u64), false);
        let signed: TransactionSigned = Signed::new_unchecked(tx, signature, B256::ZERO).into();
        let mut encoded = Vec::new();
        vec![signed].encode(&mut encoded);

        let txs = decode_recovered_tx_list(Bytes::from(encoded))
            .expect("unrecoverable transactions are skipped, not fatal");

        assert!(txs.is_empty());
    }

    #[test]
    fn tx_list_replay_recovers_non_anchor_block_gas_errors() {
        let err = BlockExecutionError::Validation(
            BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: 2,
                block_available_gas: 1,
            },
        );

        assert!(is_recoverable_tx_list_error(&err, false));
        assert!(!is_recoverable_tx_list_error(&err, true));
    }

    #[test]
    fn tx_list_replay_rejects_disallowed_anchor_tx_type() {
        let blob = blob_transaction();

        let err = should_skip_disallowed_tx_type(&blob, true)
            .expect_err("anchor transaction type failures must be fatal");

        assert!(err.to_string().contains("anchor transaction type is not allowed"), "{err}");
    }

    #[test]
    fn tx_list_replay_skips_disallowed_non_anchor_tx_type() {
        let blob = blob_transaction();

        let should_skip = should_skip_disallowed_tx_type(&blob, false)
            .expect("non-anchor disallowed tx types should be skipped");

        assert!(should_skip);
    }

    #[test]
    fn tx_list_witness_options_validate_difficulty_by_default() {
        let options = TxListWitnessOptions::default();

        assert!(!options.skip_zk_gas_difficulty_check);
    }

    #[test]
    fn tx_list_witness_options_accept_camel_case_skip_flag() {
        let options: TxListWitnessOptions =
            serde_json::from_value(json!({ "skipZkGasDifficultyCheck": true })).unwrap();

        assert!(options.skip_zk_gas_difficulty_check);
    }

    #[test]
    fn check_tx_list_size_accepts_list_within_limits() {
        assert!(check_tx_list_size(&[1, 2, 3], 1024, 1024).is_ok());
    }

    #[test]
    fn check_tx_list_size_rejects_raw_over_limit() {
        // The raw guard fires on length alone, before any compression.
        let err = check_tx_list_size(&[0u8; 10], 4, usize::MAX)
            .expect_err("raw byte limit must be enforced");

        assert!(err.to_string().contains("raw limit"), "{err}");
    }

    #[test]
    fn check_tx_list_size_rejects_compressed_over_limit() {
        let raw = [7u8; 64];
        let actual = zlib_compressed_len(&raw);
        assert!(actual > 0);

        // One byte below the actual compressed size is rejected; exactly at the size is accepted.
        let err = check_tx_list_size(&raw, raw.len(), (actual - 1) as usize)
            .expect_err("compressed size limit must be enforced");
        assert!(err.to_string().contains("compressed size"), "{err}");

        assert!(check_tx_list_size(&raw, raw.len(), actual as usize).is_ok());
    }

    fn blob_transaction() -> TransactionSigned {
        let signature = Signature::new(U256::from(1u64), U256::from(1u64), false);

        Signed::new_unchecked(
            TxEip4844 {
                chain_id: 1,
                nonce: 0,
                gas_limit: 21_000,
                max_fee_per_gas: 1,
                max_priority_fee_per_gas: 0,
                to: Address::ZERO,
                value: U256::ZERO,
                access_list: Default::default(),
                blob_versioned_hashes: vec![B256::ZERO],
                max_fee_per_blob_gas: 1,
                input: Bytes::new(),
            },
            signature,
            B256::ZERO,
        )
        .into()
    }
}
