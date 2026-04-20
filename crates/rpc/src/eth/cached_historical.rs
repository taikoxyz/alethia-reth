//! Request-local cached historical provider helpers for witness generation.
use alloy_eips::merge::EPOCH_SLOTS;
use alloy_primitives::{Address, B256, BlockNumber, Bytes, StorageKey, StorageValue};
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{
    AccountReader, BlockHashReader, BlockNumReader, BytecodeReader, ChangeSetReader, DBProvider,
    HashedPostStateProvider, HistoricalStateProviderRef, NodePrimitivesProvider, ProviderError,
    ProviderResult, RocksDBProviderFactory, StateProofProvider, StateProvider, StateRootProvider,
    StorageChangeSetReader, StorageRootProvider, StorageSettingsCache,
    providers::LowestAvailableBlocks,
};
use reth_trie::{
    AccountProof, HashedPostState, HashedPostStateSorted, KeccakKeyHasher, MultiProof,
    MultiProofTargets, StateRoot, TrieInput, hashed_cursor::HashedPostStateCursorFactory,
    proof::Proof, trie_cursor::InMemoryTrieCursorFactory, updates::TrieUpdates,
    witness::TrieWitness,
};
use reth_trie_db::{DatabaseProof, DatabaseStateRoot};
use std::sync::{Arc, Mutex};

/// Lazily initializes and caches a shared revert value for one RPC request.
#[derive(Debug)]
pub struct LazyRevertStateCache<T> {
    /// Request-local cached value shared by all provider instances for the same range call.
    value: Mutex<Option<Arc<T>>>,
}

impl<T> Default for LazyRevertStateCache<T> {
    /// Creates an empty lazy cache with no initialized revert state.
    fn default() -> Self {
        Self { value: Mutex::new(None) }
    }
}

impl<T> LazyRevertStateCache<T> {
    /// Returns the cached value, loading it once with `init` when needed.
    pub fn get_or_try_init<E, F>(&self, init: F) -> Result<Arc<T>, E>
    where
        F: FnOnce() -> Result<Arc<T>, E>,
    {
        let mut guard = self.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(value) = guard.as_ref() {
            return Ok(Arc::clone(value));
        }

        let value = init()?;
        *guard = Some(Arc::clone(&value));
        Ok(value)
    }
}

/// Database-backed `StateRoot` calculator specialized with a concrete trie table adapter.
type DbStateRoot<'a, TX, A> = StateRoot<
    reth_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    reth_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;

/// Database-backed account proof calculator specialized with a concrete trie table adapter.
type DbProof<'a, TX, A> = Proof<
    reth_trie_db::DatabaseTrieCursorFactory<&'a TX, A>,
    reth_trie_db::DatabaseHashedCursorFactory<&'a TX>,
>;

/// Historical state provider that memoizes `revert_state()` for one range RPC request.
pub struct CachedHistoricalStateProvider<Provider> {
    /// Database provider used for historical lookups and witness generation.
    provider: Provider,
    /// Historical state index, matching upstream `HistoricalStateProvider` semantics.
    block_number: BlockNumber,
    /// Lowest block numbers whose history is still available.
    lowest_available_blocks: LowestAvailableBlocks,
    /// Lazily populated revert overlay shared by all blocks in the same range request.
    revert_state: LazyRevertStateCache<HashedPostStateSorted>,
}

impl<Provider> CachedHistoricalStateProvider<Provider>
where
    Provider: DBProvider + ChangeSetReader + StorageChangeSetReader + BlockNumReader,
{
    /// Creates a cached historical provider for the state at `block_number - 1`.
    pub fn new(provider: Provider, block_number: BlockNumber) -> Self {
        Self {
            provider,
            block_number,
            lowest_available_blocks: LowestAvailableBlocks::default(),
            revert_state: LazyRevertStateCache::default(),
        }
    }

    /// Returns a borrowing historical provider view for methods we do not customize.
    fn as_ref(&self) -> HistoricalStateProviderRef<'_, Provider> {
        HistoricalStateProviderRef::new_with_lowest_available_blocks(
            &self.provider,
            self.block_number,
            self.lowest_available_blocks,
        )
    }

    /// Returns the read-only database transaction used by trie readers.
    fn tx(&self) -> &Provider::Tx {
        self.provider.tx_ref()
    }

    /// Returns whether the requested historical block is far enough from tip to deserve a warning.
    fn is_far_from_tip(&self, limit: u64) -> ProviderResult<bool> {
        let tip = self.provider.last_block_number()?;
        Ok(tip.saturating_sub(self.block_number) > limit)
    }

    /// Ensures account and storage history remain available for this historical view.
    fn ensure_history_available(&self) -> ProviderResult<()> {
        if !self.lowest_available_blocks.is_account_history_available(self.block_number) ||
            !self.lowest_available_blocks.is_storage_history_available(self.block_number)
        {
            return Err(ProviderError::StateAtBlockPruned(self.block_number));
        }

        Ok(())
    }

    /// Emits the same old-block warning as upstream historical state providers.
    fn warn_if_far_from_tip(&self) -> ProviderResult<()> {
        if self.is_far_from_tip(EPOCH_SLOTS)? {
            tracing::warn!(
                target: "providers::historical_sp",
                target = self.block_number,
                "Attempt to calculate state root for an old block might result in OOM"
            );
        }

        Ok(())
    }

    /// Loads and caches the reverted hashed-state overlay for this historical view.
    fn revert_state(&self) -> ProviderResult<Arc<HashedPostStateSorted>>
    where
        Provider: StorageSettingsCache,
    {
        self.ensure_history_available()?;
        self.warn_if_far_from_tip()?;

        self.revert_state.get_or_try_init(|| {
            reth_trie_db::from_reverts_auto(&self.provider, self.block_number..).map(Arc::new)
        })
    }

    /// Returns the historical trie input with the cached revert overlay prepended.
    fn prepend_revert_state(&self, mut input: TrieInput) -> ProviderResult<TrieInput>
    where
        Provider: StorageSettingsCache,
    {
        input.prepend((*self.revert_state()?).clone().into());
        Ok(input)
    }

    /// Returns the post-state overlay applied on top of the cached historical revert state.
    fn overlay_hashed_state(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<HashedPostStateSorted>
    where
        Provider: StorageSettingsCache,
    {
        let mut revert_state = (*self.revert_state()?).clone();
        let hashed_state_sorted = hashed_state.into_sorted();
        revert_state.extend_ref_and_sort(&hashed_state_sorted);
        Ok(revert_state)
    }
}

