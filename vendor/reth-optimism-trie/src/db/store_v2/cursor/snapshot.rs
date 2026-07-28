//! Plain (non-history-aware) cursors over the snapshot tables.
//!
//! Unlike the V2 history-aware cursors (see [`super::account_trie`] and
//! [`super::storage_trie`], [`super::account`], [`super::storage`]), these read directly from
//! snapshot tables without any merge walk: the snapshot tables already reflect trie state at the
//! snapshot's anchor block, so a single current-state read is authoritative.
//!
//! Used by the backfill job when a [`SnapshotStatus::Ready`] snapshot is
//! available — see `crate::backfill` for the rationale.
//!
//! [`SnapshotStatus::Ready`]: crate::db::models::SnapshotStatus::Ready

use alloy_primitives::{B256, U256};
use reth_db::{
    DatabaseError,
    cursor::{DbCursorRO, DbDupCursorRO},
};
use reth_primitives_traits::Account;
use reth_trie::{
    BranchNodeCompact, Nibbles, StoredNibbles, StoredNibblesSubKey,
    hashed_cursor::{HashedCursor, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieStorageCursor},
};

use crate::db::models::{
    V2AccountsTrieSnapshot, V2HashedAccountsSnapshot, V2HashedStoragesSnapshot,
    V2StoragesTrieSnapshot,
};

/// Plain account-trie cursor over [`V2AccountsTrieSnapshot`].
#[derive(Debug)]
pub struct V2AccountTrieSnapshotCursor<C> {
    /// MDBX cursor over the materialized account-trie snapshot.
    cursor: C,
    /// Last path returned, retained to implement [`TrieCursor::current`].
    last_key: Option<StoredNibbles>,
    /// Whether `seek*` has positioned the underlying cursor at least once
    /// since construction / `reset`. Guards `next` against undefined mdbx
    /// behavior when called on an unpositioned cursor.
    seeked: bool,
}

impl<C> V2AccountTrieSnapshotCursor<C> {
    /// Create a new snapshot cursor wrapping `cursor`.
    pub const fn new(cursor: C) -> Self {
        Self { cursor, last_key: None, seeked: false }
    }
}

impl<C> TrieCursor for V2AccountTrieSnapshotCursor<C>
where
    C: DbCursorRO<V2AccountsTrieSnapshot> + Send,
{
    /// Positions at the branch exactly matching `key`, returning `None` when absent and propagating
    /// backend errors.
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.seeked = true;
        let entry = self.cursor.seek_exact(StoredNibbles(key))?;
        if let Some((ref k, _)) = entry {
            self.last_key = Some(k.clone());
        }
        Ok(entry.map(|(k, v)| (k.0, v)))
    }

    /// Positions at the first trie branch whose path is at least `key`, returning `None` at the end
    /// and propagating backend errors.
    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.seeked = true;
        let entry = self.cursor.seek(StoredNibbles(key))?;
        if let Some((ref k, _)) = entry {
            self.last_key = Some(k.clone());
        }
        Ok(entry.map(|(k, v)| (k.0, v)))
    }

    /// Advances to the next trie branch in nibble-path order, returning `None` at the end and
    /// propagating backend errors.
    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if !self.seeked {
            return self.seek(Nibbles::default());
        }
        let entry = self.cursor.next()?;
        if let Some((ref k, _)) = entry {
            self.last_key = Some(k.clone());
        }
        Ok(entry.map(|(k, v)| (k.0, v)))
    }

    /// Returns the currently positioned trie path without advancing, or `None` before positioning
    /// or after exhaustion.
    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        Ok(self.last_key.as_ref().map(|k| k.0))
    }

    /// Clears the current trie position so subsequent traversal starts from an unpositioned state.
    fn reset(&mut self) {
        self.last_key = None;
        self.seeked = false;
    }
}

