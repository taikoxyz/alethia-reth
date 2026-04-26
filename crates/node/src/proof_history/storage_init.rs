//! Proof-history storage bootstrap, metadata, and window-start state machine.

use super::init::ProofHistoryInitializationJob;
use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use eyre::{WrapErr, eyre};
use reth::providers::{BlockNumReader, DBProvider, DatabaseProviderFactory, HeaderProvider};
use reth_db::Database;
use reth_optimism_trie::{OpProofsStore, api::OpProofsProviderRO};
use reth_storage_api::{
    ChainStateBlockReader, ChangeSetReader, StorageChangeSetReader, StorageSettingsCache,
};
use reth_trie_common::HashedPostStateSorted;
use reth_trie_db::{DatabaseHashedPostState, LegacyKeyAdapter, PackedKeyAdapter};
use std::{
    fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};
use tracing::info;

/// Returns whether proof-history storage needs an initial current-state snapshot.
pub(super) fn proof_history_storage_needs_initialization<Storage>(
    storage: &Storage,
) -> eyre::Result<bool>
where
    Storage: OpProofsStore,
{
    let provider_ro = storage.provider_ro()?;
    Ok(provider_ro.get_earliest_block_number()?.is_none() ||
        provider_ro.get_latest_block_number()?.is_none())
}

/// Safety threshold for automatic proof-history pruning on startup.
pub(super) const PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS: u64 = 1000;

/// File stored beside the proof-history MDBX database to validate historical init resume targets.
pub(super) const PROOF_HISTORY_HISTORICAL_INIT_METADATA_FILE: &str = "taiko-historical-init-target";

/// Returns the metadata file path used to validate in-progress historical initialization.
pub(super) fn proof_history_historical_init_metadata_path(storage_path: &Path) -> PathBuf {
    storage_path.join(PROOF_HISTORY_HISTORICAL_INIT_METADATA_FILE)
}

/// Metadata that identifies the historical initialization source state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HistoricalInitMetadata {
    /// Historical proof-history start block being initialized.
    pub(super) start_block: BlockNumHash,
    /// Canonical source block used to rewind the node state into the historical start block.
    pub(super) target_block: BlockNumHash,
}

impl HistoricalInitMetadata {
    /// Encodes metadata as a small line-oriented text file.
    pub(super) fn encode(self) -> String {
        format!(
            "version=1\nstart_number={}\nstart_hash={:?}\ntarget_number={}\ntarget_hash={:?}\n",
            self.start_block.number,
            self.start_block.hash,
            self.target_block.number,
            self.target_block.hash
        )
    }

    /// Decodes metadata written by [`Self::encode`].
    pub(super) fn decode(contents: &str) -> eyre::Result<Self> {
        let mut version = None;
        let mut start_number = None;
        let mut start_hash = None;
        let mut target_number = None;
        let mut target_hash = None;

        for line in contents.lines() {
            let Some((key, value)) = line.split_once('=') else {
                return Err(eyre!("invalid historical init metadata line: {line:?}"));
            };

            match key {
                "version" => version = Some(value),
                "start_number" => {
                    start_number = Some(value.parse::<u64>().wrap_err("invalid start_number")?)
                }
                "start_hash" => {
                    start_hash = Some(B256::from_str(value).wrap_err("invalid start_hash")?)
                }
                "target_number" => {
                    target_number = Some(value.parse::<u64>().wrap_err("invalid target_number")?)
                }
                "target_hash" => {
                    target_hash = Some(B256::from_str(value).wrap_err("invalid target_hash")?)
                }
                unknown => {
                    return Err(eyre!("unknown historical init metadata key: {unknown}"));
                }
            }
        }

        if version != Some("1") {
            return Err(eyre!("unsupported historical init metadata version"));
        }

        Ok(Self {
            start_block: BlockNumHash::new(
                start_number.ok_or_else(|| eyre!("missing start_number"))?,
                start_hash.ok_or_else(|| eyre!("missing start_hash"))?,
            ),
            target_block: BlockNumHash::new(
                target_number.ok_or_else(|| eyre!("missing target_number"))?,
                target_hash.ok_or_else(|| eyre!("missing target_hash"))?,
            ),
        })
    }
}

