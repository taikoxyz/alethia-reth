//! MDBX-backed implementation of the proofs-trie sidecar store.
//!
//! This module currently provides the [`MdbxProofsStorage`] wrapper that owns the
//! underlying MDBX environment and performs the one-time schema-version check
//! during construction. Reader/writer provider implementations are added in
//! subsequent tasks.

use super::Tables;
use crate::{
    ProofsStorageError,
    db::models::{PROOFS_TRIE_SCHEMA_VERSION, PROOFS_TRIE_SCHEMA_VERSION_KEY, SchemaVersion},
};
use reth_db::{
    Database, DatabaseEnv, DatabaseError,
    mdbx::{DatabaseArguments, init_db_for},
    transaction::{DbTx, DbTxMut},
};
use std::path::Path;

/// MDBX implementation of the proofs-trie storage layer.
#[derive(Debug)]
pub struct MdbxProofsStorage {
    /// Underlying MDBX environment. Reader/writer impls that consume this field
    /// are added in subsequent tasks; for now the `env()` test accessor is the
    /// only reader outside of [`MdbxProofsStorage::new`].
    #[cfg_attr(not(any(test, feature = "test-utils")), allow(dead_code))]
    env: DatabaseEnv,
}

impl MdbxProofsStorage {
    /// Creates a new [`MdbxProofsStorage`] instance at the given path.
    ///
    /// Opens (or creates) the MDBX environment, ensures all proofs-trie tables
    /// exist, and verifies/initialises the on-disk schema version. A fresh env
    /// gets the current [`PROOFS_TRIE_SCHEMA_VERSION`] written to the
    /// [`SchemaVersion`] table; a pre-existing env with a different stored
    /// version is rejected with [`ProofsStorageError::SchemaVersionMismatch`].
    pub fn new(path: &Path) -> Result<Self, ProofsStorageError> {
        let env = init_db_for::<_, Tables>(path, DatabaseArguments::default())
            .map_err(|e| DatabaseError::Other(format!("Failed to open database: {e}")))?;

        // Verify / initialize schema version. We use a single reserved key
        // (PROOFS_TRIE_SCHEMA_VERSION_KEY) in the SchemaVersion table.
        {
            let tx = env.tx_mut()?;
            match tx.get::<SchemaVersion>(PROOFS_TRIE_SCHEMA_VERSION_KEY)? {
                None => {
                    tx.put::<SchemaVersion>(
                        PROOFS_TRIE_SCHEMA_VERSION_KEY,
                        PROOFS_TRIE_SCHEMA_VERSION,
                    )?;
                }
                Some(found) if found == PROOFS_TRIE_SCHEMA_VERSION => {}
                Some(found) => {
                    return Err(ProofsStorageError::SchemaVersionMismatch {
                        expected: PROOFS_TRIE_SCHEMA_VERSION,
                        found,
                    });
                }
            }
            tx.commit()?;
        }

        Ok(Self { env })
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl MdbxProofsStorage {
    /// Raw access to the underlying MDBX env — test utility only.
    pub const fn env(&self) -> &DatabaseEnv {
        &self.env
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{
        PROOFS_TRIE_SCHEMA_VERSION, PROOFS_TRIE_SCHEMA_VERSION_KEY, SchemaVersion,
    };
    use reth_db::transaction::{DbTx, DbTxMut};
    use tempfile::TempDir;

    #[test]
    fn fresh_env_writes_schema_version() {
        let dir = TempDir::new().unwrap();
        let storage = MdbxProofsStorage::new(dir.path()).unwrap();
        drop(storage);
        // Reopen; the schema-version row must now exist so this must succeed.
        let _reopened = MdbxProofsStorage::new(dir.path()).unwrap();
    }

    #[test]
    fn schema_mismatch_rejected() {
        let dir = TempDir::new().unwrap();
        {
            let storage = MdbxProofsStorage::new(dir.path()).unwrap();
            let env = storage.env();
            let tx = env.tx_mut().unwrap();
            tx.put::<SchemaVersion>(PROOFS_TRIE_SCHEMA_VERSION_KEY, 999).unwrap();
            tx.commit().unwrap();
        }
        let err = MdbxProofsStorage::new(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            ProofsStorageError::SchemaVersionMismatch { expected: 1, found: 999 }
        ));
        // Sanity check the expected constant so this test fails loudly if it changes.
        assert_eq!(PROOFS_TRIE_SCHEMA_VERSION, 1);
    }
}