/// Plain storage-trie cursor over [`V2StoragesTrieSnapshot`] (a `DupSort` table).
#[derive(Debug)]
pub struct V2StorageTrieSnapshotCursor<C> {
    /// Dup-sorted MDBX cursor over materialized storage-trie snapshots.
    cursor: C,
    /// Account whose storage-trie rows are visible through this cursor.
    hashed_address: B256,
    /// Last path returned, retained to implement [`TrieCursor::current`].
    last_key: Option<StoredNibbles>,
    /// Whether `seek*` has positioned the underlying cursor at least once
    /// for the current `hashed_address`. Guards `next` against undefined
    /// mdbx behavior when called on an unpositioned cursor.
    seeked: bool,
}

impl<C> V2StorageTrieSnapshotCursor<C> {
    /// Create a new snapshot cursor wrapping `cursor`, scoped to `hashed_address`.
    pub const fn new(cursor: C, hashed_address: B256) -> Self {
        Self { cursor, hashed_address, last_key: None, seeked: false }
    }
}

impl<C> TrieCursor for V2StorageTrieSnapshotCursor<C>
where
    C: DbCursorRO<V2StoragesTrieSnapshot> + DbDupCursorRO<V2StoragesTrieSnapshot> + Send,
{
    /// Positions at the branch exactly matching `key`, returning `None` when absent and propagating
    /// backend errors.
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.seeked = true;
        let subkey = StoredNibblesSubKey(key);
        let entry = self
            .cursor
            .seek_by_key_subkey(self.hashed_address, subkey.clone())?
            .filter(|e| e.nibbles == subkey);
        if entry.is_some() {
            self.last_key = Some(StoredNibbles(key));
        }
        Ok(entry.map(|e| (key, e.node)))
    }

    /// Positions at the first trie branch whose path is at least `key`, returning `None` at the end
    /// and propagating backend errors.
    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.seeked = true;
        let entry =
            self.cursor.seek_by_key_subkey(self.hashed_address, StoredNibblesSubKey(key))?;
        if let Some(ref e) = entry {
            self.last_key = Some(StoredNibbles(e.nibbles.0));
        }
        Ok(entry.map(|e| (e.nibbles.0, e.node)))
    }

    /// Advances to the next trie branch in nibble-path order, returning `None` at the end and
    /// propagating backend errors.
    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        if !self.seeked {
            return self.seek(Nibbles::default());
        }
        let entry = self.cursor.next_dup()?.map(|(_, v)| v);
        if let Some(ref e) = entry {
            self.last_key = Some(StoredNibbles(e.nibbles.0));
        }
        Ok(entry.map(|e| (e.nibbles.0, e.node)))
    }

    /// Returns the currently positioned trie path without advancing, or `None` before positioning
    /// or after exhaustion.
    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        Ok(self.last_key.as_ref().map(|k| k.0))
    }

    /// Clears the current trie position so subsequent traversal starts from an unpositioned state.
    fn reset(&mut self) {
        self.last_key = None;
        self.seeked = false;
    }
}

impl<C> TrieStorageCursor for V2StorageTrieSnapshotCursor<C>
where
    C: DbCursorRO<V2StoragesTrieSnapshot> + DbDupCursorRO<V2StoragesTrieSnapshot> + Send,
{
    /// Selects the hashed account whose storage trie subsequent cursor operations traverse and
    /// resets account-specific position.
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = hashed_address;
        self.last_key = None;
        self.seeked = false;
    }
}

/// Plain hashed-account leaf cursor over [`V2HashedAccountsSnapshot`].
#[derive(Debug)]
pub struct V2HashedAccountSnapshotCursor<C> {
    /// MDBX cursor over materialized hashed-account values.
    cursor: C,
    /// Whether `seek*` has positioned the underlying cursor at least once.
    /// Guards `next` against undefined mdbx behavior on an unpositioned cursor.
    seeked: bool,
}

