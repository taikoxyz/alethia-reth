//! Shared V2 database ownership for bootstrap recovery and upstream proof providers.

use eyre::eyre;
use reth_db::{Database, DatabaseEnv, transaction::DbTx};
use reth_optimism_trie::{
    OpProofsBackfillStore, OpProofsStorageResult, OpProofsStore,
    db::{MdbxProofsProviderV2, Tables},
};
use std::{path::Path, sync::Arc};

/// V2 storage using upstream providers, with exclusive bootstrap reset on the same MDBX handle.
#[derive(Debug)]
pub struct ProofHistoryDatabase {
    /// The single environment shared by RPC readers, indexing and bootstrap recovery.
    env: DatabaseEnv,
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

        let db = init_db_for::<_, Tables>(path, DatabaseArguments::default())?;
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
        Ok(Self { env: db })
    }

    /// Discards an invalid, never-served bootstrap before copying a new canonical snapshot.
    /// The caller must keep readiness false and stop all indexing writers before calling this.
    pub(super) fn reset_bootstrap(&self) -> eyre::Result<()> {
        let tx = self.env.tx_mut()?;
        Self::clear_v2(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Clears only upstream V2 tables, atomically with the caller's metadata transaction.
    fn clear_v2(tx: &<DatabaseEnv as Database>::TXMut) -> eyre::Result<()> {
        for table in Tables::ALL.iter().map(Tables::name).filter(|name| name.starts_with("V2")) {
            let db = tx.inner().open_db(Some(table))?;
            tx.inner().clear_db(db.dbi())?;
        }
        Ok(())
    }

    /// Reports upstream-compatible proof database metrics without using reth's unrelated tables.
    pub(super) fn report_metrics(&self) -> eyre::Result<()> {
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
        metrics::gauge!("optimism_proof_storage.freelist").set(self.env.freelist()? as f64);
        metrics::gauge!("optimism_proof_storage.page_size")
            .set(self.env.stat()?.page_size() as f64);
        metrics::gauge!("optimism_proof_storage.timed_out_not_aborted_transactions")
            .set(self.env.timed_out_not_aborted_transactions() as f64);
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
        self.provider_rw()
    }
}