impl<Provider> BlockHashReader for CachedHistoricalStateProvider<Provider>
where
    Provider:
        DBProvider + BlockNumReader + BlockHashReader + ChangeSetReader + StorageChangeSetReader,
{
    /// Returns the canonical hash for the requested block number.
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.as_ref().block_hash(number)
    }

    /// Returns canonical block hashes for the requested range.
    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.as_ref().canonical_hashes_range(start, end)
    }
}

impl<Provider> AccountReader for CachedHistoricalStateProvider<Provider>
where
    Provider: DBProvider
        + ChangeSetReader
        + StorageChangeSetReader
        + BlockNumReader
        + StorageSettingsCache
        + RocksDBProviderFactory
        + NodePrimitivesProvider,
{
    /// Returns the basic account for the requested historical block view.
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        self.as_ref().basic_account(address)
    }
}

impl<Provider> BytecodeReader for CachedHistoricalStateProvider<Provider>
where
    Provider: DBProvider + ChangeSetReader + StorageChangeSetReader + BlockNumReader,
{
    /// Returns the contract bytecode for the requested code hash.
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.as_ref().bytecode_by_hash(code_hash)
    }
}

impl<Provider> HashedPostStateProvider for CachedHistoricalStateProvider<Provider> {
    /// Hashes the given execution bundle into a post-state overlay.
    fn hashed_post_state(
        &self,
        bundle_state: &reth_revm::revm::database::BundleState,
    ) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<Provider> StateRootProvider for CachedHistoricalStateProvider<Provider>
where
    Provider: DBProvider
        + ChangeSetReader
        + StorageChangeSetReader
        + BlockNumReader
        + StorageSettingsCache,
{
    /// Returns the post-state root on top of the cached historical revert overlay.
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        reth_trie_db::with_adapter!(self.provider, |A| {
            let revert_state = self.overlay_hashed_state(hashed_state)?;
            Ok(<DbStateRoot<'_, _, A>>::overlay_root(self.tx(), &revert_state)?)
        })
    }

    /// Returns the post-state root for the given trie input on top of the cached historical view.
    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        reth_trie_db::with_adapter!(self.provider, |A| {
            let input = self.prepend_revert_state(input)?;
            Ok(<DbStateRoot<'_, _, A>>::overlay_root_from_nodes(
                self.tx(),
                reth_trie::TrieInputSorted::from_unsorted(input),
            )?)
        })
    }

    /// Returns the post-state root and trie updates on top of the cached historical revert overlay.
    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        reth_trie_db::with_adapter!(self.provider, |A| {
            let revert_state = self.overlay_hashed_state(hashed_state)?;
            Ok(<DbStateRoot<'_, _, A>>::overlay_root_with_updates(self.tx(), &revert_state)?)
        })
    }

    /// Returns the post-state root and trie updates for the given trie input.
    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        reth_trie_db::with_adapter!(self.provider, |A| {
            let input = self.prepend_revert_state(input)?;
            Ok(<DbStateRoot<'_, _, A>>::overlay_root_from_nodes_with_updates(
                self.tx(),
                reth_trie::TrieInputSorted::from_unsorted(input),
            )?)
        })
    }
}

impl<Provider> StorageRootProvider for CachedHistoricalStateProvider<Provider>
where
    Provider: DBProvider
        + ChangeSetReader
        + StorageChangeSetReader
        + BlockNumReader
        + StorageSettingsCache
        + RocksDBProviderFactory
        + NodePrimitivesProvider,
{
    /// Returns the storage root for the requested account on the historical view.
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: reth_trie::HashedStorage,
    ) -> ProviderResult<B256> {
        self.as_ref().storage_root(address, hashed_storage)
    }

    /// Returns the storage proof for one slot on the historical view.
    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: reth_trie::HashedStorage,
    ) -> ProviderResult<reth_trie::StorageProof> {
        self.as_ref().storage_proof(address, slot, hashed_storage)
    }

    /// Returns the storage multiproof for the requested slots on the historical view.
    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: reth_trie::HashedStorage,
    ) -> ProviderResult<reth_trie::StorageMultiProof> {
        self.as_ref().storage_multiproof(address, slots, hashed_storage)
    }
}

