//! Shared V2 database ownership for bootstrap recovery and upstream proof providers.

use alloy_primitives::B256;
use eyre::eyre;
use reth_db::{
    Database, DatabaseEnv, TableSet,
    cursor::{DbCursorRO, DbCursorRW},
    table::TableInfo,
    tables::CanonicalHeaders,
    transaction::{DbTx, DbTxMut},
};
use reth_optimism_trie::{
    OpProofsBackfillStore, OpProofsStorageResult, OpProofsStore,
    db::{MdbxProofsProviderV2, Tables},
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Adds a height-to-hash journal to the proof database using reth's existing table codec.
struct JournalTables;

impl TableSet for JournalTables {
    /// Creates only the journal table; proof state uses the upstream OP schema.
    fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
        Box::new(std::iter::once(
            Box::new(reth_db::tables::Tables::CanonicalHeaders) as Box<dyn TableInfo>
        ))
    }
}

/// V2 storage using upstream providers, with exclusive bootstrap reset on the same MDBX handle.
#[derive(Debug)]
pub struct ProofHistoryDatabase {
    /// The single environment shared by RPC readers, indexing and bootstrap recovery.
    env: DatabaseEnv,
    /// Stops bootstrap at the next upstream write-batch boundary, without interrupting live saves.
    bootstrap_cancelled: AtomicBool,
}

impl ProofHistoryDatabase {
    /// Opens V2 storage, refusing V1 data and restarting only unpublished snapshot copies.
    ///
    /// This must run before any reader or writer opens the database. Completed windows, including
    /// partially backfilled history, are retained; an interrupted initial copy is discarded because
    /// the node's current-state tables may have advanced since its source transaction was lost.
    pub(super) fn open(path: &Path) -> eyre::Result<Self> {
        use reth_db::{
            mdbx::{DatabaseArguments, init_db_for},
            transaction::DbTx,
        };
        use reth_optimism_trie::db::{
            AccountTrieHistory, BlockChangeSet, HashedAccountHistory, HashedStorageHistory,
            ProofWindow, ProofWindowKey, StorageTrieHistory, Tables, V2ProofWindow,
        };

        let mut db = init_db_for::<_, Tables>(path, DatabaseArguments::default())?;
        db.create_and_track_tables_for::<JournalTables>()?;
        let tx = db.tx_mut()?;
        if tx.entries::<ProofWindow>()? > 0 ||
            tx.entries::<AccountTrieHistory>()? > 0 ||
            tx.entries::<StorageTrieHistory>()? > 0 ||
            tx.entries::<HashedAccountHistory>()? > 0 ||
            tx.entries::<HashedStorageHistory>()? > 0 ||
            tx.entries::<BlockChangeSet>()? > 0
        {
            return Err(eyre!(
                "V1 proof-history storage at {path:?} requires rebuilding into a new, empty \
                 --proofs-history.storage-path; use --proofs-history.backfill-window-only to \
                 rebuild retained history from unpruned node changesets; the V1 data is unchanged"
            ));
        }
        if tx.get::<V2ProofWindow>(ProofWindowKey::EarliestBlock)?.is_none() {
            if tx.get::<V2ProofWindow>(ProofWindowKey::LatestBlock)?.is_some() {
                return Err(eyre!("proof-history latest block exists without an earliest anchor"));
            }
            Self::clear_v2(&tx)?;
        }
        tx.commit()?;
        Ok(Self { env: db, bootstrap_cancelled: AtomicBool::new(false) })
    }