impl<C> V2HashedAccountSnapshotCursor<C> {
    /// Create a new hashed-account snapshot cursor wrapping `cursor`.
    pub const fn new(cursor: C) -> Self {
        Self { cursor, seeked: false }
    }
}

impl<C> HashedCursor for V2HashedAccountSnapshotCursor<C>
where
    C: DbCursorRO<V2HashedAccountsSnapshot> + Send,
{
    /// The leaf value paired with each hashed key returned by this cursor.
    type Value = Account;

    /// Positions at the first hashed leaf whose key is at least `key`, returning `None` at the end
    /// and propagating backend errors.
    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.seeked = true;
        self.cursor.seek(key)
    }

    /// Advances to the next hashed leaf in ascending key order, returning `None` at the end and
    /// propagating backend errors.
    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        if !self.seeked {
            return self.seek(B256::ZERO);
        }
        self.cursor.next()
    }

    /// Clears the cursor position so the next seek or iteration starts from an unpositioned state.
    fn reset(&mut self) {
        self.seeked = false;
    }
}

/// Plain hashed-storage leaf cursor over [`V2HashedStoragesSnapshot`] (a
/// `DupSort` table). Yields `(storage_key, U256)` pairs, skipping any
/// zero-valued entries defensively (the snapshot writer never inserts zeros,
/// but the cursor mirrors the live [`super::storage::V2StorageCursor`]
/// invariant).
#[derive(Debug)]
pub struct V2HashedStorageSnapshotCursor<C> {
    /// Dup-sorted MDBX cursor over materialized hashed-storage values.
    cursor: C,
    /// Account whose storage slots are visible through this cursor.
    hashed_address: B256,
    /// Whether the underlying cursor has been positioned for the selected account.
    seeked: bool,
}

impl<C> V2HashedStorageSnapshotCursor<C> {
    /// Create a new hashed-storage snapshot cursor scoped to `hashed_address`.
    pub const fn new(cursor: C, hashed_address: B256) -> Self {
        Self { cursor, hashed_address, seeked: false }
    }
}

impl<C> HashedCursor for V2HashedStorageSnapshotCursor<C>
where
    C: DbCursorRO<V2HashedStoragesSnapshot> + DbDupCursorRO<V2HashedStoragesSnapshot> + Send,
{
    /// The leaf value paired with each hashed key returned by this cursor.
    type Value = U256;

    /// Positions at the first hashed leaf whose key is at least `key`, returning `None` at the end
    /// and propagating backend errors.
    fn seek(&mut self, subkey: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.seeked = true;
        let mut entry = self.cursor.seek_by_key_subkey(self.hashed_address, subkey)?;
        while let Some(ref e) = entry {
            if !e.value.is_zero() {
                return Ok(Some((e.key, e.value)));
            }
            entry = self.cursor.next_dup_val()?;
        }
        Ok(None)
    }

    /// Advances to the next hashed leaf in ascending key order, returning `None` at the end and
    /// propagating backend errors.
    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        if !self.seeked {
            return self.seek(B256::ZERO);
        }
        while let Some(e) = self.cursor.next_dup_val()? {
            if !e.value.is_zero() {
                return Ok(Some((e.key, e.value)));
            }
        }
        Ok(None)
    }

    /// Clears the cursor position so the next seek or iteration starts from an unpositioned state.
    fn reset(&mut self) {
        self.seeked = false;
    }
}

impl<C> HashedStorageCursor for V2HashedStorageSnapshotCursor<C>
where
    C: DbCursorRO<V2HashedStoragesSnapshot> + DbDupCursorRO<V2HashedStoragesSnapshot> + Send,
{
    /// Seeks from the zero slot to test emptiness; a non-empty cursor remains at its first visible
    /// leaf and backend failures are returned.
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        Ok(self.seek(B256::ZERO)?.is_none())
    }

    /// Selects the hashed account whose storage leaves subsequent cursor operations will traverse
    /// and resets account-specific state.
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.hashed_address = hashed_address;
        self.seeked = false;
    }
}
