//! Narrow adapter around the upstream proof-history engine.

use alloy_eips::eip1898::BlockWithParent;
use reth_evm::ConfigureEvm;
use reth_optimism_trie::{EngineHandle, OpProofStoragePruner, OpProofsStore};
use reth_primitives_traits::{NodePrimitives, RecoveredBlock};
use reth_provider::{
    BlockHashReader, BlockReader, DatabaseProviderFactory, StateProviderFactory, StateReader,
};
use reth_trie_common::{HashedPostStateSorted, updates::TrieUpdatesSorted};
use std::sync::Arc;

/// Precomputed trie and hashed-state updates for a replacement chain in ascending block order.
pub(super) type ReorgBlockUpdates =
    Vec<(BlockWithParent, Arc<TrieUpdatesSorted>, Arc<HashedPostStateSorted>)>;

/// Narrow engine contract used by proof-history notification orchestration.
pub(super) trait ProofHistoryEngine<Block>: Send + 'static
where
    Block: reth_primitives_traits::Block,
{
    /// Executes an exact recovered block and buffers its verified proof-history update.
    fn execute_block(&self, block: &RecoveredBlock<Block>) -> eyre::Result<()>;

    /// Buffers already-computed trie and hashed-state updates for an exact block identity.
    fn index_block(
        &self,
        block: BlockWithParent,
        trie_updates: TrieUpdatesSorted,
        post_state: HashedPostStateSorted,
    ) -> eyre::Result<()>;

    /// Replaces the retained suffix with precomputed updates for a new canonical branch.
    fn reorg(&self, updates: ReorgBlockUpdates) -> eyre::Result<()>;

    /// Removes the supplied block and every retained or buffered descendant.
    fn unwind(&self, from: BlockWithParent) -> eyre::Result<()>;

    /// Requests background catch-up through the supplied persisted block number.
    fn sync_to(&self, target: u64) -> eyre::Result<()>;
}

impl<Block> ProofHistoryEngine<Block> for EngineHandle<Block>
where
    Block: reth_primitives_traits::Block + Clone + Send + 'static,
{
    /// Forwards exact-block execution to the upstream engine thread.
    fn execute_block(&self, block: &RecoveredBlock<Block>) -> eyre::Result<()> {
        EngineHandle::execute_block(self, block)?;
        Ok(())
    }

    /// Forwards precomputed block updates to the upstream engine thread.
    fn index_block(
        &self,
        block: BlockWithParent,
        trie_updates: TrieUpdatesSorted,
        post_state: HashedPostStateSorted,
    ) -> eyre::Result<()> {
        EngineHandle::index_block(self, block, trie_updates, post_state)?;
        Ok(())
    }

    /// Forwards a replacement suffix to the upstream engine thread.
    fn reorg(&self, updates: ReorgBlockUpdates) -> eyre::Result<()> {
        EngineHandle::reorg(self, updates)?;
        Ok(())
    }

    /// Forwards an inclusive unwind marker to the upstream engine thread.
    fn unwind(&self, from: BlockWithParent) -> eyre::Result<()> {
        EngineHandle::unwind(self, from)?;
        Ok(())
    }

    /// Forwards a fire-and-forget catch-up target to the upstream engine thread.
    fn sync_to(&self, target: u64) -> eyre::Result<()> {
        EngineHandle::sync_to(self, target)?;
        Ok(())
    }
}

/// Spawns the production upstream engine with its latest-relative pruner disabled.
///
/// Alethia runs its finality-aware pruner separately, so `u64::MAX` prevents the engine's
/// persistence worker from pruning against an unsafe moving latest-block boundary.
pub(super) fn spawn_proof_history_engine<Block, Evm, Provider, Store>(
    evm_config: Evm,
    provider: Provider,
    storage: Store,
) -> EngineHandle<Block>
where
    Block: reth_primitives_traits::Block + Clone + Send + 'static,
    Evm: ConfigureEvm<Primitives: NodePrimitives<Block = Block>> + 'static,
    Provider: BlockHashReader
        + StateReader
        + DatabaseProviderFactory
        + StateProviderFactory
        + BlockReader<Block = Block>
        + Clone
        + 'static,
    Store: OpProofsStore + Clone + 'static,
{
    let pruner = OpProofStoragePruner::new(storage.clone(), provider.clone(), u64::MAX);
    EngineHandle::spawn(evm_config, provider, storage, pruner)
}

#[cfg(test)]
fn spawn_test_proof_history_engine<Block, Evm, Provider, Store>(
    evm_config: Evm,
    provider: Provider,
    storage: Store,
) -> EngineHandle<Block>
where
    Block: reth_primitives_traits::Block + Clone + Send + 'static,
    Evm: ConfigureEvm<Primitives: NodePrimitives<Block = Block>> + 'static,
    Provider: BlockHashReader
        + StateReader
        + DatabaseProviderFactory
        + StateProviderFactory
        + BlockReader<Block = Block>
        + Clone
        + 'static,
    Store: OpProofsStore + Clone + 'static,
{
    let pruner = OpProofStoragePruner::new(storage.clone(), provider.clone(), u64::MAX);
    EngineHandle::spawn_with_thresholds(evm_config, provider, storage, pruner, 1, 2)
}