    /// Discards an invalid retained anchor or unfinished bootstrap before copying a new snapshot.
    /// The caller must keep readiness false and stop all indexing writers before calling this.
    pub(super) fn reset_bootstrap(&self) -> eyre::Result<()> {
        let tx = self.env.tx_mut()?;
        Self::clear_v2(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Clears only upstream V2 tables, atomically with the caller's metadata transaction.
    fn clear_v2(tx: &<DatabaseEnv as Database>::TXMut) -> eyre::Result<()> {
        tx.clear::<CanonicalHeaders>()?;
        // The pinned upstream schema explicitly reserves the V2 prefix for all V2 tables,
        // including auxiliary snapshots. Keep the V1 tables intact for rollback.
        for table in Tables::ALL.iter().map(Tables::name).filter(|name| name.starts_with("V2")) {
            let db = tx.inner().open_db(Some(table))?;
            tx.inner().clear_db(db.dbi())?;
        }
        Ok(())
    }

    /// Records hashes before their corresponding proof writes can become durable.
    /// Only unindexed heights may be replaced; callers must unwind before recording a new fork.
    pub(super) fn record_hashes(
        &self,
        hashes: impl IntoIterator<Item = (u64, B256)>,
    ) -> eyre::Result<()> {
        let tx = self.env.tx_mut()?;
        for (number, hash) in hashes {
            tx.put::<CanonicalHeaders>(number, hash)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Reads the indexed branch's hash, independently of reth's current canonical branch.
    pub(super) fn indexed_hash(&self, number: u64) -> eyre::Result<Option<B256>> {
        Ok(self.env.tx()?.get::<CanonicalHeaders>(number)?)
    }

    /// Removes obsolete journal entries after proof pruning or a completed unwind.
    pub(super) fn retain_hashes(&self, earliest: u64, latest: u64) -> eyre::Result<()> {
        let tx = self.env.tx_mut()?;
        let mut cursor = tx.cursor_write::<CanonicalHeaders>()?;
        while cursor.first()?.is_some_and(|(n, _)| n < earliest) {
            cursor.delete_current()?;
        }
        while cursor.last()?.is_some_and(|(n, _)| n > latest) {
            cursor.delete_current()?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Requests cooperative bootstrap cancellation; completed batches remain resumable.
    pub(super) fn cancel_bootstrap(&self) {
        self.bootstrap_cancelled.store(true, Ordering::Release);
    }

    /// Clears cancellation before launching a new bootstrap attempt.
    pub(super) fn resume_bootstrap(&self) {
        self.bootstrap_cancelled.store(false, Ordering::Release);
    }

    /// Returns whether shutdown or a canonical reorg cancelled the current bootstrap.
    pub(super) fn bootstrap_cancelled(&self) -> bool {
        self.bootstrap_cancelled.load(Ordering::Acquire)
    }

    /// Stops new bootstrap transactions while allowing existing transactions to finish.
    pub(super) fn check_bootstrap_cancelled(&self) -> OpProofsStorageResult<()> {
        if self.bootstrap_cancelled() {
            return Err(
                reth_db::DatabaseError::Other("proof-history bootstrap cancelled".into()).into()
            );
        }
        Ok(())
    }

    /// Reports upstream-compatible proof database metrics without using reth's unrelated tables.
    pub(super) fn report_metrics(&self) -> eyre::Result<()> {
        let tables = (|| -> eyre::Result<()> {
            let tx = self.env.tx()?;
            for table in Tables::ALL.iter().map(Tables::name) {
                let db = tx.inner().open_db(Some(table))?;
                let stats = tx.inner().db_stat(db.dbi())?;
                let pages = stats.leaf_pages() + stats.branch_pages() + stats.overflow_pages();
                metrics::gauge!("optimism_proof_storage.table_size", "table" => table)
                    .set((stats.page_size() as usize * pages) as f64);
                metrics::gauge!("optimism_proof_storage.table_entries", "table" => table)
                    .set(stats.entries() as f64);
                for (kind, count) in [
                    ("leaf", stats.leaf_pages()),
                    ("branch", stats.branch_pages()),
                    ("overflow", stats.overflow_pages()),
                ] {
                    metrics::gauge!("optimism_proof_storage.table_pages", "table" => table, "type" => kind)
                    .set(count as f64);
                }
            }
            Ok(())
        })();
        // A table-stat failure must not suppress independent environment gauges.
        let freelist = self
            .env
            .freelist()
            .map(|value| metrics::gauge!("optimism_proof_storage.freelist").set(value as f64));
        let page_size = self.env.stat().map(|value| {
            metrics::gauge!("optimism_proof_storage.page_size").set(value.page_size() as f64)
        });
        metrics::gauge!("optimism_proof_storage.timed_out_not_aborted_transactions")
            .set(self.env.timed_out_not_aborted_transactions() as f64);
        tables?;
        freelist?;
        page_size?;
        Ok(())
    }
}

impl OpProofsStore for ProofHistoryDatabase {
    /// Upstream historical read transaction.
    type ProviderRO<'a> = Arc<MdbxProofsProviderV2<<DatabaseEnv as Database>::TX>>;
    /// Upstream live write transaction.
    type ProviderRw<'a> = MdbxProofsProviderV2<<DatabaseEnv as Database>::TXMut>;
    /// Upstream snapshot-copy transaction.
    type Initializer<'a> = Self::ProviderRw<'a>;

    /// Opens a consistent snapshot for historical reads.
    fn provider_ro(&self) -> OpProofsStorageResult<Self::ProviderRO<'_>> {
        Ok(Arc::new(MdbxProofsProviderV2::new(self.env.tx()?)))
    }
    /// Opens the sole MDBX writer used for live indexing.
    fn provider_rw(&self) -> OpProofsStorageResult<Self::ProviderRw<'_>> {
        Ok(MdbxProofsProviderV2::new(self.env.tx_mut()?))
    }
    /// Opens the same upstream writer for current-state initialization.
    fn initialization_provider(&self) -> OpProofsStorageResult<Self::Initializer<'_>> {
        self.check_bootstrap_cancelled()?;
        self.provider_rw()
    }
}

impl OpProofsBackfillStore for ProofHistoryDatabase {
    /// Upstream backward-history write transaction.
    type BackfillProvider<'a> = Self::ProviderRw<'a>;
    /// Upstream auxiliary snapshot read transaction.
    type SnapshotProviderRO<'a> = Self::ProviderRO<'a>;
    /// Upstream auxiliary snapshot write transaction.
    type SnapshotInitializer<'a> = Self::ProviderRw<'a>;

    /// Opens a writer for one atomic backward batch.
    fn backfill_provider(&self) -> OpProofsStorageResult<Self::BackfillProvider<'_>> {
        self.check_bootstrap_cancelled()?;
        self.provider_rw()
    }
    /// Opens a reader for upstream snapshot-assisted backfill.
    fn snapshot_provider_ro(&self) -> OpProofsStorageResult<Self::SnapshotProviderRO<'_>> {
        self.provider_ro()
    }
    /// Opens a writer for an upstream auxiliary snapshot.
    fn snapshot_initialization_provider(
        &self,
    ) -> OpProofsStorageResult<Self::SnapshotInitializer<'_>> {
        self.check_bootstrap_cancelled()?;
        self.provider_rw()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::BlockNumHash;
    use reth_optimism_trie::{
        OpProofsBackfillStore, OpProofsInitProvider, OpProofsSnapshotInitProvider,
        api::SnapshotInitStatus,
    };

    #[test]
    fn reset_clears_durable_journal_and_auxiliary_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ProofHistoryDatabase::open(dir.path()).unwrap();
        storage.record_hashes([(7, B256::repeat_byte(7))]).unwrap();
        let init = storage.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(7, B256::repeat_byte(7))).unwrap();
        init.commit_initial_state().unwrap();
        OpProofsInitProvider::commit(init).unwrap();
        let snapshot = storage.snapshot_initialization_provider().unwrap();
        snapshot.set_snapshot_init_anchor(BlockNumHash::new(7, B256::repeat_byte(7))).unwrap();
        snapshot.store_hashed_accounts_snapshot(vec![(B256::ZERO, Default::default())]).unwrap();
        OpProofsSnapshotInitProvider::commit(snapshot).unwrap();
        drop(storage);
        let storage = ProofHistoryDatabase::open(dir.path()).unwrap();
        assert_eq!(storage.indexed_hash(7).unwrap(), Some(B256::repeat_byte(7)));
        storage.reset_bootstrap().unwrap();
        assert_eq!(storage.indexed_hash(7).unwrap(), None);
        assert_eq!(
            storage
                .env
                .tx()
                .unwrap()
                .entries::<reth_optimism_trie::db::V2HashedAccountsSnapshot>()
                .unwrap(),
            0
        );
        assert!(matches!(
            storage
                .snapshot_initialization_provider()
                .unwrap()
                .snapshot_init_anchor()
                .unwrap()
                .status,
            SnapshotInitStatus::NotStarted
        ));
    }

    #[test]
    fn cancellation_finishes_open_batches_and_allows_live_writes() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ProofHistoryDatabase::open(dir.path()).unwrap();
        let batch = storage.backfill_provider().unwrap();
        storage.cancel_bootstrap();
        reth_optimism_trie::OpProofsBackfillProvider::commit(batch).unwrap();
        assert!(storage.backfill_provider().is_err());
        assert!(storage.initialization_provider().is_err());
        assert!(storage.snapshot_initialization_provider().is_err());
        reth_optimism_trie::OpProofsProviderRw::commit(storage.provider_rw().unwrap()).unwrap();
        storage.resume_bootstrap();
        reth_optimism_trie::OpProofsBackfillProvider::commit(storage.backfill_provider().unwrap())
            .unwrap();
    }

    #[test]
    fn metrics_use_upstream_names_labels_and_table_values() {
        use metrics::{
            Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
        };
        use std::sync::Mutex;
        #[derive(Default)]
        struct Capture(Mutex<Vec<(Key, Arc<metrics::atomics::AtomicU64>)>>);
        impl Recorder for Capture {
            fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
            fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
            fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
            fn register_counter(&self, _: &Key, _: &Metadata<'_>) -> Counter {
                Counter::noop()
            }
            fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
                Histogram::noop()
            }
            fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
                let value = Arc::new(metrics::atomics::AtomicU64::new(0));
                self.0.lock().unwrap().push((key.clone(), value.clone()));
                Gauge::from_arc(value)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let storage = ProofHistoryDatabase::open(dir.path()).unwrap();
        let init = storage.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(7, B256::repeat_byte(7))).unwrap();
        init.commit_initial_state().unwrap();
        OpProofsInitProvider::commit(init).unwrap();
        let capture = Capture::default();
        metrics::with_local_recorder(&capture, || storage.report_metrics()).unwrap();
        let gauges = capture.0.lock().unwrap();
        let value = |name: &str, labels: Vec<(&str, &str)>| {
            let key = Key::from_parts(
                name.to_owned(),
                labels
                    .into_iter()
                    .map(|(k, v)| metrics::Label::new(k.to_owned(), v.to_owned()))
                    .collect::<Vec<_>>(),
            );
            f64::from_bits(
                gauges.iter().find(|(k, _)| *k == key).unwrap().1.load(Ordering::Relaxed),
            )
        };
        let entries =
            storage.env.tx().unwrap().entries::<reth_optimism_trie::db::V2ProofWindow>().unwrap();
        assert!(entries > 0);
        assert_eq!(
            value("optimism_proof_storage.table_entries", vec![("table", "V2ProofWindow")]),
            entries as f64
        );
        assert!(value("optimism_proof_storage.page_size", vec![]) > 0.0);
        value(
            "optimism_proof_storage.table_pages",
            vec![("table", "V2ProofWindow"), ("type", "leaf")],
        );
        value("optimism_proof_storage.freelist", vec![]);
        value("optimism_proof_storage.timed_out_not_aborted_transactions", vec![]);
    }
}
