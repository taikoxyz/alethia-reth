//! Implements [`TrieCursorFactory`] and [`HashedCursorFactory`] for [`crate::OpProofsStore`] types.

use crate::{
    api::{OpProofsProviderRO, OpProofsSnapshotProviderRO},
    cursor::{OpProofsHashedAccountCursor, OpProofsHashedStorageCursor, OpProofsTrieCursor},
};
use alloy_primitives::B256;
use reth_db::DatabaseError;
use reth_trie::{hashed_cursor::HashedCursorFactory, trie_cursor::TrieCursorFactory};

/// Factory for creating trie cursors for [`OpProofsProviderRO`].
#[derive(Debug, Clone)]
pub struct OpProofsTrieCursorFactory<P> {
    /// Proof-history provider from which cursors are opened.
    provider: P,
    /// Inclusive block height at which cursor values are reconstructed.
    block_number: u64,
}

impl<P: OpProofsProviderRO> OpProofsTrieCursorFactory<P> {
    /// Initializes new `OpProofsTrieCursorFactory`
    pub const fn new(provider: P, block_number: u64) -> Self {
        Self { provider, block_number }
    }
}

impl<P> TrieCursorFactory for OpProofsTrieCursorFactory<P>
where
    P: OpProofsProviderRO,
{
    /// The cursor type created for account-trie branch traversal.
    type AccountTrieCursor<'a>
        = OpProofsTrieCursor<P::AccountTrieCursor<'a>>
    where
        Self: 'a;
    /// The cursor type created for storage-trie branch traversal.
    type StorageTrieCursor<'a>
        = OpProofsTrieCursor<P::StorageTrieCursor<'a>>
    where
        Self: 'a;

    /// Creates a cursor over account-trie branches in the factory's configured state view;
    /// construction failures are returned.
    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        Ok(OpProofsTrieCursor::new(
            self.provider
                .account_trie_cursor(self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }

    /// Creates a cursor over storage-trie branches for `hashed_address`; construction failures are
    /// returned.
    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        Ok(OpProofsTrieCursor::new(
            self.provider
                .storage_trie_cursor(hashed_address, self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }
}

/// Factory for creating trie cursors backed by a snapshot reader.
///
/// Unlike [`OpProofsTrieCursorFactory`] (which reads history-aware cursors at
/// a given block number), this factory reads directly from the snapshot
/// tables. It carries no block-number context: the snapshot already reflects
/// trie state at a fixed anchor block. The caller is responsible for first
/// resolving that anchor via
/// [`crate::api::OpProofsSnapshotProviderRO::snapshot_anchor`] and ensuring
/// the block being queried matches it.
#[derive(Debug, Clone)]
pub struct SnapshotTrieCursorFactory<P> {
    /// Reader for the already anchored snapshot tables.
    reader: P,
}

impl<P: OpProofsSnapshotProviderRO> SnapshotTrieCursorFactory<P> {
    /// Create a new snapshot-backed trie cursor factory.
    pub const fn new(reader: P) -> Self {
        Self { reader }
    }
}

impl<P> TrieCursorFactory for SnapshotTrieCursorFactory<P>
where
    P: OpProofsSnapshotProviderRO,
{
    /// The cursor type created for account-trie branch traversal.
    type AccountTrieCursor<'a>
        = P::SnapshotAccountTrieCursor<'a>
    where
        Self: 'a;
    /// The cursor type created for storage-trie branch traversal.
    type StorageTrieCursor<'a>
        = P::SnapshotStorageTrieCursor<'a>
    where
        Self: 'a;

    /// Creates a cursor over account-trie branches in the factory's configured state view;
    /// construction failures are returned.
    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        self.reader.snapshot_account_trie_cursor().map_err(Into::<DatabaseError>::into)
    }

    /// Creates a cursor over storage-trie branches for `hashed_address`; construction failures are
    /// returned.
    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        self.reader
            .snapshot_storage_trie_cursor(hashed_address)
            .map_err(Into::<DatabaseError>::into)
    }
}

/// Factory for creating hashed account cursors for [`OpProofsProviderRO`].
#[derive(Debug, Clone)]
pub struct OpProofsHashedAccountCursorFactory<P> {
    /// Proof-history provider from which hashed cursors are opened.
    provider: P,
    /// Inclusive block height at which hashed values are reconstructed.
    block_number: u64,
}

impl<P: OpProofsProviderRO> OpProofsHashedAccountCursorFactory<P> {
    /// Creates a new `OpProofsHashedAccountCursorFactory` instance.
    pub const fn new(provider: P, block_number: u64) -> Self {
        Self { provider, block_number }
    }
}

impl<P> HashedCursorFactory for OpProofsHashedAccountCursorFactory<P>
where
    P: OpProofsProviderRO,
{
    /// The cursor type used to traverse hashed account leaves.
    type AccountCursor<'a>
        = OpProofsHashedAccountCursor<P::AccountHashedCursor<'a>>
    where
        Self: 'a;
    /// The cursor type used to traverse one account's hashed storage leaves.
    type StorageCursor<'a>
        = OpProofsHashedStorageCursor<P::StorageCursor<'a>>
    where
        Self: 'a;

    /// Creates an account-leaf cursor over the factory's configured state view; construction
    /// failures are returned.
    fn hashed_account_cursor(&self) -> Result<Self::AccountCursor<'_>, DatabaseError> {
        Ok(OpProofsHashedAccountCursor::new(
            self.provider
                .account_hashed_cursor(self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }

    /// Creates a storage-leaf cursor for `hashed_address` over the configured state view;
    /// construction failures are returned.
    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        Ok(OpProofsHashedStorageCursor::new(
            self.provider
                .storage_hashed_cursor(hashed_address, self.block_number)
                .map_err(Into::<DatabaseError>::into)?,
        ))
    }
}

/// Factory for creating hashed leaf cursors backed by the snapshot tables.
///
/// Mirrors [`SnapshotTrieCursorFactory`]'s role for hashed leaves: reads
/// directly from [`crate::db::V2HashedAccountsSnapshot`] and
/// [`crate::db::V2HashedStoragesSnapshot`] without history merges. Valid only
/// when the snapshot is `Ready` at the anchor the caller is reading.
#[derive(Debug, Clone)]
pub struct SnapshotHashedCursorFactory<P> {
    /// Reader for hashed account and storage values in the anchored snapshot.
    reader: P,
}

impl<P: OpProofsSnapshotProviderRO> SnapshotHashedCursorFactory<P> {
    /// Create a new snapshot-backed hashed cursor factory.
    pub const fn new(reader: P) -> Self {
        Self { reader }
    }
}

impl<P> HashedCursorFactory for SnapshotHashedCursorFactory<P>
where
    P: OpProofsSnapshotProviderRO,
{
    /// The cursor type used to traverse hashed account leaves.
    type AccountCursor<'a>
        = P::SnapshotHashedAccountCursor<'a>
    where
        Self: 'a;
    /// The cursor type used to traverse one account's hashed storage leaves.
    type StorageCursor<'a>
        = P::SnapshotHashedStorageCursor<'a>
    where
        Self: 'a;

    /// Creates an account-leaf cursor over the factory's configured state view; construction
    /// failures are returned.
    fn hashed_account_cursor(&self) -> Result<Self::AccountCursor<'_>, DatabaseError> {
        self.reader.snapshot_hashed_account_cursor().map_err(Into::<DatabaseError>::into)
    }

    /// Creates a storage-leaf cursor for `hashed_address` over the configured state view;
    /// construction failures are returned.
    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        self.reader
            .snapshot_hashed_storage_cursor(hashed_address)
            .map_err(Into::<DatabaseError>::into)
    }
}
