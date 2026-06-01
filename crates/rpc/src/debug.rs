//! Proof-history backed overrides for selected `debug_` RPC methods.

use crate::proof_state::ProofHistoryStateProviderFactory;
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
use alloy_eips::{BlockId, BlockNumberOrTag, eip4844::BYTES_PER_BLOB};
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
use reth_provider::HeaderProvider;
use reth_revm::{
    State, database::StateProviderDatabase, db::states::bundle_state::BundleRetention,
    witness::ExecutionWitnessRecord,
};
use reth_rpc_eth_api::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use reth_trie_common::ExecutionWitnessMode;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

/// Maximum number of concurrent proof-history witness requests.
const MAX_CONCURRENT_WITNESS_REQUESTS: usize = 3;

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
    /// Semaphore limiting concurrent witness generation.
    semaphore: Semaphore,
}

impl<Eth, Storage, Provider> TaikoDebugWitnessExt<Eth, Storage, Provider>
where
    Eth: FullEthApi<Primitives = EthPrimitives> + Send + Sync + 'static,
    Storage: OpProofsStore + Clone + 'static,
    Provider: HeaderProvider + Clone + Send + Sync + 'static,
    Provider::Header: BlockHeader + alloy_rlp::Encodable,
{
    /// Creates a new proof-history backed debug witness override.
    pub fn new(provider: Provider, eth_api: Eth, storage: OpProofsStorage<Storage>) -> Self {
        Self {
            provider,
            state_provider_factory: ProofHistoryStateProviderFactory::new(eth_api.clone(), storage),
            eth_api,
            semaphore: Semaphore::new(MAX_CONCURRENT_WITNESS_REQUESTS),
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
        self.execution_witness_for_block(block.as_ref(), mode).await
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
        let txs = decode_recovered_tx_list(tx_list)?;
        let block = block_with_tx_list(block.as_ref().clone(), txs);
        self.execution_witness_for_tx_list_block(&block, mode, options).await
    }

    /// Re-executes the provided block against proof-history backed parent state and returns the
    /// generated witness.
    async fn execution_witness_for_block(
        &self,
        block: &RecoveredBlock<Block>,
        mode: ExecutionWitnessMode,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block_number = block.header().number();
        let parent_block = BlockId::Hash(block.parent_hash().into());
        let state_provider = self
            .state_provider_factory
            .state_provider(parent_block)
            .await
            .map_err(EthApiError::from)?;
        let db = StateProviderDatabase::new(&*state_provider);
        let block_executor = self.eth_api.evm_config().executor(db);
        let mut witness_record = ExecutionWitnessRecord::default();

        block_executor
            .execute_with_state_closure(block, |statedb: &State<_>| {
                witness_record.record_executed_state(statedb, mode);
            })
            .map_err(EthApiError::from)?;

        Ok(witness_record
            .into_execution_witness(&*state_provider, &self.provider, block_number, mode)
            .map_err(EthApiError::from)?)
    }

    /// Replays the explicit transaction-list block with prover-style filtering, without enabling
    /// the block crate's global `prover` feature for normal node execution.
    async fn execution_witness_for_tx_list_block(
        &self,
        block: &RecoveredBlock<Block>,
        mode: ExecutionWitnessMode,
        options: TxListWitnessOptions,
    ) -> Result<ExecutionWitness, Eth::Error> {
        let block_number = block.header().number();
        let parent_block = BlockId::Hash(block.parent_hash().into());
        let state_provider = self
            .state_provider_factory
            .state_provider(parent_block)
            .await
            .map_err(EthApiError::from)?;
        let db = StateProviderDatabase::new(&*state_provider);
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut witness_record = ExecutionWitnessRecord::default();

        {
            let mut block_executor = self
                .eth_api
                .evm_config()
                .executor_for_block(&mut state, block.sealed_block())
                .map_err(|err| EthApiError::EvmCustom(err.to_string()))?;

            block_executor.apply_pre_execution_changes().map_err(EthApiError::from)?;

            for (idx, tx) in block.transactions_recovered().enumerate() {
                let is_anchor_transaction = idx == 0;

                // Taiko blocks never contain blob transactions: the build paths skip non-anchor
                // ones (crates/payload/src/builder/execution.rs) and consensus validation rejects
                // any block that includes one. Keep the same filtering, but never silently discard
                // the mandatory anchor transaction.
                match should_skip_disallowed_tx_type(tx.inner(), is_anchor_transaction) {
                    Ok(true) => {
                        continue;
                    }
                    Ok(false) => {}
                    Err(err) => return Err(err.into()),
                }

                match block_executor.execute_transaction(tx) {
                    Ok(_) => {}
                    Err(err) if is_recoverable_tx_list_error(&err, is_anchor_transaction) => {
                        if is_zk_gas_limit_exceeded(&err) {
                            break;
                        }
                    }
                    Err(err) => return Err(EthApiError::from(err).into()),
                }
            }

            match block_executor.apply_post_execution_changes() {
                Ok(_) => {}
                Err(err)
                    if options.skip_zk_gas_difficulty_check
                        && is_zk_gas_difficulty_mismatch(&err) => {}
                Err(err) => return Err(EthApiError::from(err).into()),
            }
        }

        state.merge_transitions(BundleRetention::Reverts);
        witness_record.record_executed_state(&state, mode);

        Ok(witness_record
            .into_execution_witness(&*state_provider, &self.provider, block_number, mode)
            .map_err(EthApiError::from)?)
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
        let _permit = self.semaphore.acquire().await;
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
        let _permit = self.semaphore.acquire().await;
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
        let _permit = self.semaphore.acquire().await;
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
/// mandatory block setup, so silently skipping it would produce a witness for a block the node would
/// never execute.
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
    use alloy_consensus::{Signed, TxEip4844, TxLegacy};
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
    use alloy_rlp::Encodable;
    use reth_evm::execute::BlockValidationError;
    use serde_json::json;

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
