//! Live trie collector executing blocks against proof-history storage.
//!
//! Ported from the `live` module of the OP monorepo's `reth-optimism-trie` crate (last present
//! upstream at `bcf489ea`): upstream replaced it with an `engine` service driven by op-node's
//! engine flow, while Alethia's proof-history sidecar drives collection itself. The collector
//! therefore lives on here as first-party code, written against the crate's public storage API
//! (`get_proof_window`).

use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
use derive_more::Constructor;
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_optimism_trie::{
    BlockStateDiff, OpProofsStorage, OpProofsStorageError, OpProofsStore,
    api::{OpProofsProviderRO, OpProofsProviderRw, OperationDurations},
    provider::OpProofsStateProviderRef,
};
use reth_primitives_traits::{AlloyBlockHeader, BlockTy, RecoveredBlock};
use reth_provider::{
    DatabaseProviderFactory, HashedPostStateProvider, StateProviderFactory, StateReader,
    StateRootProvider,
};
use reth_revm::database::StateProviderDatabase;
use reth_trie_common::{HashedPostStateSorted, updates::TrieUpdatesSorted};
use std::{sync::Arc, time::Instant};
use tracing::info;

/// Live trie collector for external proofs storage.
#[derive(Debug, Constructor)]
pub struct LiveTrieCollector<'tx, Evm, Provider, PreimageStore>
where
    Evm: ConfigureEvm,
    Provider: StateReader + DatabaseProviderFactory + StateProviderFactory,
{
    /// EVM configuration used to re-execute blocks whose trie updates are collected.
    evm_config: Evm,
    /// Provider the collector reads parent state from.
    provider: Provider,
    /// Proof-history storage the collected trie updates are written to.
    storage: &'tx OpProofsStorage<PreimageStore>,
}

