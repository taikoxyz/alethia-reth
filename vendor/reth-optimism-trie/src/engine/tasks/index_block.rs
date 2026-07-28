use super::super::state::EngineState;
use crate::{BlockStateDiff, OpProofsStore, engine::EngineError};
use alloy_eips::eip1898::BlockWithParent;
use crossbeam_channel::Sender;
use reth_evm::ConfigureEvm;
use reth_primitives_traits::BlockTy;
use reth_provider::{
    BlockHashReader, BlockReader, DatabaseProviderFactory, StateProviderFactory, StateReader,
};
use reth_trie_common::{HashedPostStateSorted, updates::TrieUpdatesSorted};
use std::time::Instant;
use tracing::{debug, info};

/// Request to buffer externally computed state and trie updates for one block.
pub(crate) struct IndexBlockTask {
    /// Block and parent hashes identifying the update.
    pub(crate) block: BlockWithParent,
    /// Sorted branch-node updates produced for the block.
    pub(crate) sorted_trie_updates: TrieUpdatesSorted,
    /// Sorted hashed-state changes produced for the block.
    pub(crate) sorted_post_state: HashedPostStateSorted,
    /// One-shot response channel for success or ancestry/indexing failure.
    pub(crate) reply: Sender<Result<(), EngineError>>,
}

impl IndexBlockTask {
    /// Applies the indexing request to engine state and sends the result to the caller.
    pub(crate) fn execute<Evm, Provider, Store>(self, state: &mut EngineState<Evm, Provider, Store>)
    where
        Evm: ConfigureEvm,
        Provider: BlockHashReader
            + StateReader
            + DatabaseProviderFactory
            + StateProviderFactory
            + BlockReader<Block = BlockTy<Evm::Primitives>>
            + Clone
            + 'static,
        Store: OpProofsStore + Clone + 'static,
    {
        let Self { block, sorted_trie_updates, sorted_post_state, reply } = self;
        let _ = reply.send(run(state, block, sorted_trie_updates, sorted_post_state));
    }
}

/// Validates continuity and buffers one precomputed block diff.
///
/// Already-covered blocks are skipped and gaps advance the sync target.
fn run<Evm, Provider, Store>(
    state: &mut EngineState<Evm, Provider, Store>,
    block: BlockWithParent,
    sorted_trie_updates: TrieUpdatesSorted,
    sorted_post_state: HashedPostStateSorted,
) -> Result<(), EngineError>
where
    Evm: ConfigureEvm,
    Provider: BlockHashReader
        + StateReader
        + DatabaseProviderFactory
        + StateProviderFactory
        + BlockReader<Block = BlockTy<Evm::Primitives>>
        + Clone
        + 'static,
    Store: OpProofsStore + Clone + 'static,
{
    let start = Instant::now();
    let tip = state.get_tip()?;

    if block.block.number <= tip.number {
        debug!(
            target: "trie::engine::task",
            block_number = block.block.number,
            tip_number = tip.number,
            "Block already covered by tip, skipping store_block_updates",
        );
        return Ok(());
    }

    if block.block.number > tip.number.saturating_add(1) {
        debug!(
            target: "trie::engine::task",
            block_number = block.block.number,
            tip_number = tip.number,
            "Gap detected, updating sync target",
        );
        if block.block.number > state.sync_target {
            state.sync_target = block.block.number;
        }
        return Ok(());
    }

    if block.parent != tip.hash {
        return Err(EngineError::ParentHashMismatch {
            block_number: block.block.number,
            expected_parent_hash: tip.hash,
            actual_parent_hash: block.parent,
        });
    }

    state.memory.insert(block, BlockStateDiff { sorted_trie_updates, sorted_post_state });

    #[cfg(feature = "metrics")]
    state.metrics.index_block_duration_seconds.record(start.elapsed());

    info!(
        target: "trie::engine::task",
        block_number = block.block.number,
        total_duration = ?start.elapsed(),
        "Trie updates buffered successfully",
    );

    Ok(())
}
