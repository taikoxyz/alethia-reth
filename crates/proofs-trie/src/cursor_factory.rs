//! Implements [`TrieCursorFactory`] and [`HashedCursorFactory`] for
//! [`ProofsStore`](crate::ProofsStore) types.

use crate::api::ProofsProviderRO;
use alloy_primitives::B256;
use reth_db::DatabaseError;
use reth_trie::{hashed_cursor::HashedCursorFactory, trie_cursor::TrieCursorFactory};

/// Factory for creating trie cursors for [`ProofsProviderRO`].
#[derive(Debug, Clone)]
pub struct ProofsTrieCursorFactory<P> {
    /// Underlying read-only proofs provider used to open cursors.
    provider: P,
    /// Block number upper bound used when resolving versioned cursors.
    block_number: u64,
}

impl<P: ProofsProviderRO> ProofsTrieCursorFactory<P> {
    /// Initializes a new [`ProofsTrieCursorFactory`].
    pub const fn new(provider: P, block_number: u64) -> Self {
        Self { provider, block_number }
    }
}

impl<P> TrieCursorFactory for ProofsTrieCursorFactory<P>
where
    P: ProofsProviderRO,
{
    type AccountTrieCursor<'a>
        = P::AccountTrieCursor<'a>
    where
        Self: 'a;
    type StorageTrieCursor<'a>
        = P::StorageTrieCursor<'a>
    where
        Self: 'a;

    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        self.provider.account_trie_cursor(self.block_number).map_err(Into::<DatabaseError>::into)
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        self.provider
            .storage_trie_cursor(hashed_address, self.block_number)
            .map_err(Into::<DatabaseError>::into)
    }
}

/// Factory for creating hashed account cursors for [`ProofsProviderRO`].
#[derive(Debug, Clone)]
pub struct ProofsHashedAccountCursorFactory<P> {
    /// Underlying read-only proofs provider used to open cursors.
    provider: P,
    /// Block number upper bound used when resolving versioned cursors.
    block_number: u64,
}

impl<P: ProofsProviderRO> ProofsHashedAccountCursorFactory<P> {
    /// Creates a new [`ProofsHashedAccountCursorFactory`] instance.
    pub const fn new(provider: P, block_number: u64) -> Self {
        Self { provider, block_number }
    }
}

impl<P> HashedCursorFactory for ProofsHashedAccountCursorFactory<P>
where
    P: ProofsProviderRO,
{
    type AccountCursor<'a>
        = P::AccountHashedCursor<'a>
    where
        Self: 'a;
    type StorageCursor<'a>
        = P::StorageCursor<'a>
    where
        Self: 'a;

    fn hashed_account_cursor(&self) -> Result<Self::AccountCursor<'_>, DatabaseError> {
        self.provider.account_hashed_cursor(self.block_number).map_err(Into::<DatabaseError>::into)
    }

    fn hashed_storage_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageCursor<'_>, DatabaseError> {
        self.provider
            .storage_hashed_cursor(hashed_address, self.block_number)
            .map_err(Into::<DatabaseError>::into)
    }
}