/// Writes historical initialization metadata before creating the OP storage anchor.
pub(super) fn write_historical_init_metadata(
    path: &Path,
    metadata: HistoricalInitMetadata,
) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).wrap_err_with(|| {
            format!("failed to create historical init metadata directory at {parent:?}")
        })?;
    }
    fs::write(path, metadata.encode())
        .wrap_err_with(|| format!("failed to write historical init metadata at {path:?}"))
}

/// Reads historical initialization metadata if it exists.
pub(super) fn read_historical_init_metadata(
    path: &Path,
) -> eyre::Result<Option<HistoricalInitMetadata>> {
    match fs::read_to_string(path) {
        Ok(contents) => HistoricalInitMetadata::decode(&contents).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .wrap_err_with(|| format!("failed to read historical init metadata at {path:?}")),
    }
}

/// Removes historical initialization metadata after successful initialization.
pub(super) fn remove_historical_init_metadata(path: &Path) -> eyre::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .wrap_err_with(|| format!("failed to remove historical init metadata at {path:?}")),
    }
}

/// Validates that in-progress historical initialization is resuming the same source state.
pub(super) fn validate_historical_init_metadata_file(
    metadata_path: Option<&Path>,
    expected_metadata: HistoricalInitMetadata,
) -> eyre::Result<()> {
    let Some(path) = metadata_path else {
        return Err(eyre!(
            "in-progress historical proof-history initialization cannot resume without target metadata"
        ));
    };
    let Some(stored_metadata) = read_historical_init_metadata(path)? else {
        return Err(eyre!(
            "missing historical proof-history initialization metadata at {path:?}; wipe proof-history storage and restart initialization"
        ));
    };

    if stored_metadata != expected_metadata {
        return Err(eyre!(
            "historical proof-history initialization target changed: stored={stored_metadata:?} current={expected_metadata:?}; wipe proof-history storage and restart initialization"
        ));
    }

    Ok(())
}

/// Returns the highest block proof-history may backfill without running ahead of execution.
pub(super) fn proof_history_backfill_target(
    latest_stored: u64,
    requested_target: u64,
    executed_head: u64,
) -> Option<u64> {
    let target = requested_target.min(executed_head);
    (target > latest_stored).then_some(target)
}

/// Decision for delayed proof-history initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DelayedProofHistoryStart {
    /// No finalized block has been observed yet.
    WaitForFinalized,
    /// Local execution has not reached the derived proof-history start block.
    WaitForExecution {
        /// First block where proof-history should initialize.
        start_block: u64,
    },
    /// Local execution has passed the derived proof-history start block.
    MissedStart {
        /// First block where proof-history should have initialized.
        start_block: u64,
    },
    /// Local execution is exactly at the derived proof-history start block.
    Ready {
        /// First block where proof-history should initialize.
        start_block: u64,
    },
}

/// Work that empty proof-history storage must perform before live indexing can start.
#[derive(Debug)]
pub(super) enum ProofHistoryInitializationAction {
    /// Initialization is waiting for a stable window anchor or local execution.
    Wait,
    /// Initialize from the node's current canonical state.
    CurrentState,
    /// Build an initial state for a missed historical proof-history window.
    HistoricalWindow {
        /// First block retained in proof-history storage.
        start_block: u64,
        /// Current local execution head used as the source state for reverse changesets.
        target_block: u64,
    },
}

/// Returns the first block retained by a finalized proof-history window.
pub(super) const fn proof_history_window_start_block(finalized_block: u64, window: u64) -> u64 {
    finalized_block.saturating_sub(window)
}

/// Computes whether delayed proof-history initialization can start.
pub(super) fn delayed_proof_history_start(
    finalized_block: Option<u64>,
    executed_head: u64,
    window: u64,
) -> DelayedProofHistoryStart {
    let Some(finalized_block) = finalized_block else {
        return DelayedProofHistoryStart::WaitForFinalized;
    };

    let start_block = proof_history_window_start_block(finalized_block, window);
    match executed_head.cmp(&start_block) {
        std::cmp::Ordering::Less => DelayedProofHistoryStart::WaitForExecution { start_block },
        std::cmp::Ordering::Equal => DelayedProofHistoryStart::Ready { start_block },
        std::cmp::Ordering::Greater => DelayedProofHistoryStart::MissedStart { start_block },
    }
}

