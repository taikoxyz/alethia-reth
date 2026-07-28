//! [`OpProofsSnapshotProviderRO`] implementation for [`MdbxProofsProviderV2`].

use super::{
    MdbxProofsProviderV2,
    cursor::{
        V2AccountTrieSnapshotCursor, V2HashedAccountSnapshotCursor, V2HashedStorageSnapshotCursor,
        V2StorageTrieSnapshotCursor,
    },
};
use crate::{
    OpProofsStorageError, OpProofsStorageResult,
    api::{OpProofsSnapshotProviderRO, SnapshotInitStatus},
    db::{
        SnapshotMeta, SnapshotMetaKey, SnapshotStatus,
        models::{
            V2AccountsTrieSnapshot, V2HashedAccountsSnapshot, V2HashedStoragesSnapshot,
            V2SnapshotMeta, V2StoragesTrieSnapshot,
        },
    },
};
use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use reth_db::{cursor::DbCursorRO, transaction::DbTx};
use std::fmt::Debug;

impl<TX: DbTx> MdbxProofsProviderV2<TX> {
    /// Read the singleton row from [`V2SnapshotMeta`], or
    /// [`OpProofsStorageError::SnapshotNotInitialized`] if absent.
    ///
    /// Internal helper for V2 write paths that need to verify or mutate the
    /// current lifecycle state. External reads go through
    /// [`OpProofsSnapshotProviderRO::snapshot_anchor`].
    pub(super) fn read_snapshot_meta(&self) -> OpProofsStorageResult<SnapshotMeta> {
        let mut cursor = self.tx.cursor_read::<V2SnapshotMeta>()?;
        cursor
            .seek_exact(SnapshotMetaKey::Singleton)?
            .map(|(_, meta)| meta)
            .ok_or(OpProofsStorageError::SnapshotNotInitialized)
    }
}

impl<TX: DbTx + Send + Sync + Debug + 'static> OpProofsSnapshotProviderRO
    for MdbxProofsProviderV2<TX>
{
    /// The cursor type used to traverse account-trie branches in the committed snapshot.
    type SnapshotAccountTrieCursor<'tx>
        = V2AccountTrieSnapshotCursor<TX::Cursor<V2AccountsTrieSnapshot>>
    where
        Self: 'tx,
        TX: 'tx;

    /// The cursor type used to traverse one account's storage-trie branches in the committed
    /// snapshot.
    type SnapshotStorageTrieCursor<'tx>
        = V2StorageTrieSnapshotCursor<TX::DupCursor<V2StoragesTrieSnapshot>>
    where
        Self: 'tx,
        TX: 'tx;

    /// The cursor type used to traverse hashed account leaves in the committed snapshot.
    type SnapshotHashedAccountCursor<'tx>
        = V2HashedAccountSnapshotCursor<TX::Cursor<V2HashedAccountsSnapshot>>
    where
        Self: 'tx,
        TX: 'tx;

    /// The cursor type used to traverse one account's hashed storage leaves in the committed
    /// snapshot.
    type SnapshotHashedStorageCursor<'tx>
        = V2HashedStorageSnapshotCursor<TX::DupCursor<V2HashedStoragesSnapshot>>
    where
        Self: 'tx,
        TX: 'tx;

    /// Returns the committed snapshot anchor, or `SnapshotNotReady` until a snapshot is complete.
    fn snapshot_anchor(&self) -> OpProofsStorageResult<BlockNumHash> {
        match self.read_snapshot_meta() {
            Ok(SnapshotMeta { anchor, status: SnapshotStatus::Ready }) => Ok(anchor),
            Ok(SnapshotMeta { status: SnapshotStatus::Building, .. }) => {
                Err(OpProofsStorageError::SnapshotNotReady {
                    status: SnapshotInitStatus::InProgress,
                })
            }
            Err(OpProofsStorageError::SnapshotNotInitialized) => {
                Err(OpProofsStorageError::SnapshotNotReady {
                    status: SnapshotInitStatus::NotStarted,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Opens the account-trie snapshot table cursor; callers must first validate readiness via
    /// `snapshot_anchor`, and database cursor failures are returned.
    fn snapshot_account_trie_cursor<'tx>(
        &self,
    ) -> OpProofsStorageResult<Self::SnapshotAccountTrieCursor<'tx>> {
        Ok(V2AccountTrieSnapshotCursor::new(self.tx.cursor_read::<V2AccountsTrieSnapshot>()?))
    }

    /// Opens the storage-trie snapshot table cursor for `hashed_address`; callers must first
    /// validate readiness, and database cursor failures are returned.
    fn snapshot_storage_trie_cursor<'tx>(
        &self,
        hashed_address: B256,
    ) -> OpProofsStorageResult<Self::SnapshotStorageTrieCursor<'tx>> {
        Ok(V2StorageTrieSnapshotCursor::new(
            self.tx.cursor_dup_read::<V2StoragesTrieSnapshot>()?,
            hashed_address,
        ))
    }

    /// Opens the hashed-account snapshot table cursor; callers must first validate readiness, and
    /// database cursor failures are returned.
    fn snapshot_hashed_account_cursor<'tx>(
        &self,
    ) -> OpProofsStorageResult<Self::SnapshotHashedAccountCursor<'tx>> {
        Ok(V2HashedAccountSnapshotCursor::new(self.tx.cursor_read::<V2HashedAccountsSnapshot>()?))
    }

    /// Opens the hashed-storage snapshot cursor for `hashed_address`; callers must first validate
    /// readiness, and database cursor failures are returned.
    fn snapshot_hashed_storage_cursor<'tx>(
        &self,
        hashed_address: B256,
    ) -> OpProofsStorageResult<Self::SnapshotHashedStorageCursor<'tx>> {
        Ok(V2HashedStorageSnapshotCursor::new(
            self.tx.cursor_dup_read::<V2HashedStoragesSnapshot>()?,
            hashed_address,
        ))
    }
}
