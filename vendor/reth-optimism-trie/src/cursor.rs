//! Implementation of [`HashedCursor`] and [`TrieCursor`] for
//! [`OpProofsStorage`](crate::OpProofsStorage).

use alloy_primitives::{B256, U256};
use derive_more::Constructor;
use reth_db::DatabaseError;
use reth_primitives_traits::Account;
use reth_trie::{
    hashed_cursor::{HashedCursor, HashedStorageCursor},
    trie_cursor::{TrieCursor, TrieStorageCursor},
};
use reth_trie_common::{BranchNodeCompact, Nibbles};

/// Manages reading storage or account trie nodes from [`TrieCursor`].
#[derive(Debug, Clone, Constructor)]
pub struct OpProofsTrieCursor<C>(pub C);

impl<C> TrieCursor for OpProofsTrieCursor<C>
where
    C: TrieCursor,
{
    #[inline]
    /// Positions at the branch exactly matching `key`, returning `None` when absent and propagating
    /// backend errors.
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.0.seek_exact(key)
    }

    #[inline]
    /// Positions at the first trie branch whose path is at least `key`, returning `None` at the end
    /// and propagating backend errors.
    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.0.seek(key)
    }

    #[inline]
    /// Advances to the next trie branch in nibble-path order, returning `None` at the end and
    /// propagating backend errors.
    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.0.next()
    }

    #[inline]
    /// Returns the currently positioned trie path without advancing, or `None` before positioning
    /// or after exhaustion.
    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        self.0.current()
    }

    #[inline]
    /// Delegates reset without retaining any additional wrapper position.
    fn reset(&mut self) {
        self.0.reset()
    }
}

impl<C> TrieStorageCursor for OpProofsTrieCursor<C>
where
    C: TrieStorageCursor,
{
    #[inline]
    /// Delegates storage-account selection and preserves the wrapped cursor's positioning
    /// behavior.
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.0.set_hashed_address(hashed_address)
    }
}

/// Manages reading hashed account nodes from external storage.
#[derive(Debug, Clone, Constructor)]
pub struct OpProofsHashedAccountCursor<C>(pub C);

impl<C> HashedCursor for OpProofsHashedAccountCursor<C>
where
    C: HashedCursor<Value = Account> + Send,
{
    /// The leaf value paired with each hashed key returned by this cursor.
    type Value = Account;

    #[inline]
    /// Positions at the first hashed leaf whose key is at least `key`, returning `None` at the end
    /// and propagating backend errors.
    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.0.seek(key)
    }

    #[inline]
    /// Advances to the next hashed leaf in ascending key order, returning `None` at the end and
    /// propagating backend errors.
    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.0.next()
    }

    #[inline]
    /// Delegates reset without retaining any additional wrapper position.
    fn reset(&mut self) {
        self.0.reset()
    }
}

/// Manages reading hashed storage nodes from external storage.
#[derive(Debug, Clone, Constructor)]
pub struct OpProofsHashedStorageCursor<C>(pub C);

impl<C> HashedCursor for OpProofsHashedStorageCursor<C>
where
    C: HashedCursor<Value = U256> + Send,
{
    /// The leaf value paired with each hashed key returned by this cursor.
    type Value = U256;

    #[inline]
    /// Positions at the first hashed leaf whose key is at least `key`, returning `None` at the end
    /// and propagating backend errors.
    fn seek(&mut self, key: B256) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.0.seek(key)
    }

    #[inline]
    /// Advances to the next hashed leaf in ascending key order, returning `None` at the end and
    /// propagating backend errors.
    fn next(&mut self) -> Result<Option<(B256, Self::Value)>, DatabaseError> {
        self.0.next()
    }

    #[inline]
    /// Delegates reset without retaining any additional wrapper position.
    fn reset(&mut self) {
        self.0.reset()
    }
}

impl<C> HashedStorageCursor for OpProofsHashedStorageCursor<C>
where
    C: HashedStorageCursor<Value = U256> + Send,
{
    #[inline]
    /// Delegates the empty-storage check and preserves the wrapped cursor's positioning and error
    /// behavior.
    fn is_storage_empty(&mut self) -> Result<bool, DatabaseError> {
        self.0.is_storage_empty()
    }

    #[inline]
    /// Delegates hashed-storage account selection and preserves the wrapped cursor's positioning
    /// behavior.
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.0.set_hashed_address(hashed_address)
    }
}