impl<'tx, Evm, Provider, Store> LiveTrieCollector<'tx, Evm, Provider, Store>
where
    Evm: ConfigureEvm,
    Provider: StateReader + DatabaseProviderFactory + StateProviderFactory,
    Store: 'tx + OpProofsStore + Clone + 'static,
{
    /// Execute a block and store the updates in the storage.
    pub fn execute_and_store_block_updates(
        &self,
        block: &RecoveredBlock<BlockTy<Evm::Primitives>>,
    ) -> Result<(), OpProofsStorageError> {
        let mut operation_durations = OperationDurations::default();

        let start = Instant::now();
        // ensure that we have the state of the parent block
        let provider_ro = self.storage.provider_ro()?;
        // Errors with `NoBlocksFound` when the proof window is empty.
        let window = provider_ro.get_proof_window()?;
        let (earliest, latest) = (window.earliest.number, window.latest.number);

        let parent_block_number = block.number() - 1;
        if parent_block_number < earliest {
            return Err(OpProofsStorageError::UnknownParent);
        }

        if parent_block_number > latest {
            return Err(OpProofsStorageError::MissingParentBlock {
                block_number: block.number(),
                parent_block_number,
                latest_block_number: latest,
            });
        }

        let block_ref =
            BlockWithParent::new(block.parent_hash(), NumHash::new(block.number(), block.hash()));

        // TODO: should we check block hash here?

        let state_provider = OpProofsStateProviderRef::new(
            self.provider.state_by_block_hash(block.parent_hash())?,
            self.storage.provider_ro()?,
            parent_block_number,
        );

        let db = StateProviderDatabase::new(&state_provider);
        let block_executor = self.evm_config.batch_executor(db);

        let execution_result = block_executor.execute(&(*block).clone())?;

        operation_durations.execution_duration_seconds = start.elapsed();

        let hashed_state = state_provider.hashed_post_state(&execution_result.state);
        let (state_root, trie_updates) =
            state_provider.state_root_with_updates(hashed_state.clone())?;

        operation_durations.state_root_duration_seconds =
            start.elapsed() - operation_durations.execution_duration_seconds;

        if state_root != block.state_root() {
            return Err(OpProofsStorageError::StateRootMismatch {
                block_number: block.number(),
                current_state_hash: state_root,
                expected_state_hash: block.state_root(),
            });
        }

        let provider_rw = self.storage.provider_rw()?;
        let update_result = provider_rw.store_trie_updates(
            block_ref,
            BlockStateDiff {
                sorted_trie_updates: trie_updates.into_sorted(),
                sorted_post_state: hashed_state.into_sorted(),
            },
        )?;
        provider_rw.commit()?;

        operation_durations.total_duration_seconds = start.elapsed();
        operation_durations.write_duration_seconds = operation_durations.total_duration_seconds -
            operation_durations.state_root_duration_seconds -
            operation_durations.execution_duration_seconds;

        info!(
            block_number = block.number(),
            ?operation_durations,
            ?update_result,
            "Block executed and trie updates stored successfully",
        );

        Ok(())
    }

    /// Store trie updates for a given block.
    pub fn store_block_updates(
        &self,
        block: BlockWithParent,
        sorted_trie_updates: TrieUpdatesSorted,
        sorted_post_state: HashedPostStateSorted,
    ) -> Result<(), OpProofsStorageError> {
        let start = Instant::now();
        let mut operation_durations = OperationDurations::default();

        let provider_rw = self.storage.provider_rw()?;
        let storage_result = provider_rw
            .store_trie_updates(block, BlockStateDiff { sorted_trie_updates, sorted_post_state })?;
        provider_rw.commit()?;

        let write_duration = start.elapsed();
        operation_durations.total_duration_seconds = write_duration;
        operation_durations.write_duration_seconds = write_duration;

        info!(
            block_number = block.block.number,
            ?operation_durations,
            ?storage_result,
            "Trie updates stored successfully",
        );

        Ok(())
    }

    /// Handles chain reorganizations by replacing block updates after a common ancestor.
    ///
    /// This method removes all block updates after the latest common ancestor (the block before
    /// the first block in `new_blocks`) and replaces them with the updates from the provided new
    /// chain. A common ancestor at the retained earliest block is supported as long as the new
    /// chain descends from the stored anchor: `replace_updates` refuses that boundary, so the
    /// window is rebuilt by unwinding to the anchor and appending the new chain in one
    /// transaction.
    ///
    /// # Arguments
    ///
    /// * `new_blocks` - A vector of references to `RecoveredBlock` instances representing the new
    ///   blocks to be added to the trie storage.
    pub fn unwind_and_store_block_updates(
        &self,
        block_updates: Vec<(BlockWithParent, Arc<TrieUpdatesSorted>, Arc<HashedPostStateSorted>)>,
    ) -> Result<(), OpProofsStorageError> {
        if block_updates.is_empty() {
            return Ok(());
        }

        let start = Instant::now();
        let mut operation_durations = OperationDurations::default();
        let first = &block_updates[0].0;
        let latest_common_block =
            BlockNumHash::new(first.block.number.saturating_sub(1), first.parent);
        let mut block_trie_updates: Vec<(BlockWithParent, BlockStateDiff)> =
            Vec::with_capacity(block_updates.len());

        for (block, trie_updates, hashed_state) in &block_updates {
            block_trie_updates.push((
                *block,
                BlockStateDiff {
                    sorted_trie_updates: (**trie_updates).clone(),
                    sorted_post_state: (**hashed_state).clone(),
                },
            ));
        }

        let earliest = self.storage.provider_ro()?.get_proof_window()?.earliest;
        let provider_rw = self.storage.provider_rw()?;
        if latest_common_block.number == earliest.number {
            // `replace_updates` refuses a common ancestor at the window's earliest block, but
            // that is exactly where an ordinary reorg lands right after initialization (or an
            // unwind) collapsed the window onto its anchor: the anchor itself stays, only the
            // blocks above it are replaced. The new chain must descend from the stored anchor.
            if latest_common_block.hash != earliest.hash {
                return Err(OpProofsStorageError::OutOfOrder {
                    block_number: first.block.number,
                    parent_block_hash: first.parent,
                    latest_block_hash: earliest.hash,
                });
            }
            block_trie_updates.sort_unstable_by_key(|(block, _)| block.block.number);
            // `unwind_history` only reads the unwind number and the parent that becomes the new
            // latest block; the replaced block's hash is not tracked here, so label the unwind
            // with the replacement block at that height.
            let unwind_to = BlockWithParent::new(
                earliest.hash,
                NumHash::new(earliest.number.saturating_add(1), first.block.hash),
            );
            provider_rw.unwind_history(unwind_to)?;
            for (block, diff) in block_trie_updates {
                provider_rw.store_trie_updates(block, diff)?;
            }
        } else {
            provider_rw.replace_updates(latest_common_block, block_trie_updates)?;
        }
        provider_rw.commit()?;
        let write_duration = start.elapsed();
        operation_durations.total_duration_seconds = write_duration;
        operation_durations.write_duration_seconds = write_duration;

        info!(
            start_block_number = block_updates.first().map(|(b, _, _)| b.block.number),
            end_block_number = block_updates.last().map(|(b, _, _)| b.block.number),
            ?operation_durations,
            "Trie updates rewound and stored successfully",
        );
        Ok(())
    }

    /// Remove account, storage and trie updates from historical storage for all blocks from
    /// the specified block (inclusive).
    pub fn unwind_history(&self, to: BlockWithParent) -> Result<(), OpProofsStorageError> {
        let provider_rw = self.storage.provider_rw()?;
        provider_rw.unwind_history(to)?;
        provider_rw.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use reth_chainspec::{ChainSpec, ChainSpecBuilder, MAINNET};
    use reth_db::Database;
    use reth_db_common::init::init_genesis;
    use reth_ethereum_primitives::{Block, BlockBody};
    use reth_evm_ethereum::EthEvmConfig;
    use reth_optimism_trie::{
        RethTrieStorageLayout, db::MdbxProofsStorageV2, initialize::InitializationJob,
    };
    use reth_primitives_traits::Block as _;
    use reth_provider::{
        StorageSettingsCache,
        providers::BlockchainProvider,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory_with_chain_spec},
    };
    use tempfile::TempDir;

    /// Paris-activated chain spec on the mainnet genesis; empty blocks keep the genesis root.
    fn test_chain_spec() -> Arc<ChainSpec> {
        Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(MAINNET.genesis.clone())
                .paris_activated()
                .build(),
        )
    }

    /// Empty block at `number` on top of `parent_hash` claiming `state_root`.
    fn empty_block(number: u64, parent_hash: B256, state_root: B256) -> RecoveredBlock<Block> {
        Block {
            header: Header { parent_hash, number, state_root, ..Default::default() },
            body: BlockBody::default(),
        }
        .try_into_recovered()
        .expect("empty block recovers without senders")
    }

    /// Genesis-initialized blockchain provider plus proofs storage seeded at block zero.
    fn genesis_fixture(
        chain_spec: &Arc<ChainSpec>,
    ) -> (BlockchainProvider<MockNodeTypesWithDB>, OpProofsStorage<Arc<MdbxProofsStorageV2>>) {
        let factory = create_test_provider_factory_with_chain_spec(chain_spec.clone());
        init_genesis(&factory).expect("genesis state initializes");

        let path = TempDir::new().expect("temp dir").keep();
        let storage: OpProofsStorage<Arc<MdbxProofsStorageV2>> =
            Arc::new(MdbxProofsStorageV2::new(&path).expect("mdbx proofs storage opens")).into();

        let layout = if factory.cached_storage_settings().is_v2() {
            RethTrieStorageLayout::Packed
        } else {
            RethTrieStorageLayout::Legacy
        };
        let tx = factory.db_ref().tx().expect("read transaction opens");
        InitializationJob::new(storage.clone(), tx, layout)
            .run(0, chain_spec.genesis_hash())
            .expect("proofs storage initializes to genesis");

        let provider = BlockchainProvider::new(factory).expect("blockchain provider");
        (provider, storage)
    }

    /// Latest block recorded in the proofs storage window.
    fn stored_latest(storage: &OpProofsStorage<Arc<MdbxProofsStorageV2>>) -> NumHash {
        storage
            .provider_ro()
            .expect("read provider opens")
            .get_proof_window()
            .expect("proof window exists")
            .latest
    }

    #[test]
    fn collector_executes_and_stores_an_empty_block() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        let genesis_root = chain_spec.genesis_header().state_root;
        let block = empty_block(1, chain_spec.genesis_hash(), genesis_root);

        collector.execute_and_store_block_updates(&block).expect("empty block stores cleanly");
        assert_eq!(stored_latest(&storage), NumHash::new(1, block.hash()));
    }

    #[test]
    fn collector_rejects_a_block_beyond_the_stored_window() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        // Parent 2 is past the window (storage only holds genesis), so collection must refuse.
        let block = empty_block(3, B256::repeat_byte(0x11), B256::ZERO);
        let err = collector.execute_and_store_block_updates(&block).unwrap_err();
        assert!(matches!(err, OpProofsStorageError::MissingParentBlock { .. }), "got {err:?}");
    }

    #[test]
    fn collector_rejects_a_block_with_a_wrong_state_root() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        let block = empty_block(1, chain_spec.genesis_hash(), B256::repeat_byte(0xAA));
        let err = collector.execute_and_store_block_updates(&block).unwrap_err();
        assert!(matches!(err, OpProofsStorageError::StateRootMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn collector_stores_precomputed_updates_and_unwinds_them() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        let block_hash = B256::repeat_byte(0x01);
        let block_ref =
            BlockWithParent::new(chain_spec.genesis_hash(), NumHash::new(1, block_hash));
        collector
            .store_block_updates(
                block_ref,
                TrieUpdatesSorted::default(),
                HashedPostStateSorted::default(),
            )
            .expect("precomputed updates store cleanly");
        assert_eq!(stored_latest(&storage), NumHash::new(1, block_hash));

        collector.unwind_history(block_ref).expect("stored block unwinds");
        assert_eq!(stored_latest(&storage), NumHash::new(0, chain_spec.genesis_hash()));
    }

    #[test]
    fn collector_replaces_blocks_after_the_common_ancestor() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        // Grow the window to [0, 2] and reorg block 2 on top of block 1: a common ancestor
        // strictly above the earliest block takes the `replace_updates` path.
        let block_one =
            BlockWithParent::new(chain_spec.genesis_hash(), NumHash::new(1, B256::repeat_byte(1)));
        let original =
            BlockWithParent::new(block_one.block.hash, NumHash::new(2, B256::repeat_byte(2)));
        for block in [block_one, original] {
            collector
                .store_block_updates(
                    block,
                    TrieUpdatesSorted::default(),
                    HashedPostStateSorted::default(),
                )
                .expect("canonical block stores cleanly");
        }

        let replacement =
            BlockWithParent::new(block_one.block.hash, NumHash::new(2, B256::repeat_byte(3)));
        collector
            .unwind_and_store_block_updates(vec![(
                replacement,
                Arc::new(TrieUpdatesSorted::default()),
                Arc::new(HashedPostStateSorted::default()),
            )])
            .expect("reorg replacement stores cleanly");
        assert_eq!(stored_latest(&storage), replacement.block);

        // An empty update set is a no-op.
        collector.unwind_and_store_block_updates(vec![]).expect("empty replacement is a no-op");
        assert_eq!(stored_latest(&storage), replacement.block);
    }

    #[test]
    fn collector_replaces_blocks_at_the_earliest_window_boundary() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        // Window [0, 1]: the genesis anchor plus one stored block. Reorging block 1 makes the
        // common ancestor exactly the retained earliest block — the state right after
        // initialization, when any reorg of the first collected block lands on the anchor.
        let original =
            BlockWithParent::new(chain_spec.genesis_hash(), NumHash::new(1, B256::repeat_byte(2)));
        collector
            .store_block_updates(
                original,
                TrieUpdatesSorted::default(),
                HashedPostStateSorted::default(),
            )
            .expect("canonical block stores cleanly");

        let replacement_one =
            BlockWithParent::new(chain_spec.genesis_hash(), NumHash::new(1, B256::repeat_byte(3)));
        let replacement_two =
            BlockWithParent::new(replacement_one.block.hash, NumHash::new(2, B256::repeat_byte(4)));
        collector
            .unwind_and_store_block_updates(vec![
                (
                    replacement_one,
                    Arc::new(TrieUpdatesSorted::default()),
                    Arc::new(HashedPostStateSorted::default()),
                ),
                (
                    replacement_two,
                    Arc::new(TrieUpdatesSorted::default()),
                    Arc::new(HashedPostStateSorted::default()),
                ),
            ])
            .expect("boundary reorg replaces blocks above the retained anchor");
        assert_eq!(stored_latest(&storage), replacement_two.block);
    }

    #[test]
    fn collector_rejects_a_boundary_reorg_not_descending_from_the_anchor() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        let original =
            BlockWithParent::new(chain_spec.genesis_hash(), NumHash::new(1, B256::repeat_byte(2)));
        collector
            .store_block_updates(
                original,
                TrieUpdatesSorted::default(),
                HashedPostStateSorted::default(),
            )
            .expect("canonical block stores cleanly");

        // The replacement claims a common ancestor at the earliest height but with a different
        // hash than the retained anchor: the new chain does not descend from stored state.
        let replacement =
            BlockWithParent::new(B256::repeat_byte(0xEE), NumHash::new(1, B256::repeat_byte(3)));
        let err = collector
            .unwind_and_store_block_updates(vec![(
                replacement,
                Arc::new(TrieUpdatesSorted::default()),
                Arc::new(HashedPostStateSorted::default()),
            )])
            .unwrap_err();
        assert!(matches!(err, OpProofsStorageError::OutOfOrder { .. }), "got {err:?}");
    }
}
