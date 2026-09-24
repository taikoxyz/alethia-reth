//! Live trie collector executing blocks against proof-history storage.
//!
//! Ported from the `live` module of the OP monorepo's `reth-optimism-trie` crate (last present
//! upstream at `bcf489ea`): upstream replaced it with an `engine` service driven by op-node's
//! engine flow, while Alethia's proof-history sidecar drives collection itself. The collector
//! therefore lives on here as first-party code, written against the crate's public storage API
//! (`get_proof_window`).

use alloy_eips::{NumHash, eip1898::BlockWithParent};
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
use std::time::Instant;
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

        // Genesis has no parent state to execute against.
        let parent_block_number =
            block.number().checked_sub(1).ok_or(OpProofsStorageError::UnknownParent)?;
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

        // The storage only accepts appends on top of its latest block (`store_trie_updates`
        // re-checks this at write time), so require the parent to be the stored tip before
        // paying for execution and state-root collection: a reorg race would otherwise surface
        // late as a confusing state-root mismatch, and even a matching root could not be stored.
        if parent_block_number != latest || block.parent_hash() != window.latest.hash {
            return Err(OpProofsStorageError::OutOfOrder {
                block_number: block.number(),
                parent_block_hash: block.parent_hash(),
                latest_block_hash: window.latest.hash,
            });
        }

        let block_ref =
            BlockWithParent::new(block.parent_hash(), NumHash::new(block.number(), block.hash()));

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
        RethTrieStorageLayout, db::MdbxProofsStorage, initialize::InitializationJob,
    };
    use reth_primitives_traits::Block as _;
    use reth_provider::{
        StorageSettingsCache,
        providers::BlockchainProvider,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory_with_chain_spec},
    };
    use std::sync::Arc;
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
    ) -> (BlockchainProvider<MockNodeTypesWithDB>, OpProofsStorage<Arc<MdbxProofsStorage>>) {
        let factory = create_test_provider_factory_with_chain_spec(chain_spec.clone());
        init_genesis(&factory).expect("genesis state initializes");

        let path = TempDir::new().expect("temp dir").keep();
        let storage: OpProofsStorage<Arc<MdbxProofsStorage>> =
            Arc::new(MdbxProofsStorage::new(&path).expect("mdbx proofs storage opens")).into();

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
    fn stored_latest(storage: &OpProofsStorage<Arc<MdbxProofsStorage>>) -> NumHash {
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
    fn collector_rejects_a_genesis_block() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        // Block zero has no parent: the parent-height subtraction must not wrap into a window
        // probe.
        let block = empty_block(0, B256::ZERO, chain_spec.genesis_header().state_root);
        let err = collector.execute_and_store_block_updates(&block).unwrap_err();
        assert!(matches!(err, OpProofsStorageError::UnknownParent), "got {err:?}");
    }

    #[test]
    fn collector_rejects_a_block_whose_parent_is_not_the_stored_tip() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        // Parent height matches the stored tip (genesis) but the hash belongs to another fork:
        // the collector must refuse up front instead of executing toward a state-root mismatch.
        let block = empty_block(1, B256::repeat_byte(0xEE), chain_spec.genesis_header().state_root);
        let err = collector.execute_and_store_block_updates(&block).unwrap_err();
        assert!(matches!(err, OpProofsStorageError::OutOfOrder { .. }), "got {err:?}");
    }

    #[test]
    fn collector_rejects_a_block_executing_inside_the_stored_window() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        // Window [0, 1]: a block whose parent is the interior genesis block is a reorg of the
        // stored tip. The append-only storage would reject it at write time, so execution must
        // be refused up front (reorgs unwind the old branch first and then append).
        let stored =
            BlockWithParent::new(chain_spec.genesis_hash(), NumHash::new(1, B256::repeat_byte(1)));
        collector
            .store_block_updates(
                stored,
                TrieUpdatesSorted::default(),
                HashedPostStateSorted::default(),
            )
            .expect("canonical block stores cleanly");

        let block =
            empty_block(1, chain_spec.genesis_hash(), chain_spec.genesis_header().state_root);
        let err = collector.execute_and_store_block_updates(&block).unwrap_err();
        assert!(matches!(err, OpProofsStorageError::OutOfOrder { .. }), "got {err:?}");
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
    fn collector_reorgs_by_unwinding_then_appending() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        // Window [0, 2]. Reorg block 2 onto block 1 the way the sidecar applies every reorg:
        // unwind from the first replaced block, then append the new chain block by block.
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

        collector.unwind_history(original).expect("old branch unwinds");
        assert_eq!(stored_latest(&storage), block_one.block);

        let replacement =
            BlockWithParent::new(block_one.block.hash, NumHash::new(2, B256::repeat_byte(3)));
        collector
            .store_block_updates(
                replacement,
                TrieUpdatesSorted::default(),
                HashedPostStateSorted::default(),
            )
            .expect("replacement block appends onto the unwound tip");
        assert_eq!(stored_latest(&storage), replacement.block);
    }

    #[test]
    fn collector_reorgs_the_first_block_above_the_retained_anchor() {
        let chain_spec = test_chain_spec();
        let (provider, storage) = genesis_fixture(&chain_spec);
        let collector =
            LiveTrieCollector::new(EthEvmConfig::ethereum(chain_spec.clone()), provider, &storage);

        // Window [0, 1]: the genesis anchor plus one stored block, the state right after
        // initialization (or an unwind) collapsed the window onto its anchor. Reorging block 1
        // lands on the anchor itself; unwinding to it and appending needs no special case.
        let original =
            BlockWithParent::new(chain_spec.genesis_hash(), NumHash::new(1, B256::repeat_byte(2)));
        collector
            .store_block_updates(
                original,
                TrieUpdatesSorted::default(),
                HashedPostStateSorted::default(),
            )
            .expect("canonical block stores cleanly");

        collector.unwind_history(original).expect("unwinding to the anchor is allowed");
        assert_eq!(stored_latest(&storage), NumHash::new(0, chain_spec.genesis_hash()));

        let replacement_one =
            BlockWithParent::new(chain_spec.genesis_hash(), NumHash::new(1, B256::repeat_byte(3)));
        let replacement_two =
            BlockWithParent::new(replacement_one.block.hash, NumHash::new(2, B256::repeat_byte(4)));
        for block in [replacement_one, replacement_two] {
            collector
                .store_block_updates(
                    block,
                    TrieUpdatesSorted::default(),
                    HashedPostStateSorted::default(),
                )
                .expect("replacement chain appends onto the anchor");
        }
        assert_eq!(stored_latest(&storage), replacement_two.block);
    }

    #[test]
    fn collector_rejects_a_replacement_not_descending_from_the_unwound_tip() {
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
        collector.unwind_history(original).expect("old branch unwinds");

        // The replacement claims a parent other than the anchor the unwind exposed: the new
        // chain does not descend from stored state, and the append-only store refuses it.
        let replacement =
            BlockWithParent::new(B256::repeat_byte(0xEE), NumHash::new(1, B256::repeat_byte(3)));
        let err = collector
            .store_block_updates(
                replacement,
                TrieUpdatesSorted::default(),
                HashedPostStateSorted::default(),
            )
            .unwrap_err();
        assert!(matches!(err, OpProofsStorageError::OutOfOrder { .. }), "got {err:?}");
    }
}