/// Initializes empty proof-history storage from the node's current canonical state.
pub(super) fn initialize_proof_history_storage<Provider, Storage>(
    provider: &Provider,
    storage: Storage,
) -> eyre::Result<()>
where
    Provider: BlockNumReader + DatabaseProviderFactory,
    Provider::Provider: StorageSettingsCache,
    <Provider::DB as Database>::TX: Sync,
    Storage: OpProofsStore + Send,
{
    if !proof_history_storage_needs_initialization(&storage)? {
        return Ok(());
    }

    let chain_info = provider.chain_info()?;
    info!(
        target: "reth::taiko::proof_history",
        best_number = chain_info.best_number,
        best_hash = ?chain_info.best_hash,
        "initializing proof-history storage from current canonical state"
    );

    let db_provider = provider.database_provider_ro()?.disable_long_read_transaction_safety();
    let storage_v2 = db_provider.cached_storage_settings().is_v2();
    let db_tx = db_provider.into_tx();
    let init_job = ProofHistoryInitializationJob::new(storage, db_tx);
    if storage_v2 {
        init_job
            .run_with_adapter::<PackedKeyAdapter>(chain_info.best_number, chain_info.best_hash)?;
    } else {
        init_job
            .run_with_adapter::<LegacyKeyAdapter>(chain_info.best_number, chain_info.best_hash)?;
    }

    info!(
        target: "reth::taiko::proof_history",
        best_number = chain_info.best_number,
        best_hash = ?chain_info.best_hash,
        "proof-history storage initialized"
    );

    Ok(())
}

/// Initializes empty proof-history storage from a historical canonical state.
pub(super) fn initialize_historical_proof_history_storage<Provider, Storage>(
    provider: &Provider,
    storage: Storage,
    metadata_path: Option<&Path>,
    start_block: u64,
    target_block: u64,
) -> eyre::Result<()>
where
    Provider: BlockNumReader + DatabaseProviderFactory,
    Provider::Provider: BlockNumReader
        + ChainStateBlockReader
        + ChangeSetReader
        + DBProvider
        + HeaderProvider
        + StorageChangeSetReader
        + StorageSettingsCache,
    <Provider::DB as Database>::TX: Sync,
    Storage: OpProofsStore + Send,
{
    if !proof_history_storage_needs_initialization(&storage)? {
        return Ok(());
    }

    if start_block > target_block {
        return Err(eyre!(
            "proof-history historical initialization start block {start_block} is above target block {target_block}"
        ));
    }

    let db_provider = provider.database_provider_ro()?.disable_long_read_transaction_safety();
    let anchor_header = db_provider
        .sealed_header(start_block)?
        .ok_or_else(|| eyre!("missing proof-history anchor header {start_block}"))?;
    let anchor = BlockNumHash::new(start_block, anchor_header.hash());
    let target_header = db_provider
        .sealed_header(target_block)?
        .ok_or_else(|| eyre!("missing proof-history target header {target_block}"))?;
    let target = BlockNumHash::new(target_block, target_header.hash());

    info!(
        target: "reth::taiko::proof_history",
        start_block,
        target_block,
        anchor_hash = ?anchor.hash,
        target_hash = ?target.hash,
        "initializing proof-history storage from historical canonical state"
    );

    let historical_post_state =
        HashedPostStateSorted::from_reverts(&db_provider, start_block.saturating_add(1)..=target_block)
            .wrap_err_with(|| {
                format!(
                    "failed to build reverse changesets for proof-history anchor {start_block} from target {target_block}"
                )
            })?;

    let storage_v2 = db_provider.cached_storage_settings().is_v2();
    let init_job = ProofHistoryInitializationJob::new(storage, db_provider.into_tx());
    if storage_v2 {
        init_job.run_historical_with_adapter::<PackedKeyAdapter>(
            anchor,
            target,
            anchor_header.state_root(),
            historical_post_state,
            metadata_path,
        )?;
    } else {
        init_job.run_historical_with_adapter::<LegacyKeyAdapter>(
            anchor,
            target,
            anchor_header.state_root(),
            historical_post_state,
            metadata_path,
        )?;
    }

    info!(
        target: "reth::taiko::proof_history",
        start_block,
        target_block,
        anchor_hash = ?anchor.hash,
        "historical proof-history storage initialized"
    );

    Ok(())
}

