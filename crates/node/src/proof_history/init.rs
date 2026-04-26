//! Bulk-copy initialization job for proof-history storage.

use super::storage_init::{
    HistoricalInitMetadata, remove_historical_init_metadata,
    validate_historical_init_metadata_file, write_historical_init_metadata,
};
use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, U256};
use eyre::eyre;
use reth_db::{
    DatabaseError,
    cursor::{DbCursorRO, DbDupCursorRO},
    table::{DupSort, Table},
    tables,
    transaction::DbTx,
};
use reth_optimism_trie::{
    OpProofsStore,
    api::{InitialStateAnchor, InitialStateStatus, OpProofsInitProvider},
    db::{HashedStorageKey, StorageTrieKey},
};
use reth_primitives_traits::Account;
use reth_trie::StateRoot;
use reth_trie_common::{
    BranchNodeCompact, HashedPostStateSorted, Nibbles, StoredNibbles,
    updates::{StorageTrieUpdatesSorted, TrieUpdatesSorted},
};
use reth_trie_db::{
    DatabaseHashedCursorFactory, DatabaseStateRoot, DatabaseTrieCursorFactory,
    StorageTrieEntryLike, TrieTableAdapter,
};
use std::path::Path;
use tracing::error;

/// Batch size used while copying the current node state into proof-history storage.
const PROOF_HISTORY_INITIALIZATION_BATCH_SIZE: usize = 100_000;

/// Historical overlay used to rewrite the current node state into an older anchor state.
#[derive(Debug)]
struct HistoricalAnchorOverlay {
    /// Account and storage leaf changes that rewind current state to the anchor.
    hashed_state: HashedPostStateSorted,
    /// Account and storage branch changes that rewind current tries to the anchor.
    trie_updates: TrieUpdatesSorted,
}

impl HistoricalAnchorOverlay {
    /// Returns whether a current hashed account row must be replaced by the overlay.
    fn has_account_update(&self, hashed_address: &B256) -> bool {
        self.hashed_state.accounts.binary_search_by_key(hashed_address, |(key, _)| *key).is_ok()
    }

    /// Returns whether a current hashed storage row must be replaced by the overlay.
    fn has_storage_update(&self, hashed_address: &B256, hashed_slot: &B256) -> bool {
        self.hashed_state.storages.get(hashed_address).is_some_and(|storage| {
            storage.is_wiped() ||
                storage
                    .storage_slots_ref()
                    .binary_search_by_key(hashed_slot, |(key, _)| *key)
                    .is_ok()
        })
    }

    /// Returns whether a current account trie branch must be replaced by the overlay.
    fn has_account_trie_update(&self, path: &Nibbles) -> bool {
        self.trie_updates.account_nodes_ref().binary_search_by_key(path, |(key, _)| *key).is_ok()
    }

    /// Returns whether a current storage trie branch must be replaced by the overlay.
    fn has_storage_trie_update(&self, hashed_address: &B256, path: &Nibbles) -> bool {
        self.trie_updates
            .storage_tries_ref()
            .get(hashed_address)
            .is_some_and(|storage| storage_trie_has_path_update(storage, path))
    }
}

/// Returns whether a sorted storage trie overlay replaces a given path.
fn storage_trie_has_path_update(storage: &StorageTrieUpdatesSorted, path: &Nibbles) -> bool {
    storage.is_deleted() ||
        storage.storage_nodes_ref().binary_search_by_key(path, |(key, _)| *key).is_ok()
}

/// Copies the node's current trie and hashed-state tables into proof-history storage.
#[derive(Debug)]
pub(super) struct ProofHistoryInitializationJob<Tx, Storage> {
    /// Read-only transaction opened against the node's primary database.
    tx: Tx,
    /// Destination proof-history storage initialized by this job.
    storage: Storage,
}

