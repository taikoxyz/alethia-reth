//! Filtered block reconstruction helpers for txlist-driven proving paths.

use alloy_consensus::transaction::Recovered;
use reth_ethereum_primitives::{Block, EthPrimitives, Receipt, TransactionSigned};
use reth_evm::{
    ConfigureEvm,
    block::{BlockExecutionError, BlockValidationError},
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_execution_types::BlockExecutionResult;
use reth_primitives_traits::{RecoveredBlock, SealedHeader, SignedTransaction};
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_storage_api::StateProvider;
use reth_trie_common::{HashedPostState, updates::TrieUpdates};

use crate::config::{TaikoEvmConfig, TaikoNextBlockEnvAttributes};

/// Filtered block execution artifacts returned by txlist-driven reconstruction.
#[derive(Debug)]
pub struct FilteredBlockExecutionOutcome {
    /// The block assembled from transactions that were actually committed.
    pub filtered_block: RecoveredBlock<Block>,
    /// The execution result produced while building the filtered block.
    pub execution_result: BlockExecutionResult<Receipt>,
    /// The hashed post-state after execution.
    pub hashed_state: HashedPostState,
    /// Trie updates collected during state-root calculation.
    pub trie_updates: TrieUpdates,
}

impl From<BlockBuilderOutcome<EthPrimitives>> for FilteredBlockExecutionOutcome {
    fn from(outcome: BlockBuilderOutcome<EthPrimitives>) -> Self {
        Self {
            filtered_block: outcome.block,
            execution_result: outcome.execution_result,
            hashed_state: outcome.hashed_state,
            trie_updates: outcome.trie_updates,
        }
    }
}

fn execute_candidate_transaction<B>(
    builder: &mut B,
    tx: TransactionSigned,
) -> Result<(), BlockExecutionError>
where
    B: BlockBuilder<Primitives = EthPrimitives>,
{
    let recovered = match tx.try_into_recovered() {
        Ok(recovered) => recovered,
        Err(_) => return Ok(()),
    };

    match builder.execute_transaction(recovered) {
        Ok(_) => Ok(()),
        Err(
            BlockExecutionError::Validation(BlockValidationError::InvalidTx { .. })
            | BlockExecutionError::Validation(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas { .. },
            ),
        ) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Executes `anchor + txlist` against the provided pre-state and assembles the filtered block.
///
/// The optional `anchor_tx` is always executed first and treated as fatal if execution fails. All
/// non-anchor candidate transactions are processed in order; invalid-signature transactions are
/// skipped before EVM execution, while nonce/balance/gas-invalid transactions are skipped when the
/// builder rejects them.
pub fn execute_and_filter_block_transactions<P>(
    evm_config: &TaikoEvmConfig,
    parent_header: &SealedHeader,
    block_env: TaikoNextBlockEnvAttributes,
    anchor_tx: Option<Recovered<TransactionSigned>>,
    transactions: Vec<TransactionSigned>,
    state_provider: P,
) -> Result<FilteredBlockExecutionOutcome, BlockExecutionError>
where
    P: StateProvider + Clone,
{
    let mut db = State::builder()
        .with_database(StateProviderDatabase::new(state_provider.clone()))
        .with_bundle_update()
        .build();

    let mut builder = evm_config
        .builder_for_next_block(&mut db, parent_header, block_env)
        .map_err(BlockExecutionError::other)?;

    builder.apply_pre_execution_changes()?;

    if let Some(anchor_tx) = anchor_tx {
        builder.execute_transaction(anchor_tx)?;
    }

    for tx in transactions {
        execute_candidate_transaction(&mut builder, tx)?;
    }

    builder.finish(state_provider, None).map(Into::into)
}
