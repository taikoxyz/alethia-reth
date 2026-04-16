//! Taiko `eth` namespace RPC methods backed by Taiko DB tables.
use crate::eth::error::{TaikoApiError, internal_eth_error};
use alethia_reth_block::config::TaikoEvmConfig;
use alethia_reth_db::model::{
    STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable,
};
use alethia_reth_primitives::payload::attributes::RpcL1Origin;
use alloy_consensus::{BlockHeader, Header};
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Bytes, U256};
use alloy_rlp::Encodable;
use alloy_rpc_types_debug::ExecutionWitness;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth_chain_state::{ComputedTrieData, ExecutedBlock, MemoryOverlayStateProvider};
use reth_db_api::transaction::DbTx;
use reth_ethereum::{Block, EthPrimitives};
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_provider::{
    BlockExecutionOutput, BlockNumReader, BlockReaderIdExt, ChangeSetReader, DBProvider,
    DatabaseProviderFactory, HeaderProvider, StageCheckpointReader, StateProviderBox,
    StateProviderFactory, StorageChangeSetReader, StorageSettingsCache,
};
use reth_revm::{database::StateProviderDatabase, witness::ExecutionWitnessRecord};
use reth_rpc_eth_types::EthApiError;
use reth_trie::{
    HashedPostState, HashedPostStateSorted, StateRoot, TrieInputSorted,
    hashed_cursor::HashedPostStateCursorFactory,
    trie_cursor::InMemoryTrieCursorFactory,
    updates::{TrieUpdates, TrieUpdatesSorted},
    witness::TrieWitness,
};
use reth_trie_db::{
    ChangesetCache, DatabaseHashedCursorFactory, DatabaseStateRoot, DatabaseTrieCursorFactory,
    from_reverts_auto,
};
use std::sync::Arc;

/// Trait interface for the custom `taiko` RPC namespace.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "taiko"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "taiko"))]
pub trait TaikoExtApi {
    /// Return the stored L1 origin by L2 block id.
    #[method(name = "l1OriginByID")]
    fn l1_origin_by_id(&self, id: U256) -> RpcResult<Option<RpcL1Origin>>;

    /// Return the latest non-preconfirmation L1 origin pointer.
    #[method(name = "headL1Origin")]
    fn head_l1_origin(&self) -> RpcResult<Option<RpcL1Origin>>;

    /// Return execution witnesses for an inclusive canonical block range.
    #[method(name = "executionWitnessRange")]
    fn execution_witness_range(
        &self,
        start_block: U256,
        end_block: U256,
    ) -> RpcResult<Vec<ExecutionWitness>>;
}

/// Shared historical revert overlay reused across a contiguous witness request.
#[derive(Clone, Debug)]
struct HistoricalWitnessBase {
    /// Trie-node reverts that restore the state at the parent of the first requested block.
    trie_nodes: Arc<TrieUpdatesSorted>,
    /// Hashed-state reverts that restore the state at the parent of the first requested block.
    hashed_state: Arc<HashedPostStateSorted>,
}

/// Mutable overlay representing the parent state of the next block in the current range.
#[derive(Clone, Debug)]
struct WitnessRangeOverlay {
    /// Trie nodes for the current parent state.
    trie_nodes: Arc<TrieUpdatesSorted>,
    /// Hashed state for the current parent state.
    hashed_state: Arc<HashedPostStateSorted>,
}

impl WitnessRangeOverlay {
    /// Creates the first range overlay from the shared historical base.
    fn from_base(base: &HistoricalWitnessBase) -> Self {
        Self {
            trie_nodes: Arc::clone(&base.trie_nodes),
            hashed_state: Arc::clone(&base.hashed_state),
        }
    }

    /// Extends the hashed-state overlay with the executed block result.
    fn extend_hashed_state(&mut self, block_hashed_state: &Arc<HashedPostStateSorted>) {
        Arc::make_mut(&mut self.hashed_state).extend_ref_and_sort(block_hashed_state.as_ref());
    }

    /// Extends the trie-node overlay with the executed block trie updates.
    fn extend_trie_nodes(&mut self, block_trie_updates: &Arc<TrieUpdatesSorted>) {
        Arc::make_mut(&mut self.trie_nodes).extend_ref_and_sort(block_trie_updates.as_ref());
    }
}