/// Returns the latest finalized block number recorded in the node database.
pub(super) fn finalized_block_number<Provider>(provider: &Provider) -> eyre::Result<Option<u64>>
where
    Provider: DatabaseProviderFactory,
    Provider::Provider: ChainStateBlockReader,
{
    Ok(provider.database_provider_ro()?.last_finalized_block_number()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_optimism_trie::{InMemoryProofsStorage, OpProofsStorage, api::OpProofsProviderRw};

    #[test]
    fn proof_history_storage_initialization_check_tracks_empty_storage() {
        let storage: OpProofsStorage<InMemoryProofsStorage> =
            InMemoryProofsStorage::default().into();

        assert!(proof_history_storage_needs_initialization(&storage).unwrap());

        let provider_rw = storage.provider_rw().expect("provider_rw");
        provider_rw.set_earliest_block_number(0, B256::ZERO).expect("set earliest block");
        provider_rw.commit().expect("commit");

        assert!(!proof_history_storage_needs_initialization(&storage).unwrap());
    }

    #[test]
    fn proof_history_backfill_waits_when_executed_head_has_no_next_parent_state() {
        assert_eq!(proof_history_backfill_target(0, 350_000, 0), None);
    }

    #[test]
    fn proof_history_window_start_saturates_at_genesis() {
        assert_eq!(proof_history_window_start_block(100, 350_000), 0);
    }

    #[test]
    fn proof_history_window_start_subtracts_window() {
        assert_eq!(proof_history_window_start_block(1_000_000, 350_000), 650_000);
    }

    #[test]
    fn delayed_proof_history_initialization_waits_without_finalized_head() {
        assert_eq!(
            delayed_proof_history_start(None, 900_000, 350_000),
            DelayedProofHistoryStart::WaitForFinalized
        );
    }

    #[test]
    fn delayed_proof_history_initialization_waits_until_local_execution_reaches_start() {
        assert_eq!(
            delayed_proof_history_start(Some(1_000_000), 649_999, 350_000),
            DelayedProofHistoryStart::WaitForExecution { start_block: 650_000 }
        );
    }

    #[test]
    fn delayed_proof_history_initialization_reports_missed_start() {
        assert_eq!(
            delayed_proof_history_start(Some(1_000_000), 650_001, 350_000),
            DelayedProofHistoryStart::MissedStart { start_block: 650_000 }
        );
    }

    #[test]
    fn delayed_proof_history_initialization_starts_when_execution_reaches_window_start() {
        assert_eq!(
            delayed_proof_history_start(Some(1_000_000), 650_000, 350_000),
            DelayedProofHistoryStart::Ready { start_block: 650_000 }
        );
    }

    #[test]
    fn historical_init_metadata_round_trips() {
        let metadata = HistoricalInitMetadata {
            start_block: BlockNumHash::new(650_000, B256::with_last_byte(1)),
            target_block: BlockNumHash::new(1_000_000, B256::with_last_byte(2)),
        };

        assert_eq!(HistoricalInitMetadata::decode(&metadata.encode()).unwrap(), metadata);
    }

    #[test]
    fn historical_init_metadata_validation_rejects_target_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PROOF_HISTORY_HISTORICAL_INIT_METADATA_FILE);

        let stored = HistoricalInitMetadata {
            start_block: BlockNumHash::new(650_000, B256::with_last_byte(1)),
            target_block: BlockNumHash::new(1_000_000, B256::with_last_byte(2)),
        };
        let current = HistoricalInitMetadata {
            start_block: stored.start_block,
            target_block: BlockNumHash::new(999_999, B256::with_last_byte(3)),
        };

        write_historical_init_metadata(&path, stored).unwrap();
        let error =
            validate_historical_init_metadata_file(Some(&path), current).unwrap_err().to_string();

        assert!(error.contains("initialization target changed"));
    }
}
