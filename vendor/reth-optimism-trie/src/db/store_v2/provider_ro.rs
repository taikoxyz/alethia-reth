//! [`OpProofsProviderRO`] implementation for [`MdbxProofsProviderV2`].

use super::{
    MdbxProofsProviderV2,
    cursor::{V2AccountCursor, V2AccountTrieCursor, V2StorageCursor, V2StorageTrieCursor},
};
use crate::{
    BlockStateDiff, OpProofsStorageResult,
    api::{OpProofsProviderRO, ProofWindowRange},
    db::{
        ProofWindowKey,
        models::{
            V2AccountTrieChangeSets, V2AccountsTrie, V2AccountsTrieHistory,
            V2HashedAccountChangeSets, V2HashedAccounts, V2HashedAccountsHistory,
            V2HashedStorageChangeSets, V2HashedStorages, V2HashedStoragesHistory,
            V2StorageTrieChangeSets, V2StoragesTrie, V2StoragesTrieHistory,
        },
    },
};
use alloy_eips::NumHash;
use alloy_primitives::B256;
use reth_db::transaction::DbTx;
use std::fmt::Debug;

impl<TX: DbTx + Send + Sync + Debug + 'static> OpProofsProviderRO for MdbxProofsProviderV2<TX> {
    /// The cursor type used to read one account's storage-trie branches at a historical block.
    type StorageTrieCursor<'tx>
        = V2StorageTrieCursor<
        TX::DupCursor<V2StoragesTrie>,
        TX::Cursor<V2StoragesTrieHistory>,
        TX::DupCursor<V2StorageTrieChangeSets>,
    >
    where
        Self: 'tx,
        TX: 'tx;

    /// The cursor type used to read account-trie branches at a historical block.
    type AccountTrieCursor<'tx>
        = V2AccountTrieCursor<
        TX::Cursor<V2AccountsTrie>,
        TX::Cursor<V2AccountsTrieHistory>,
        TX::DupCursor<V2AccountTrieChangeSets>,
    >
    where
        Self: 'tx,
        TX: 'tx;

    /// The cursor type used to read one account's hashed storage leaves at a historical block.
    type StorageCursor<'tx>
        = V2StorageCursor<
        TX::DupCursor<V2HashedStorages>,
        TX::Cursor<V2HashedStoragesHistory>,
        TX::DupCursor<V2HashedStorageChangeSets>,
    >
    where
        Self: 'tx,
        TX: 'tx;

    /// The cursor type used to read hashed account leaves at a historical block.
    type AccountHashedCursor<'tx>
        = V2AccountCursor<
        TX::Cursor<V2HashedAccounts>,
        TX::Cursor<V2HashedAccountsHistory>,
        TX::DupCursor<V2HashedAccountChangeSets>,
    >
    where
        Self: 'tx,
        TX: 'tx;

    /// Returns the inclusive earliest block in the proof window, or `NoBlocksFound` when the window
    /// is empty.
    fn get_earliest_block(&self) -> OpProofsStorageResult<NumHash> {
        self.get_block_number_hash_inner(ProofWindowKey::EarliestBlock)
    }

    /// Returns the inclusive latest block in the proof window, or `NoBlocksFound` when the window
    /// is empty.
    fn get_latest_block(&self) -> OpProofsStorageResult<NumHash> {
        self.get_block_number_hash_inner(ProofWindowKey::LatestBlock)
    }

    /// Returns both inclusive proof-window endpoints from one consistent view, or `NoBlocksFound`
    /// when empty.
    fn get_proof_window(&self) -> OpProofsStorageResult<ProofWindowRange> {
        self.get_proof_window_inner()
    }

    /// Opens a storage-trie cursor for `hashed_address` at or before `max_block_number`; backend
    /// failures are returned.
    fn storage_trie_cursor<'tx>(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> OpProofsStorageResult<Self::StorageTrieCursor<'tx>> {
        let is_latest = self.is_latest_block(max_block_number)?;
        Ok(V2StorageTrieCursor::new(
            self.tx.cursor_dup_read::<V2StoragesTrie>()?,
            self.tx.cursor_read::<V2StoragesTrieHistory>()?,
            self.tx.cursor_read::<V2StoragesTrieHistory>()?,
            self.tx.cursor_dup_read::<V2StorageTrieChangeSets>()?,
            hashed_address,
            max_block_number,
            is_latest,
        ))
    }

    /// Opens an account-trie cursor bounded by `max_block_number`; proof-window lookup and database
    /// cursor construction failures are returned.
    fn account_trie_cursor<'tx>(
        &self,
        max_block_number: u64,
    ) -> OpProofsStorageResult<Self::AccountTrieCursor<'tx>> {
        let is_latest = self.is_latest_block(max_block_number)?;
        Ok(V2AccountTrieCursor::new(
            self.tx.cursor_read::<V2AccountsTrie>()?,
            self.tx.cursor_read::<V2AccountsTrieHistory>()?,
            self.tx.cursor_read::<V2AccountsTrieHistory>()?,
            self.tx.cursor_dup_read::<V2AccountTrieChangeSets>()?,
            max_block_number,
            is_latest,
        ))
    }

    /// Opens a storage-leaf cursor for `hashed_address` at or before `max_block_number`; backend
    /// failures are returned.
    fn storage_hashed_cursor<'tx>(
        &self,
        hashed_address: B256,
        max_block_number: u64,
    ) -> OpProofsStorageResult<Self::StorageCursor<'tx>> {
        let is_latest = self.is_latest_block(max_block_number)?;
        Ok(V2StorageCursor::new(
            self.tx.cursor_dup_read::<V2HashedStorages>()?,
            self.tx.cursor_read::<V2HashedStoragesHistory>()?,
            self.tx.cursor_read::<V2HashedStoragesHistory>()?,
            self.tx.cursor_dup_read::<V2HashedStorageChangeSets>()?,
            hashed_address,
            max_block_number,
            is_latest,
        ))
    }

    /// Opens an account-leaf cursor bounded by `max_block_number`; proof-window lookup and database
    /// cursor construction failures are returned.
    fn account_hashed_cursor<'tx>(
        &self,
        max_block_number: u64,
    ) -> OpProofsStorageResult<Self::AccountHashedCursor<'tx>> {
        let is_latest = self.is_latest_block(max_block_number)?;
        Ok(V2AccountCursor::new(
            self.tx.cursor_read::<V2HashedAccounts>()?,
            self.tx.cursor_read::<V2HashedAccountsHistory>()?,
            self.tx.cursor_read::<V2HashedAccountsHistory>()?,
            self.tx.cursor_dup_read::<V2HashedAccountChangeSets>()?,
            max_block_number,
            is_latest,
        ))
    }

    /// Reconstructs the trie and hashed-post-state diff for `block_number`; a block with no
    /// change-set rows yields an empty diff, while database failures are returned.
    fn fetch_trie_updates(&self, block_number: u64) -> OpProofsStorageResult<BlockStateDiff> {
        Ok(BlockStateDiff {
            sorted_trie_updates: self.fetch_block_trie_updates(block_number)?.into_sorted(),
            sorted_post_state: self.fetch_block_post_state(block_number)?.into_sorted(),
        })
    }
}