#[cfg(test)]
mod tests {
    use super::{
        ProofHistoryEngine, ReorgBlockUpdates, spawn_proof_history_engine,
        spawn_test_proof_history_engine,
    };
    use alloy_consensus::Header;
    use alloy_eips::{NumHash, eip1898::BlockWithParent};
    use alloy_primitives::B256;
    use reth_chainspec::{ChainSpec, ChainSpecBuilder, MAINNET};
    use reth_db::Database;
    use reth_db_common::init::init_genesis;
    use reth_ethereum_primitives::{Block, BlockBody};
    use reth_evm_ethereum::EthEvmConfig;
    use reth_optimism_trie::{
        EngineHandle, OpProofsProviderRO, OpProofsStorage, OpProofsStore, RethTrieStorageLayout,
        db::MdbxProofsStorageV2, engine::EngineError, initialize::InitializationJob,
    };
    use reth_primitives_traits::{Block as _, RecoveredBlock};
    use reth_provider::{
        StorageSettingsCache,
        providers::BlockchainProvider,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory_with_chain_spec},
    };
    use reth_trie_common::{HashedPostStateSorted, Nibbles, updates::TrieUpdatesSorted};
    use std::sync::Arc;
    use tempfile::TempDir;

    type TestProvider = BlockchainProvider<MockNodeTypesWithDB>;
    type TestStorage = OpProofsStorage<Arc<MdbxProofsStorageV2>>;

    fn test_chain_spec() -> Arc<ChainSpec> {
        Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(MAINNET.genesis.clone())
                .paris_activated()
                .build(),
        )
    }

    fn empty_block(number: u64, parent_hash: B256, state_root: B256) -> RecoveredBlock<Block> {
        Block {
            header: Header { parent_hash, number, state_root, ..Default::default() },
            body: BlockBody::default(),
        }
        .try_into_recovered()
        .expect("empty block recovers without senders")
    }

    fn genesis_fixture(
        chain_spec: &Arc<ChainSpec>,
    ) -> (TestProvider, TestStorage, EngineHandle<Block>) {
        let factory = create_test_provider_factory_with_chain_spec(chain_spec.clone());
        init_genesis(&factory).expect("genesis state initializes");

        let path = TempDir::new().expect("temp dir").keep();
        let storage: TestStorage =
            Arc::new(MdbxProofsStorageV2::new(&path).expect("MDBX proofs storage opens")).into();
        let layout = if factory.cached_storage_settings().is_v2() {
            RethTrieStorageLayout::Packed
        } else {
            RethTrieStorageLayout::Legacy
        };
        let tx = factory.db_ref().tx().expect("read transaction opens");
        InitializationJob::new(storage.clone(), tx, layout)
            .run(0, chain_spec.genesis_hash())
            .expect("proofs storage initializes to genesis");

        let provider = BlockchainProvider::new(factory).expect("blockchain provider opens");
        let engine = spawn_test_proof_history_engine(
            EthEvmConfig::ethereum(chain_spec.clone()),
            provider.clone(),
            storage.clone(),
        );
        (provider, storage, engine)
    }

    fn block_ref(number: u64, hash: u8, parent: B256) -> BlockWithParent {
        BlockWithParent::new(parent, NumHash::new(number, B256::repeat_byte(hash)))
    }

    fn stored_latest(storage: &TestStorage) -> NumHash {
        storage
            .provider_ro()
            .expect("read provider opens")
            .get_proof_window()
            .expect("proof window exists")
            .latest
    }

    fn non_empty_updates(byte: u8) -> (Arc<TrieUpdatesSorted>, Arc<HashedPostStateSorted>) {
        let trie = TrieUpdatesSorted::new(
            vec![(Nibbles::from_nibbles([byte & 0x0f]), None)],
            Default::default(),
        );
        let state =
            HashedPostStateSorted::new(vec![(B256::repeat_byte(byte), None)], Default::default());
        assert!(!trie.is_empty());
        assert!(!state.is_empty());
        (Arc::new(trie), Arc::new(state))
    }

    #[test]
    fn engine_executes_and_persists_an_empty_block() {
        let chain_spec = test_chain_spec();
        let (_provider, storage, engine) = genesis_fixture(&chain_spec);
        let block =
            empty_block(1, chain_spec.genesis_hash(), chain_spec.genesis_header().state_root);

        ProofHistoryEngine::execute_block(&engine, &block)
            .expect("upstream engine executes the empty block");
        ProofHistoryEngine::sync_to(&engine, 1).expect("sync target forwards to upstream");
        engine.flush();

        assert_eq!(stored_latest(&storage), NumHash::new(1, block.hash()));
    }

    #[test]
    fn engine_rejects_a_wrong_state_root() {
        let chain_spec = test_chain_spec();
        let (provider, storage, test_engine) = genesis_fixture(&chain_spec);
        drop(test_engine);
        let engine = spawn_proof_history_engine(
            EthEvmConfig::ethereum(chain_spec.clone()),
            provider,
            storage.clone(),
        );
        let block = empty_block(1, chain_spec.genesis_hash(), B256::repeat_byte(0xaa));

        let error = ProofHistoryEngine::execute_block(&engine, &block)
            .expect_err("an incorrect state root must be rejected");

        assert!(matches!(error.downcast_ref(), Some(EngineError::StateRootMismatch { .. })));
        assert_eq!(stored_latest(&storage), NumHash::new(0, chain_spec.genesis_hash()));
    }

    #[test]
    fn engine_indexes_precomputed_updates() {
        let chain_spec = test_chain_spec();
        let (_provider, storage, engine) = genesis_fixture(&chain_spec);
        let block = block_ref(1, 0x11, chain_spec.genesis_hash());
        let (trie_updates, post_state) = non_empty_updates(0x11);

        ProofHistoryEngine::index_block(
            &engine,
            block,
            (*trie_updates).clone(),
            (*post_state).clone(),
        )
        .expect("upstream engine accepts precomputed updates");
        engine.flush();

        let stored = storage
            .provider_ro()
            .expect("read provider opens")
            .fetch_trie_updates(1)
            .expect("stored block diff exists");
        assert_eq!(stored_latest(&storage), block.block);
        assert_eq!(stored.sorted_trie_updates, *trie_updates);
        assert_eq!(stored.sorted_post_state, *post_state);
    }

    #[test]
    fn engine_unwinds_to_the_retained_earliest() {
        let chain_spec = test_chain_spec();
        let (_provider, storage, engine) = genesis_fixture(&chain_spec);
        let block = block_ref(1, 0x21, chain_spec.genesis_hash());
        ProofHistoryEngine::index_block(
            &engine,
            block,
            TrieUpdatesSorted::default(),
            HashedPostStateSorted::default(),
        )
        .expect("block buffers");
        engine.flush();

        ProofHistoryEngine::unwind(&engine, block).expect("block unwinds to the genesis anchor");

        assert_eq!(stored_latest(&storage), NumHash::new(0, chain_spec.genesis_hash()));
    }

    #[test]
    fn engine_reorgs_at_the_retained_earliest_with_non_empty_updates() {
        let chain_spec = test_chain_spec();
        let (_provider, storage, engine) = genesis_fixture(&chain_spec);
        let original = block_ref(1, 0x31, chain_spec.genesis_hash());
        ProofHistoryEngine::index_block(
            &engine,
            original,
            TrieUpdatesSorted::default(),
            HashedPostStateSorted::default(),
        )
        .expect("original block buffers");
        engine.flush();

        let replacement_one = block_ref(1, 0x41, chain_spec.genesis_hash());
        let replacement_two = block_ref(2, 0x42, replacement_one.block.hash);
        let (updates_one, state_one) = non_empty_updates(0x41);
        let (updates_two, state_two) = non_empty_updates(0x42);
        let reorg: ReorgBlockUpdates = vec![
            (replacement_one, updates_one.clone(), state_one.clone()),
            (replacement_two, updates_two, state_two),
        ];

        ProofHistoryEngine::reorg(&engine, reorg)
            .expect("upstream engine supports a common ancestor at retained earliest");
        engine.flush();

        let stored = storage
            .provider_ro()
            .expect("read provider opens")
            .fetch_trie_updates(1)
            .expect("replacement block diff exists");
        assert_eq!(stored_latest(&storage), replacement_two.block);
        assert_eq!(stored.sorted_trie_updates, *updates_one);
        assert_eq!(stored.sorted_post_state, *state_one);
    }

    #[test]
    fn engine_acknowledges_updates_above_tip_without_advancing_storage() {
        let chain_spec = test_chain_spec();
        let (_provider, storage, engine) = genesis_fixture(&chain_spec);
        let skipped_parent = B256::repeat_byte(0x51);
        let replacement = block_ref(2, 0x52, skipped_parent);
        let (trie_updates, post_state) = non_empty_updates(0x52);

        ProofHistoryEngine::reorg(&engine, vec![(replacement, trie_updates, post_state)])
            .expect("upstream acknowledges a reorg starting above its proof tip");
        ProofHistoryEngine::unwind(&engine, replacement)
            .expect("upstream acknowledges a revert starting above its proof tip");
        engine.flush();

        assert_eq!(stored_latest(&storage), NumHash::new(0, chain_spec.genesis_hash()));
    }
}