impl<Provider> StateProofProvider for CachedHistoricalStateProvider<Provider>
where
    Provider: DBProvider
        + ChangeSetReader
        + StorageChangeSetReader
        + BlockNumReader
        + StorageSettingsCache,
{
    /// Returns an account proof using the cached historical revert overlay.
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        reth_trie_db::with_adapter!(self.provider, |A| {
            let input = self.prepend_revert_state(input)?;
            let proof = <DbProof<'_, _, A> as DatabaseProof>::from_tx(self.tx());
            proof.overlay_account_proof(input, address, slots).map_err(ProviderError::from)
        })
    }

    /// Returns a multiproof using the cached historical revert overlay.
    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        reth_trie_db::with_adapter!(self.provider, |A| {
            let input = self.prepend_revert_state(input)?;
            let proof = <DbProof<'_, _, A> as DatabaseProof>::from_tx(self.tx());
            proof.overlay_multiproof(input, targets).map_err(ProviderError::from)
        })
    }

    /// Returns a witness using the cached historical revert overlay.
    fn witness(&self, input: TrieInput, target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        reth_trie_db::with_adapter!(self.provider, |A| {
            let input = self.prepend_revert_state(input)?;
            let nodes_sorted = input.nodes.into_sorted();
            let state_sorted = input.state.into_sorted();
            TrieWitness::new(
                InMemoryTrieCursorFactory::new(
                    reth_trie_db::DatabaseTrieCursorFactory::<_, A>::new(self.tx()),
                    &nodes_sorted,
                ),
                HashedPostStateCursorFactory::new(
                    reth_trie_db::DatabaseHashedCursorFactory::new(self.tx()),
                    &state_sorted,
                ),
            )
            .with_prefix_sets_mut(input.prefix_sets)
            .always_include_root_node()
            .compute(target)
            .map_err(ProviderError::from)
            .map(|nodes| nodes.into_values().collect())
        })
    }
}

impl<Provider> StateProvider for CachedHistoricalStateProvider<Provider>
where
    Provider: DBProvider
        + BlockNumReader
        + BlockHashReader
        + ChangeSetReader
        + StorageChangeSetReader
        + StorageSettingsCache
        + RocksDBProviderFactory
        + NodePrimitivesProvider,
{
    /// Returns the historical storage value for the requested plain slot.
    fn storage(
        &self,
        address: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        self.as_ref().storage(address, storage_key)
    }
}

#[cfg(test)]
mod tests {
    use super::LazyRevertStateCache;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn lazy_revert_cache_loads_once() {
        let loads = AtomicUsize::new(0);
        let cache: LazyRevertStateCache<usize> = LazyRevertStateCache::default();

        let first = cache
            .get_or_try_init(|| {
                loads.fetch_add(1, Ordering::Relaxed);
                Ok::<Arc<usize>, &'static str>(Arc::new(7))
            })
            .unwrap();
        let second = cache
            .get_or_try_init(|| {
                loads.fetch_add(1, Ordering::Relaxed);
                Ok::<Arc<usize>, &'static str>(Arc::new(9))
            })
            .unwrap();

        assert_eq!(*first, 7);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn lazy_revert_cache_is_shared_across_arcs() {
        let loads = Arc::new(AtomicUsize::new(0));
        let cache: Arc<LazyRevertStateCache<usize>> = Arc::default();

        let first = {
            let loads = Arc::clone(&loads);
            Arc::clone(&cache)
                .get_or_try_init(move || {
                    loads.fetch_add(1, Ordering::Relaxed);
                    Ok::<Arc<usize>, &'static str>(Arc::new(11))
                })
                .unwrap()
        };

        let second = {
            let loads = Arc::clone(&loads);
            cache.get_or_try_init(move || {
                loads.fetch_add(1, Ordering::Relaxed);
                Ok::<Arc<usize>, &'static str>(Arc::new(13))
            })
        }
        .unwrap();

        assert_eq!(*first, 11);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn lazy_revert_cache_does_not_persist_failed_init() {
        let loads = AtomicUsize::new(0);
        let cache: LazyRevertStateCache<usize> = LazyRevertStateCache::default();

        let first = cache.get_or_try_init(|| {
            loads.fetch_add(1, Ordering::Relaxed);
            Err::<Arc<usize>, &'static str>("boom")
        });
        let second = cache
            .get_or_try_init(|| {
                loads.fetch_add(1, Ordering::Relaxed);
                Ok::<Arc<usize>, &'static str>(Arc::new(17))
            })
            .unwrap();

        assert_eq!(first, Err("boom"));
        assert_eq!(*second, 17);
        assert_eq!(loads.load(Ordering::Relaxed), 2);
    }
}
