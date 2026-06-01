//! Read-side query helpers for the Taiko-specific db tables.
//!
//! Each helper takes a `&impl DbTx` (the caller is responsible for opening the read
//! transaction via `DatabaseProviderFactory::database_provider_ro()?.into_tx()`) and
//! exposes a typed read API over [`crate::model::StoredL1OriginTable`] etc. The helpers
//! exist so consumers that aren't the engine API (e.g. the `debug_` RPC namespace) don't
//! reach into table internals — same justification as having a typed `StateProvider`
//! wrapper around the raw account table.

use alloy_primitives::BlockNumber;
use reth_db_api::{DatabaseError, transaction::DbTx};

use crate::model::StoredL1OriginTable;

/// Read the L1 origin block height (`Proposal.originBlockNumber`) recorded for a given L2 block.
///
/// The taiko-client driver writes [`crate::model::StoredL1Origin`] into this table (via
/// `taikoAuth_updateL1Origin`) from the on-chain `Proposed` event. The L1 precompiles' 256-block
/// lookback hangs off `originBlockHash = blockhash(originBlockNumber)`, so re-execution paths
/// (e.g. `debug_executionWitness`) look the origin up here and feed it into the executor context.
///
/// `None` when no entry exists (pre-Shasta block, or the driver hasn't committed yet); callers
/// must surface an error or leave the window unset — never silently use the wrong pivot.
pub fn read_l1_origin_block_height<Tx: DbTx>(
    tx: &Tx,
    l2_block_number: BlockNumber,
) -> Result<Option<u64>, DatabaseError> {
    let stored = tx.get::<StoredL1OriginTable>(l2_block_number)?;
    // `l1_block_height` is stored as `U256` (serde-compat with `RpcL1Origin`) but real L1 block
    // numbers fit in `u64`; reject an overflowing value rather than silently truncate.
    Ok(stored.and_then(|s| u64::try_from(s.l1_block_height).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{STORED_L1_HEAD_ORIGIN_KEY, StoredL1Origin, Tables};
    use alloy_primitives::{B256, U256};
    use reth_db::{
        DatabaseEnv,
        mdbx::{DatabaseArguments, init_db_for},
        test_utils::TempDatabase,
    };
    use reth_db_api::{Database, transaction::DbTxMut};
    use std::sync::Arc;

    /// Build a temp database with the Taiko-specific tables (`StoredL1OriginTable` etc.) created.
    /// Reth's `create_test_rw_db()` only creates the default Tables set; we need ours too.
    fn temp_taiko_db() -> Arc<TempDatabase<DatabaseEnv>> {
        let path = reth_db::test_utils::tempdir_path();
        let db = init_db_for::<_, Tables>(&path, DatabaseArguments::test()).expect("init_db_for");
        Arc::new(TempDatabase::new(db, path))
    }

    fn sample_stored_origin(l1_height: u64) -> StoredL1Origin {
        StoredL1Origin {
            block_id: U256::from(1u64),
            l2_block_hash: B256::from([0x11u8; 32]),
            l1_block_height: U256::from(l1_height),
            l1_block_hash: B256::from([0x22u8; 32]),
            build_payload_args_id: [0u8; 8],
            is_forced_inclusion: false,
            signature: [0u8; 65],
        }
    }

    #[test]
    fn read_l1_origin_block_height_returns_stored_value() {
        let db = temp_taiko_db();
        // Write a fixture entry for L2 block #5 referencing L1 block #1000.
        let l2_block_number: BlockNumber = 5;
        let l1_height: u64 = 1000;
        let tx = db.tx_mut().unwrap();
        tx.put::<StoredL1OriginTable>(l2_block_number, sample_stored_origin(l1_height)).unwrap();
        tx.commit().unwrap();

        let tx = db.tx().unwrap();
        let read = read_l1_origin_block_height(&tx, l2_block_number).unwrap();
        assert_eq!(read, Some(l1_height));
    }

    #[test]
    fn read_l1_origin_block_height_returns_none_for_missing_entry() {
        let db = temp_taiko_db();
        let tx = db.tx().unwrap();
        let read = read_l1_origin_block_height(&tx, 42).unwrap();
        assert_eq!(read, None);
    }

    /// Sanity: the helper doesn't accidentally read from `StoredL1HeadOriginTable` (a sibling
    /// table that uses the same key type but stores `BlockNumber`, not `StoredL1Origin`).
    /// If someone refactors the table macro and confuses the type parameter, this regression
    /// guard catches it.
    #[test]
    fn read_l1_origin_block_height_does_not_confuse_sibling_tables() {
        let db = temp_taiko_db();
        let tx = db.tx_mut().unwrap();
        // Only write to the *head* table.
        tx.put::<crate::model::StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, 999).unwrap();
        tx.commit().unwrap();

        let tx = db.tx().unwrap();
        let read = read_l1_origin_block_height(&tx, STORED_L1_HEAD_ORIGIN_KEY).unwrap();
        // Should be None — we only wrote to a sibling table with the same key.
        assert_eq!(read, None);
    }
}