/// The Taiko RPC extension implementation.
#[derive(Clone)]
pub struct TaikoExt<Provider>
where
    Provider: DatabaseProviderFactory + BlockReaderIdExt,
{
    /// Provider used for chain and state access.
    provider: Provider,
    /// EVM configuration used to re-execute blocks.
    evm_config: TaikoEvmConfig,
    /// Trie changeset cache used to amortize revert reconstruction across a range.
    changeset_cache: ChangesetCache,
}

/// Database-backed `StateRoot` calculator specialized with a concrete trie table adapter.
type DbStateRoot<'a, TX, A> =
    StateRoot<DatabaseTrieCursorFactory<&'a TX, A>, DatabaseHashedCursorFactory<&'a TX>>;

impl<Provider> TaikoExt<Provider>
where
    Provider: DatabaseProviderFactory + BlockReaderIdExt,
{
    /// Creates a new instance of `TaikoExt` with the given dependencies.
    pub fn new(
        provider: Provider,
        evm_config: TaikoEvmConfig,
        changeset_cache: ChangesetCache,
    ) -> Self {
        Self { provider, evm_config, changeset_cache }
    }
}

impl<Provider> TaikoExt<Provider>
where
    Provider: DatabaseProviderFactory
        + BlockReaderIdExt<Block = Block, Header = Header>
        + HeaderProvider<Header = Header>
        + StateProviderFactory
        + 'static,
    Provider::Provider: DBProvider
        + BlockNumReader
        + ChangeSetReader
        + StageCheckpointReader
        + StorageChangeSetReader
        + StorageSettingsCache,
{
    /// Loads a contiguous inclusive block range with recovered senders.
    fn recovered_block_range(
        &self,
        start_block: u64,
        end_block: u64,
    ) -> Result<Vec<reth_primitives_traits::RecoveredBlock<Block>>, EthApiError> {
        let blocks = self
            .provider
            .block_with_senders_range(start_block..=end_block)
            .map_err(EthApiError::from)?;

        let expected_len = end_block.saturating_sub(start_block).saturating_add(1) as usize;
        let is_contiguous = blocks.len() == expected_len
            && blocks.first().map(|block| block.number()) == Some(start_block)
            && blocks.last().map(|block| block.number()) == Some(end_block)
            && blocks.windows(2).all(|window| {
                window[0].number().saturating_add(1) == window[1].number()
                    && window[0].hash() == window[1].parent_hash()
            });

        if !is_contiguous {
            return Err(EthApiError::HeaderRangeNotFound(
                BlockId::Number(BlockNumberOrTag::Number(start_block)),
                BlockId::Number(BlockNumberOrTag::Number(end_block)),
            ));
        }

        Ok(blocks)
    }

    /// Precomputes the historical revert overlay shared by every block in the requested range.
    fn historical_witness_base(
        &self,
        first_block: u64,
    ) -> Result<HistoricalWitnessBase, EthApiError> {
        let db_provider = self.provider.database_provider_ro().map_err(internal_eth_error)?;
        let db_tip = self
            .provider
            .latest_header()
            .map_err(EthApiError::from)?
            .map(|header| header.number())
            .ok_or(EthApiError::HeaderNotFound(BlockId::Number(BlockNumberOrTag::Latest)))?;
        let trie_nodes = self
            .changeset_cache
            .get_or_compute_range(&db_provider, first_block..=db_tip)
            .map_err(EthApiError::from)?;
        let hashed_state =
            from_reverts_auto(&db_provider, first_block..).map_err(internal_eth_error)?;

        Ok(HistoricalWitnessBase {
            trie_nodes: Arc::new(trie_nodes),
            hashed_state: Arc::new(hashed_state),
        })
    }

    /// Builds the execution provider for the next block in the range.
    fn execution_state_provider(
        &self,
        anchor_hash: alloy_primitives::B256,
        overlay_blocks: &[ExecutedBlock<EthPrimitives>],
    ) -> Result<StateProviderBox, EthApiError> {
        let historical =
            self.provider.state_by_block_hash(anchor_hash).map_err(EthApiError::from)?;
        if overlay_blocks.is_empty() {
            return Ok(historical);
        }

        Ok(MemoryOverlayStateProvider::new(historical, overlay_blocks.to_vec()).boxed())
    }

    /// Computes the witness trie nodes for a block on top of the current parent overlay.
    fn compute_witness_state(
        &self,
        overlay: &WitnessRangeOverlay,
        hashed_state: HashedPostState,
    ) -> Result<Vec<Bytes>, EthApiError> {
        let db_provider = self.provider.database_provider_ro().map_err(internal_eth_error)?;
        let tx = db_provider.tx_ref();

        reth_trie_db::with_adapter!(db_provider, |A| {
            TrieWitness::new(
                InMemoryTrieCursorFactory::new(
                    DatabaseTrieCursorFactory::<_, A>::new(tx),
                    overlay.trie_nodes.as_ref(),
                ),
                HashedPostStateCursorFactory::new(
                    DatabaseHashedCursorFactory::new(tx),
                    overlay.hashed_state.as_ref(),
                ),
            )
            .always_include_root_node()
            .compute(hashed_state)
            .map_err(internal_eth_error)
            .map(|nodes| nodes.into_values().collect())
        })
    }

    /// Computes the post-block state root and trie updates for the current block.
    fn compute_state_root_with_updates(
        &self,
        overlay: &WitnessRangeOverlay,
        block_hashed_state: &HashedPostState,
    ) -> Result<(alloy_primitives::B256, TrieUpdates), EthApiError> {
        let db_provider = self.provider.database_provider_ro().map_err(internal_eth_error)?;
        let tx = db_provider.tx_ref();
        let input = TrieInputSorted::new(
            Arc::clone(&overlay.trie_nodes),
            Arc::clone(&overlay.hashed_state),
            block_hashed_state.construct_prefix_sets(),
        );

        reth_trie_db::with_adapter!(db_provider, |A| {
            <DbStateRoot<'_, _, A> as DatabaseStateRoot<_>>::overlay_root_from_nodes_with_updates(
                tx, input,
            )
            .map_err(internal_eth_error)
        })
    }

    /// Serializes the headers required to validate BLOCKHASH and pre-state references.
    fn witness_headers(
        &self,
        start_block: u64,
        end_block_exclusive: u64,
    ) -> Result<Vec<Bytes>, EthApiError> {
        self.provider
            .headers_range(start_block..end_block_exclusive)
            .map_err(EthApiError::from)
            .map(|headers| headers.into_iter().map(serialize_header).collect())
    }

    /// Re-executes one block in the range and produces its execution witness.
    fn execution_witness_for_block(
        &self,
        anchor_hash: alloy_primitives::B256,
        overlay: &mut WitnessRangeOverlay,
        execution_overlay: &mut Vec<ExecutedBlock<EthPrimitives>>,
        block: reth_primitives_traits::RecoveredBlock<Block>,
    ) -> Result<ExecutionWitness, EthApiError> {
        let mut executor = self.evm_config.executor(StateProviderDatabase::new(
            self.execution_state_provider(anchor_hash, execution_overlay.as_slice())?,
        ));
        let result = executor.execute_one(&block).map_err(internal_eth_error)?;
        let mut db = executor.into_state();
        let ExecutionWitnessRecord { hashed_state, codes, keys, lowest_block_number } =
            ExecutionWitnessRecord::from_executed_state(&db);

        let witness_state = self.compute_witness_state(overlay, hashed_state.clone())?;
        let block_hashed_state = Arc::new(hashed_state.clone_into_sorted());
        overlay.extend_hashed_state(&block_hashed_state);

        let (state_root, trie_updates) =
            self.compute_state_root_with_updates(overlay, &hashed_state)?;
        if state_root != block.state_root() {
            return Err(internal_eth_error(format!(
                "computed state root mismatch for block {}: expected {:?}, got {:?}",
                block.number(),
                block.state_root(),
                state_root
            )));
        }

        let block_trie_updates = Arc::new(trie_updates.into_sorted());
        overlay.extend_trie_nodes(&block_trie_updates);

        let execution_output = Arc::new(BlockExecutionOutput { result, state: db.take_bundle() });
        let executed_block = ExecutedBlock::new(
            Arc::new(block.clone()),
            execution_output,
            ComputedTrieData::without_trie_input(
                Arc::clone(&block_hashed_state),
                Arc::clone(&block_trie_updates),
            ),
        );
        execution_overlay.insert(0, executed_block);

        Ok(ExecutionWitness {
            state: witness_state,
            codes,
            keys,
            headers: self.witness_headers(
                header_start_block_number(block.number(), lowest_block_number),
                block.number(),
            )?,
        })
    }
}