impl<Tx, Storage> ProofHistoryInitializationJob<Tx, Storage>
where
    Tx: DbTx + Sync,
    Storage: OpProofsStore + Send,
{
    /// Creates a proof-history initialization job over a source database transaction.
    pub(super) const fn new(storage: Storage, tx: Tx) -> Self {
        Self { tx, storage }
    }

    /// Runs initialization using the trie key encoding selected by the node's storage settings.
    pub(super) fn run_with_adapter<Adapter>(
        &self,
        best_number: u64,
        best_hash: B256,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        let Some(anchor) = self.prepare_anchor(BlockNumHash::new(best_number, best_hash))? else {
            return Ok(());
        };

        self.initialize_hashed_accounts(anchor.latest_hashed_account_key)?;
        self.initialize_hashed_storages(anchor.latest_hashed_storage_key)?;
        self.initialize_storage_trie::<Adapter>(anchor.latest_storage_trie_key)?;
        self.initialize_account_trie::<Adapter>(anchor.latest_account_trie_key)?;

        self.commit_initial_state()
    }

    /// Runs initialization after rewriting current node tables into a historical anchor state.
    pub(super) fn run_historical_with_adapter<Adapter>(
        &self,
        block: BlockNumHash,
        target_block: BlockNumHash,
        state_root: B256,
        hashed_state: HashedPostStateSorted,
        metadata_path: Option<&Path>,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        let (computed_root, trie_updates) = <StateRoot<
            DatabaseTrieCursorFactory<&Tx, Adapter>,
            DatabaseHashedCursorFactory<&Tx>,
        > as DatabaseStateRoot<'_, Tx>>::overlay_root_with_updates(
            &self.tx, &hashed_state
        )?;

        if computed_root != state_root {
            return Err(eyre!(
                "historical proof-history anchor state root mismatch at block {}: computed={computed_root:?} expected={state_root:?}",
                block.number
            ));
        }

        let overlay =
            HistoricalAnchorOverlay { hashed_state, trie_updates: trie_updates.into_sorted() };

        let metadata = HistoricalInitMetadata { start_block: block, target_block };
        let Some(anchor) = self.prepare_historical_anchor(block, metadata_path, metadata)? else {
            return Ok(());
        };

        self.initialize_hashed_accounts_with_overlay(anchor.latest_hashed_account_key, &overlay)?;
        self.initialize_hashed_storages_with_overlay(anchor.latest_hashed_storage_key, &overlay)?;
        self.initialize_storage_trie_with_overlay::<Adapter>(
            anchor.latest_storage_trie_key,
            &overlay,
        )?;
        self.initialize_account_trie_with_overlay::<Adapter>(
            anchor.latest_account_trie_key,
            &overlay,
        )?;

        self.commit_initial_state()?;
        if let Some(path) = metadata_path &&
            let Err(error) = remove_historical_init_metadata(path)
        {
            error!(
                target: "reth::taiko::proof_history",
                ?error,
                metadata_path = ?path,
                "failed to remove historical proof-history initialization metadata"
            );
        }
        Ok(())
    }

    /// Loads or initializes the proof-history anchor for `block`, returning `None` when
    /// initialization is already completed.
    fn prepare_anchor(&self, block: BlockNumHash) -> eyre::Result<Option<InitialStateAnchor>> {
        let init_provider = self.storage.initialization_provider()?;
        let anchor = init_provider.initial_state_anchor()?;

        match anchor.status {
            InitialStateStatus::Completed => Ok(None),
            InitialStateStatus::NotStarted => {
                init_provider.set_initial_state_anchor(block)?;
                OpProofsInitProvider::commit(init_provider)?;
                Ok(Some(anchor))
            }
            InitialStateStatus::InProgress => {
                self.validate_anchor_block(&anchor, block.number, block.hash)?;
                drop(init_provider);
                Ok(Some(anchor))
            }
        }
    }

    /// Loads or initializes a historical proof-history anchor and validates the resume target.
    fn prepare_historical_anchor(
        &self,
        block: BlockNumHash,
        metadata_path: Option<&Path>,
        expected_metadata: HistoricalInitMetadata,
    ) -> eyre::Result<Option<InitialStateAnchor>> {
        let init_provider = self.storage.initialization_provider()?;
        let anchor = init_provider.initial_state_anchor()?;

        match anchor.status {
            InitialStateStatus::Completed => Ok(None),
            InitialStateStatus::NotStarted => {
                if let Some(path) = metadata_path {
                    write_historical_init_metadata(path, expected_metadata)?;
                }
                init_provider.set_initial_state_anchor(block)?;
                OpProofsInitProvider::commit(init_provider)?;
                Ok(Some(anchor))
            }
            InitialStateStatus::InProgress => {
                self.validate_anchor_block(&anchor, block.number, block.hash)?;
                self.validate_historical_init_metadata(metadata_path, expected_metadata)?;
                drop(init_provider);
                Ok(Some(anchor))
            }
        }
    }

    /// Validates that historical initialization is resuming the same source state.
    fn validate_historical_init_metadata(
        &self,
        metadata_path: Option<&Path>,
        expected_metadata: HistoricalInitMetadata,
    ) -> eyre::Result<()> {
        validate_historical_init_metadata_file(metadata_path, expected_metadata)
    }

    /// Marks proof-history initialization as completed and commits.
    fn commit_initial_state(&self) -> eyre::Result<()> {
        let init_provider = self.storage.initialization_provider()?;
        init_provider.commit_initial_state()?;
        OpProofsInitProvider::commit(init_provider)?;
        Ok(())
    }

    /// Validates that a resumed initialization still targets the same source block.
    fn validate_anchor_block(
        &self,
        anchor: &InitialStateAnchor,
        best_number: u64,
        best_hash: B256,
    ) -> eyre::Result<()> {
        let Some(block) = anchor.block else {
            return Err(eyre!("proof-history initialization anchor is missing its source block"));
        };

        if block.number != best_number || block.hash != best_hash {
            return Err(eyre!(
                "proof-history initialization anchor mismatch: stored=({:?}, {:?}) current=({:?}, {:?})",
                block.number,
                block.hash,
                best_number,
                best_hash
            ));
        }

        Ok(())
    }

    /// Copies hashed account leaves from the node database into proof-history storage.
    fn initialize_hashed_accounts(&self, start_key: Option<B256>) -> eyre::Result<()> {
        self.initialize_hashed_accounts_filtered(start_key, |_| true)
    }

    /// Copies historical hashed account leaves, replacing current rows with overlay rows.
    fn initialize_hashed_accounts_with_overlay(
        &self,
        start_key: Option<B256>,
        overlay: &HistoricalAnchorOverlay,
    ) -> eyre::Result<()> {
        self.initialize_hashed_accounts_filtered(start_key, |hashed_address| {
            !overlay.has_account_update(hashed_address)
        })?;
        self.store_hashed_accounts(overlay.hashed_state.accounts.clone())
    }

    /// Copies hashed account leaves that satisfy `include`.
    fn initialize_hashed_accounts_filtered(
        &self,
        start_key: Option<B256>,
        include: impl Fn(&B256) -> bool,
    ) -> eyre::Result<()> {
        let mut cursor = self.tx.cursor_read::<tables::HashedAccounts>()?;

        if let Some(latest_key) = start_key {
            cursor.seek(latest_key)?.filter(|(key, _)| *key == latest_key).ok_or_else(|| {
                eyre!("proof-history initialization resume key missing in HashedAccounts")
            })?;
        }

        let mut batch = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((hashed_address, account)) = cursor.next()? {
            if !include(&hashed_address) {
                continue;
            }

            batch.push((hashed_address, Some(account)));
            if batch.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE {
                self.store_hashed_accounts(std::mem::take(&mut batch))?;
            }
        }
        self.store_hashed_accounts(batch)
    }

    /// Copies hashed storage leaves from the node database into proof-history storage.
    fn initialize_hashed_storages(&self, start_key: Option<HashedStorageKey>) -> eyre::Result<()> {
        self.initialize_hashed_storages_filtered(start_key, |_, _| true)
    }

    /// Copies historical hashed storage leaves, replacing current rows with overlay rows.
    fn initialize_hashed_storages_with_overlay(
        &self,
        start_key: Option<HashedStorageKey>,
        overlay: &HistoricalAnchorOverlay,
    ) -> eyre::Result<()> {
        self.initialize_hashed_storages_filtered(start_key, |hashed_address, hashed_slot| {
            !overlay.has_storage_update(hashed_address, hashed_slot)
        })?;

        for (hashed_address, storage) in &overlay.hashed_state.storages {
            self.store_hashed_storages(*hashed_address, storage.storage_slots_ref().to_vec())?;
        }

        Ok(())
    }

    /// Copies hashed storage leaves that satisfy `include`.
    fn initialize_hashed_storages_filtered(
        &self,
        start_key: Option<HashedStorageKey>,
        include: impl Fn(&B256, &B256) -> bool,
    ) -> eyre::Result<()> {
        let mut cursor = self.tx.cursor_dup_read::<tables::HashedStorages>()?;

        if let Some(latest_key) = start_key {
            cursor
                .seek_by_key_subkey(latest_key.hashed_address, latest_key.hashed_storage_key)?
                .filter(|storage| storage.key == latest_key.hashed_storage_key)
                .ok_or_else(|| {
                    eyre!("proof-history initialization resume key missing in HashedStorages")
                })?;
        }

        let mut current_address = None;
        let mut current_slots = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((hashed_address, storage)) =
            next_dup_entry::<tables::HashedStorages, _>(&mut cursor)?
        {
            if !include(&hashed_address, &storage.key) {
                continue;
            }

            if current_address.is_some_and(|address| address != hashed_address) ||
                current_slots.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE
            {
                let address =
                    current_address.expect("storage batch is only populated after address is set");
                self.store_hashed_storages(address, std::mem::take(&mut current_slots))?;
            }

            current_address = Some(hashed_address);
            current_slots.push((storage.key, storage.value));
        }

        if let Some(address) = current_address {
            self.store_hashed_storages(address, current_slots)?;
        }

        Ok(())
    }

    /// Copies account trie branches from the node database into proof-history storage.
    fn initialize_account_trie<Adapter>(&self, start_key: Option<StoredNibbles>) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        self.initialize_account_trie_filtered::<Adapter>(start_key, |_| true)
    }

    /// Copies historical account trie branches, replacing current rows with overlay rows.
    fn initialize_account_trie_with_overlay<Adapter>(
        &self,
        start_key: Option<StoredNibbles>,
        overlay: &HistoricalAnchorOverlay,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        self.initialize_account_trie_filtered::<Adapter>(start_key, |path| {
            !overlay.has_account_trie_update(path)
        })?;
        self.store_account_branches(overlay.trie_updates.account_nodes_ref().to_vec())
    }

    /// Copies account trie branches that satisfy `include`.
    fn initialize_account_trie_filtered<Adapter>(
        &self,
        start_key: Option<StoredNibbles>,
        include: impl Fn(&Nibbles) -> bool,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        let mut cursor = self.tx.cursor_read::<Adapter::AccountTrieTable>()?;

        if let Some(latest_key) = start_key {
            let adapter_key = Adapter::AccountKey::from(latest_key.0);
            cursor.seek(adapter_key.clone())?.filter(|(key, _)| *key == adapter_key).ok_or_else(
                || eyre!("proof-history initialization resume key missing in account trie"),
            )?;
        }

        let mut batch = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((path, branch)) = cursor.next()? {
            let nibbles = Adapter::account_key_to_nibbles(&path);
            if !include(&nibbles) {
                continue;
            }

            batch.push((nibbles, Some(branch)));
            if batch.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE {
                self.store_account_branches(std::mem::take(&mut batch))?;
            }
        }
        self.store_account_branches(batch)
    }

    /// Copies storage trie branches from the node database into proof-history storage.
    fn initialize_storage_trie<Adapter>(
        &self,
        start_key: Option<StorageTrieKey>,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        self.initialize_storage_trie_filtered::<Adapter>(start_key, |_, _| true)
    }

    /// Copies historical storage trie branches, replacing current rows with overlay rows.
    fn initialize_storage_trie_with_overlay<Adapter>(
        &self,
        start_key: Option<StorageTrieKey>,
        overlay: &HistoricalAnchorOverlay,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        self.initialize_storage_trie_filtered::<Adapter>(start_key, |hashed_address, path| {
            !overlay.has_storage_trie_update(hashed_address, path)
        })?;

        for (hashed_address, storage) in overlay.trie_updates.storage_tries_ref() {
            self.store_storage_branches(*hashed_address, storage.storage_nodes_ref().to_vec())?;
        }

        Ok(())
    }

    /// Copies storage trie branches that satisfy `include`.
    fn initialize_storage_trie_filtered<Adapter>(
        &self,
        start_key: Option<StorageTrieKey>,
        include: impl Fn(&B256, &Nibbles) -> bool,
    ) -> eyre::Result<()>
    where
        Adapter: TrieTableAdapter,
    {
        let mut cursor = self.tx.cursor_dup_read::<Adapter::StorageTrieTable>()?;

        if let Some(latest_key) = start_key {
            let adapter_subkey = Adapter::StorageSubKey::from(latest_key.path.0);
            cursor
                .seek_by_key_subkey(latest_key.hashed_address, adapter_subkey.clone())?
                .filter(|entry| *entry.nibbles() == adapter_subkey)
                .ok_or_else(|| {
                    eyre!("proof-history initialization resume key missing in storage trie")
                })?;
        }

        let mut current_address = None;
        let mut current_nodes = Vec::with_capacity(PROOF_HISTORY_INITIALIZATION_BATCH_SIZE);
        while let Some((hashed_address, entry)) =
            next_dup_entry::<Adapter::StorageTrieTable, _>(&mut cursor)?
        {
            let (subkey, branch) = entry.into_parts();
            let nibbles = Adapter::subkey_to_nibbles(&subkey);
            if !include(&hashed_address, &nibbles) {
                continue;
            }

            if current_address.is_some_and(|address| address != hashed_address) ||
                current_nodes.len() >= PROOF_HISTORY_INITIALIZATION_BATCH_SIZE
            {
                let address = current_address
                    .expect("storage trie batch is only populated after address is set");
                self.store_storage_branches(address, std::mem::take(&mut current_nodes))?;
            }

            current_address = Some(hashed_address);
            current_nodes.push((nibbles, Some(branch)));
        }

        if let Some(address) = current_address {
            self.store_storage_branches(address, current_nodes)?;
        }

        Ok(())
    }

    /// Stores a batch of hashed account leaves and commits the proof-history transaction.
    fn store_hashed_accounts(&self, batch: Vec<(B256, Option<Account>)>) -> eyre::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let provider = self.storage.initialization_provider()?;
        provider.store_hashed_accounts(batch)?;
        OpProofsInitProvider::commit(provider)?;
        Ok(())
    }

    /// Stores a batch of hashed storage leaves for one account and commits it.
    fn store_hashed_storages(
        &self,
        hashed_address: B256,
        batch: Vec<(B256, U256)>,
    ) -> eyre::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let provider = self.storage.initialization_provider()?;
        provider.store_hashed_storages(hashed_address, batch)?;
        OpProofsInitProvider::commit(provider)?;
        Ok(())
    }

    /// Stores a batch of account trie branches and commits the proof-history transaction.
    fn store_account_branches(
        &self,
        batch: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> eyre::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let provider = self.storage.initialization_provider()?;
        provider.store_account_branches(batch)?;
        OpProofsInitProvider::commit(provider)?;
        Ok(())
    }

    /// Stores a batch of storage trie branches for one account and commits it.
    fn store_storage_branches(
        &self,
        hashed_address: B256,
        batch: Vec<(Nibbles, Option<BranchNodeCompact>)>,
    ) -> eyre::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let provider = self.storage.initialization_provider()?;
        provider.store_storage_branches(hashed_address, batch)?;
        OpProofsInitProvider::commit(provider)?;
        Ok(())
    }
}

