//! Taiko `eth` namespace RPC methods backed by Taiko DB tables.
use crate::eth::{
    cached_historical::CachedHistoricalStateProvider,
    error::{TaikoApiError, internal_eth_error},
};
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
    BlockExecutionOutput, BlockHashReader, BlockNumReader, BlockReaderIdExt, ChangeSetReader,
    DBProvider, DatabaseProviderFactory, HeaderProvider, NodePrimitivesProvider,
    RocksDBProviderFactory, StateProviderBox, StateProviderFactory, StorageChangeSetReader,
    StorageSettingsCache,
};
use reth_revm::{database::StateProviderDatabase, witness::ExecutionWitnessRecord};
use reth_rpc_eth_types::EthApiError;
use std::sync::Arc;

/// Shared historical state provider reused across all blocks in one range request.
type RangeHistoricalStateProvider<Provider> =
    Arc<CachedHistoricalStateProvider<<Provider as DatabaseProviderFactory>::Provider>>;

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
}

impl<Provider> TaikoExt<Provider>
where
    Provider: DatabaseProviderFactory + BlockReaderIdExt,
{
    /// Creates a new instance of `TaikoExt` with the given dependencies.
    pub fn new(provider: Provider, evm_config: TaikoEvmConfig) -> Self {
        Self { provider, evm_config }
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
        + BlockHashReader
        + ChangeSetReader
        + NodePrimitivesProvider
        + RocksDBProviderFactory
        + StorageChangeSetReader
        + StorageSettingsCache
        + Send
        + Sync
        + 'static,
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
        let is_contiguous = blocks.len() == expected_len &&
            blocks.first().map(|block| block.number()) == Some(start_block) &&
            blocks.last().map(|block| block.number()) == Some(end_block) &&
            blocks.windows(2).all(|window| {
                window[0].number().saturating_add(1) == window[1].number() &&
                    window[0].hash() == window[1].parent_hash()
            });

        if !is_contiguous {
            return Err(EthApiError::HeaderRangeNotFound(
                BlockId::Number(BlockNumberOrTag::Number(start_block)),
                BlockId::Number(BlockNumberOrTag::Number(end_block)),
            ));
        }

        Ok(blocks)
    }

    /// Creates the shared historical provider for the requested range.
    ///
    /// `CachedHistoricalStateProvider` follows upstream `HistoricalStateProvider` semantics:
    /// `block_number` points at the target block, and historical reads observe `block_number - 1`.
    fn range_historical_state_provider(
        &self,
        first_block: u64,
    ) -> Result<RangeHistoricalStateProvider<Provider>, EthApiError> {
        let db_provider = self.provider.database_provider_ro().map_err(internal_eth_error)?;
        Ok(Arc::new(CachedHistoricalStateProvider::new(db_provider, first_block)))
    }

    /// Builds the execution provider for the next block in the range.
    fn range_block_state_provider(
        &self,
        historical: &RangeHistoricalStateProvider<Provider>,
        overlay_blocks: &[ExecutedBlock<EthPrimitives>],
    ) -> Result<StateProviderBox, EthApiError> {
        if overlay_blocks.is_empty() {
            return Ok(Box::new(Arc::clone(historical)));
        }

        Ok(MemoryOverlayStateProvider::new(
            Box::new(Arc::clone(historical)),
            overlay_blocks.to_vec(),
        )
        .boxed())
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
        historical: &RangeHistoricalStateProvider<Provider>,
        execution_overlay: &mut Vec<ExecutedBlock<EthPrimitives>>,
        block: reth_primitives_traits::RecoveredBlock<Block>,
    ) -> Result<ExecutionWitness, EthApiError> {
        let mut executor = self.evm_config.executor(StateProviderDatabase::new(
            self.range_block_state_provider(historical, execution_overlay.as_slice())?,
        ));
        let result = executor.execute_one(&block).map_err(internal_eth_error)?;
        let mut db = executor.into_state();
        let ExecutionWitnessRecord { hashed_state, codes, keys, lowest_block_number } =
            ExecutionWitnessRecord::from_executed_state(&db);

        let witness_state = db
            .database
            .0
            .witness(Default::default(), hashed_state.clone())
            .map_err(EthApiError::from)?;
        let (state_root, trie_updates) = db
            .database
            .0
            .state_root_with_updates(hashed_state.clone())
            .map_err(EthApiError::from)?;
        let block_hashed_state = Arc::new(hashed_state.clone_into_sorted());

        if state_root != block.state_root() {
            return Err(internal_eth_error(format!(
                "computed state root mismatch for block {}: expected {:?}, got {:?}",
                block.number(),
                block.state_root(),
                state_root
            )));
        }

        let block_trie_updates = Arc::new(trie_updates.into_sorted());
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
        + BlockHashReader
        + ChangeSetReader
        + NodePrimitivesProvider
        + RocksDBProviderFactory
        + StorageChangeSetReader
        + StorageSettingsCache
        + Send
        + Sync
        + 'static,
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
        let historical = self.range_historical_state_provider(start_block)?;

        let mut execution_overlay = Vec::with_capacity(blocks.len());
        let mut witnesses = Vec::with_capacity(blocks.len());

        for block in blocks {
            witnesses.push(self.execution_witness_for_block(
                &historical,
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
    fn parse_witness_range_accepts_single_block() {
        let range = parse_witness_block_range(U256::from(42_u64), U256::from(42_u64)).unwrap();
        assert_eq!(range, (42, 42));
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