impl<Provider> TaikoExtApiServer for TaikoExt<Provider>
where
    Provider: DatabaseProviderFactory
        + BlockReaderIdExt<Block = Block, Header = Header>
        + HeaderProvider<Header = Header>
        + StateProviderFactory
        + 'static,
    Provider::Provider: DBProvider
        + BlockNumReader
        + ChangeSetReader
        + StageCheckpointReader
        + StorageChangeSetReader
        + StorageSettingsCache,
{
    /// Retrieves the L1 origin by its ID from the database.
    fn l1_origin_by_id(&self, id: U256) -> RpcResult<Option<RpcL1Origin>> {
        let provider = self.provider.database_provider_ro().map_err(internal_eth_error)?;

        Ok(Some(
            provider
                .into_tx()
                .get::<StoredL1OriginTable>(id.to())
                .map_err(internal_eth_error)?
                .ok_or(TaikoApiError::GethNotFound)?
                .into_rpc(),
        ))
    }

    /// Retrieves the head L1 origin from the database.
    fn head_l1_origin(&self) -> RpcResult<Option<RpcL1Origin>> {
        let provider = self.provider.database_provider_ro().map_err(internal_eth_error)?;

        self.l1_origin_by_id(U256::from(
            provider
                .into_tx()
                .get::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY)
                .map_err(internal_eth_error)?
                .ok_or(TaikoApiError::GethNotFound)?,
        ))
    }

    /// Re-executes an inclusive block range and returns per-block execution witnesses.
    fn execution_witness_range(
        &self,
        start_block: U256,
        end_block: U256,
    ) -> RpcResult<Vec<ExecutionWitness>> {
        let (start_block, end_block) = parse_witness_block_range(start_block, end_block)?;
        let blocks = self.recovered_block_range(start_block, end_block)?;
        let base = self.historical_witness_base(start_block)?;
        let anchor_hash = blocks
            .first()
            .map(|block| block.parent_hash())
            .ok_or(EthApiError::InvalidBlockRange)?;

        let mut overlay = WitnessRangeOverlay::from_base(&base);
        let mut execution_overlay = Vec::with_capacity(blocks.len());
        let mut witnesses = Vec::with_capacity(blocks.len());

        for block in blocks {
            witnesses.push(self.execution_witness_for_block(
                anchor_hash,
                &mut overlay,
                &mut execution_overlay,
                block,
            )?);
        }

        Ok(witnesses)
    }
}