/// Result type returned while iterating duplicate-sorted source tables.
type DupEntryResult<T> = Result<Option<(<T as Table>::Key, <T as Table>::Value)>, DatabaseError>;

/// Returns the next duplicate-sorted table entry, advancing to the next key when needed.
fn next_dup_entry<T, C>(cursor: &mut C) -> DupEntryResult<T>
where
    T: DupSort,
    C: DbCursorRO<T> + DbDupCursorRO<T>,
{
    if let Some(entry) = cursor.next_dup()? {
        return Ok(Some(entry));
    }

    let Some((next_key, _)) = cursor.next_no_dup()? else {
        return Ok(None);
    };

    cursor.seek(next_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256, map::B256Map};
    use reth::providers::{DBProvider, DatabaseProviderFactory};
    use reth_db::{
        tables::{self, PackedStoragesTrie},
        transaction::DbTxMut,
    };
    use reth_optimism_trie::{
        InMemoryProofsStorage, OpProofsStorage, OpProofsStore, api::OpProofsInitProvider,
        db::StorageTrieKey,
    };
    use reth_primitives_traits::StorageEntry;
    use reth_provider::test_utils::create_test_provider_factory;
    use reth_storage_api::{StorageSettings, StorageSettingsCache};
    use reth_trie_common::{
        BranchNodeCompact, HashedStorageSorted, Nibbles, PackedStorageTrieEntry,
        PackedStoredNibblesSubKey, StoredNibbles, TrieMask, updates::StorageTrieUpdatesSorted,
    };
    use reth_trie_db::PackedKeyAdapter;

    #[test]
    fn historical_anchor_overlay_tracks_replaced_rows() {
        let account = B256::with_last_byte(1);
        let storage = B256::with_last_byte(2);
        let wiped_storage = B256::with_last_byte(3);
        let slot = B256::with_last_byte(4);
        let account_path = Nibbles::from_nibbles([0x01]);
        let storage_path = Nibbles::from_nibbles([0x02]);
        let untouched_path = Nibbles::from_nibbles([0x03]);
        let branch = BranchNodeCompact::new(
            TrieMask::new(0b0011),
            TrieMask::new(0b0011),
            TrieMask::new(0),
            Vec::new(),
            None,
        );

        let overlay = HistoricalAnchorOverlay {
            hashed_state: HashedPostStateSorted::new(
                vec![(account, None)],
                B256Map::from_iter([
                    (
                        storage,
                        HashedStorageSorted {
                            storage_slots: vec![(slot, U256::ZERO)],
                            wiped: false,
                        },
                    ),
                    (wiped_storage, HashedStorageSorted { storage_slots: Vec::new(), wiped: true }),
                ]),
            ),
            trie_updates: TrieUpdatesSorted::new(
                vec![(account_path.clone(), None)],
                B256Map::from_iter([
                    (
                        storage,
                        StorageTrieUpdatesSorted {
                            is_deleted: false,
                            storage_nodes: vec![(storage_path.clone(), Some(branch))],
                        },
                    ),
                    (
                        wiped_storage,
                        StorageTrieUpdatesSorted { is_deleted: true, storage_nodes: Vec::new() },
                    ),
                ]),
            ),
        };

        assert!(overlay.has_account_update(&account));
        assert!(overlay.has_storage_update(&storage, &slot));
        assert!(overlay.has_storage_update(&wiped_storage, &B256::with_last_byte(99)));
        assert!(overlay.has_account_trie_update(&account_path));
        assert!(overlay.has_storage_trie_update(&storage, &storage_path));
        assert!(overlay.has_storage_trie_update(&wiped_storage, &untouched_path));
        assert!(!overlay.has_account_update(&B256::with_last_byte(99)));
        assert!(!overlay.has_storage_trie_update(&storage, &untouched_path));
    }

    #[test]
    fn proof_history_initialization_reads_storage_v2_packed_trie_entries() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(StorageSettings::v2());

        let hashed_address = B256::with_last_byte(1);
        let hashed_slot = B256::with_last_byte(2);
        let trie_path = Nibbles::from_nibbles([0x0a, 0x0b]);
        let branch = BranchNodeCompact::new(
            TrieMask::new(0b0011),
            TrieMask::new(0b0011),
            TrieMask::new(0),
            Vec::new(),
            None,
        );

        let provider_rw = factory.database_provider_rw().unwrap();
        {
            let tx = provider_rw.tx_ref();
            tx.put::<tables::HashedStorages>(
                hashed_address,
                StorageEntry { key: hashed_slot, value: U256::from(1) },
            )
            .unwrap();
            tx.put::<PackedStoragesTrie>(
                hashed_address,
                PackedStorageTrieEntry {
                    nibbles: PackedStoredNibblesSubKey::from(trie_path),
                    node: branch,
                },
            )
            .unwrap();
        }
        provider_rw.commit().unwrap();

        let source_provider = factory.database_provider_ro().unwrap();
        let storage: OpProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::default().into();

        ProofHistoryInitializationJob::new(storage.clone(), source_provider.into_tx())
            .run_with_adapter::<PackedKeyAdapter>(0, B256::ZERO)
            .unwrap();

        let anchor = storage.initialization_provider().unwrap().initial_state_anchor().unwrap();
        assert_eq!(
            anchor.latest_storage_trie_key,
            Some(StorageTrieKey::new(hashed_address, StoredNibbles::from(trie_path)))
        );
        assert_eq!(anchor.latest_hashed_storage_key.unwrap().hashed_storage_key, hashed_slot);
    }
}