/// Parses a quantity into a block number and rejects values outside the `u64` domain.
fn quantity_to_block_number(quantity: U256, name: &str) -> Result<u64, EthApiError> {
    quantity.try_into().map_err(|_| EthApiError::InvalidParams(format!("{name} exceeds u64 range")))
}

/// Validates the witness range parameters accepted by `taiko_executionWitnessRange`.
fn parse_witness_block_range(
    start_block: U256,
    end_block: U256,
) -> Result<(u64, u64), EthApiError> {
    let start_block = quantity_to_block_number(start_block, "start_block")?;
    let end_block = quantity_to_block_number(end_block, "end_block")?;

    if start_block == 0 {
        return Err(EthApiError::InvalidParams(
            "executionWitnessRange requires start_block >= 1".to_string(),
        ));
    }
    if start_block > end_block {
        return Err(EthApiError::InvalidBlockRange);
    }

    Ok((start_block, end_block))
}

/// Returns the first header number that must be attached to a witness response.
fn header_start_block_number(block_number: u64, lowest_block_number: Option<u64>) -> u64 {
    lowest_block_number.unwrap_or_else(|| block_number.saturating_sub(1))
}

/// RLP-encodes a header into the witness `headers` payload.
fn serialize_header(header: Header) -> Bytes {
    let mut encoded = Vec::new();
    header.encode(&mut encoded);
    encoded.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_witness_range_rejects_zero_start() {
        let err = parse_witness_block_range(U256::ZERO, U256::from(1_u64)).unwrap_err();
        assert!(matches!(err, EthApiError::InvalidParams(_)));
    }

    #[test]
    fn parse_witness_range_rejects_reversed_bounds() {
        let err = parse_witness_block_range(U256::from(9_u64), U256::from(8_u64)).unwrap_err();
        assert!(matches!(err, EthApiError::InvalidBlockRange));
    }

    #[test]
    fn parse_witness_range_rejects_values_above_u64() {
        let err =
            parse_witness_block_range(U256::from(u128::MAX), U256::from(u128::MAX)).unwrap_err();
        assert!(matches!(err, EthApiError::InvalidParams(_)));
    }

    #[test]
    fn header_start_uses_parent_when_blockhash_was_not_accessed() {
        assert_eq!(header_start_block_number(42, None), 41);
    }

    #[test]
    fn header_start_uses_lowest_blockhash_reference_when_present() {
        assert_eq!(header_start_block_number(42, Some(7)), 7);
    }
}
