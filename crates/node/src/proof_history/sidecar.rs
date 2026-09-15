//! Canonical-chain adapter for the upstream proof-history engine.

use super::{
    CanonicalSnapshotProvider, ProofHistoryStorage, REBUILD_AT_NEW_PATH,
    config::ProofHistoryConfig,
    fatal_integrity, is_canonical, is_fatal_integrity, opt_block,
    storage_init::{
        BackfillStep, backfill_proof_history_storage, finish_backfill,
        initialize_proof_history_storage, pending_backfill_target,
    },
    store::ProofHistoryDatabase,
};
use alethia_reth_rpc::proof_state::ProofHistoryReadiness;
use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::B256;
use derive_more::Constructor;
use eyre::{WrapErr, eyre};
use reth::tasks::shutdown::GracefulShutdown;
use reth_db::Database;
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_evm::ConfigureEvm;
use reth_optimism_trie::{
    EngineHandle, OpProofStoragePruner, OpProofsBackfillProvider, OpProofsProviderRO,
    OpProofsProviderRw, OpProofsStore,
    engine::{EngineError, persistence::error::PersistenceError},
    proof::DatabaseStateRoot,
};
use reth_primitives_traits::AlloyBlockHeader;
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReader, CanonStateNotification, CanonStateSubscriptions,
    ChainStateBlockReader, ChangeSetReader, DBProvider, DatabaseProviderFactory, HeaderProvider,
    StageCheckpointReader, StateProviderFactory, StateReader, StorageChangeSetReader,
    StorageSettingsCache, TransactionVariant,
};
use reth_trie::StateRoot;
use reth_trie_common::SortedTrieData;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::broadcast, task, time};
use tracing::{debug, error, info, warn};

/// Maximum forward replay work before handling another notification or shutdown.
const REPLAY_BATCH_SIZE: u64 = 32;
/// Allow unavailable state or an outstanding save this long before requesting reconciliation.
const PERSISTENCE_TIMEOUT: Duration = Duration::from_secs(30);
/// Delay between attempts when canonical state has not reached the retained window.
const STARTUP_RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Preserve per-block durability and RPC freshness; explicit replay waits for each commit.
const PERSISTENCE_THRESHOLD: u64 = 1;
/// Bound accepted in-memory work while a persistence transaction is running, in blocks.
const BACKPRESSURE_THRESHOLD: u64 = 10;
/// Consecutive identical wait observations between repeated log lines: about ten minutes at the
/// five-second retry interval, matching the pinned-read reporter cadence.
const WAIT_REPORT_INTERVAL: u32 = 120;
/// Preparation attempts allowed to fail without an integrity error before the sidecar stops with
/// the original cause; five retry delays separate the first and last attempts.
const MAX_PREPARE_FAILURES: u32 = 6;

/// Awaits blocking work while preserving worker panics as critical-task failures.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> eyre::Result<T> + Send + 'static,
) -> eyre::Result<T> {
    match task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(eyre!("proof-history worker failed to join: {error}")),
    }
}

/// Startup reconciliation action for existing proof-history storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProofHistoryStartupAction {
    /// Proof-history storage has no complete retained range and needs the normal initialization
    /// path.
    Uninitialized,
    /// Stored proof-history bounds match canonical state and can be served as-is.
    Ready,
    /// The retained base is non-canonical; rebuild the snapshot with readers paused.
    Rebuild,
    /// Stored proof-history diverges; find the last common block using the retained hash journal.
    ReconcileFork {
        /// Validated fallback anchor when older databases have no hash journal.
        earliest: BlockNumHash,
    },
    /// Canonical chain has not yet reached the stored earliest block; reconciliation must retry
    /// once the chain database catches up (e.g. a chain re-sync that kept proof-history storage).
    WaitForCanonicalEarliest {
        /// Earliest retained proof-history block number missing from the canonical chain.
        earliest: u64,
    },
    /// The canonical chain is below the stored tip; wait for normal catch-up.
    WaitForCanonicalLatest {
        /// Latest retained proof-history block number.
        latest: u64,
        /// Canonical chain height observed at reconciliation time.
        canonical_best: u64,
    },
    /// A hash disappeared at or below the observed head; repeated identical holes must escalate.
    WaitForMissingCanonicalHash {
        /// Retained tip whose canonical hash is unavailable.
        latest: BlockNumHash,
        /// Canonical height observed before reading the missing hash.
        canonical_best: u64,
    },
}

/// Determines how proof-history storage should be reconciled against canonical block hashes.
pub(super) fn proof_history_startup_action(
    earliest: Option<(u64, B256)>,
    latest: Option<(u64, B256)>,
    canonical_best: u64,
    canonical_earliest_hash: Option<B256>,
    canonical_latest_hash: Option<B256>,
) -> eyre::Result<ProofHistoryStartupAction> {
    let (Some((earliest_number, earliest_hash)), Some((latest_number, latest_hash))) =
        (earliest, latest)
    else {
        return Ok(ProofHistoryStartupAction::Uninitialized);
    };

    if earliest_number > latest_number {
        return Err(eyre!(
            "proof-history storage has earliest block {earliest_number} after latest block {latest_number}"
        ));
    }

    // No canonical header at the earliest height yet: the chain database is (re-)syncing and has
    // not reached the retained range. Failing here would crash the node before it could ever sync
    // past this point, so wait instead; a mismatching anchor must be rebuilt.
    let Some(canonical_earliest) = canonical_earliest_hash else {
        return Ok(ProofHistoryStartupAction::WaitForCanonicalEarliest {
            earliest: earliest_number,
        });
    };
    if canonical_earliest != earliest_hash {
        return Ok(ProofHistoryStartupAction::Rebuild);
    }

    // The stored head runs ahead of the canonical chain. This is the normal aftermath of an
    // ungraceful restart: live indexing follows in-memory commits, which outpace the persisted
    // head by up to the engine persistence threshold. Wait for canonical state to catch back up
    // and only then decide between `Ready` and an unwind — discarding the retained window here
    // would trade a few seconds of catch-up for days of re-execution.
    if latest_number > canonical_best {
        return Ok(ProofHistoryStartupAction::WaitForCanonicalLatest {
            latest: latest_number,
            canonical_best,
        });
    }

    let Some(canonical_latest) = canonical_latest_hash else {
        // The best-height and hash reads may straddle a canonical revert. Label this separately
        // so a stationary hole can escalate without mistaking ordinary catch-up for corruption.
        return Ok(ProofHistoryStartupAction::WaitForMissingCanonicalHash {
            latest: BlockNumHash::new(latest_number, latest_hash),
            canonical_best,
        });
    };

    if canonical_latest == latest_hash {
        return Ok(ProofHistoryStartupAction::Ready);
    }

    // The canonical chain reached the stored height with a different block: real divergence
    // (a reorg happened while the sidecar was down). Locate the fork before unwinding.
    Ok(ProofHistoryStartupAction::ReconcileFork {
        earliest: BlockNumHash::new(earliest_number, earliest_hash),
    })
}

/// Why startup cannot proceed yet; canonical waits are logged with throttling by the run loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitReason {
    /// The canonical chain has not reached the retained earliest anchor (e.g. a chain re-sync).
    CanonicalEarliest {
        /// Earliest retained proof-history block number missing from the canonical chain.
        earliest: u64,
    },
    /// The canonical chain is below the retained tip; ordinary catch-up after a restart.
    CanonicalLatest {
        /// Latest retained proof-history block number.
        latest: u64,
        /// Canonical chain height observed at reconciliation time.
        canonical_best: u64,
    },
    /// Finality or local execution has not reached the requested backfill window.
    FinalizedWindow,
    /// A bootstrap prerequisite already reported at its source: pipeline consistency, a changed
    /// snapshot header, or a lagging persisted backfill source.
    Bootstrap,
}

/// Startup either waits for canonical state, makes bounded progress, or starts indexing.
enum StartupStep {
    /// Canonical state or a bootstrap prerequisite is still unavailable; retry after a delay.
    Wait(WaitReason),
    /// Snapshot/backfill/reconciliation progressed; retry without an idle delay.
    Progress,
    /// A canonical hash at or below the observed head is missing; apply the bounded alert policy.
    MissingCanonicalHash {
        /// Retained tip whose canonical hash is unavailable.
        latest: BlockNumHash,
        /// Canonical height paired with this observation.
        canonical_best: u64,
    },
    /// Initialization and canonical reconciliation completed.
    Ready(EngineHandle<Block>),
}

impl ProofHistoryStartupAction {
    /// Maps the actions that cannot make progress to their wait steps. Both `prepare` and the
    /// pre-engine recheck use this single mapping, so neither can silently drop a wait reason.
    const fn unavailable_step(&self) -> Option<StartupStep> {
        match *self {
            Self::WaitForCanonicalEarliest { earliest } => {
                Some(StartupStep::Wait(WaitReason::CanonicalEarliest { earliest }))
            }
            Self::WaitForCanonicalLatest { latest, canonical_best } => {
                Some(StartupStep::Wait(WaitReason::CanonicalLatest { latest, canonical_best }))
            }
            Self::WaitForMissingCanonicalHash { latest, canonical_best } => {
                Some(StartupStep::MissingCanonicalHash { latest, canonical_best })
            }
            Self::Uninitialized | Self::Ready | Self::Rebuild | Self::ReconcileFork { .. } => None,
        }
    }
}

/// Observations of one unavailable retained tip before a non-fatal alert; five retry delays
/// separate the first and sixth observations (normally about 25 seconds plus processing time).
const MISSING_HASH_ALERT_OBSERVATIONS: u32 = 6;

/// Identity of one repeated startup wait, ignoring fields that legitimately change every poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitIdentity {
    /// The retained tip whose canonical hash is unavailable.
    MissingHash(BlockNumHash),
    /// The earliest anchor the canonical chain has not reached.
    CanonicalEarliest(u64),
    /// The retained tip the canonical chain has not reached; the observed head moves every block.
    CanonicalLatest(u64),
    /// Finality or execution prerequisites of the requested backfill window.
    FinalizedWindow,
}

/// Logs a repeated wait on its first observation and then every `WAIT_REPORT_INTERVAL`.
const fn wait_report_due(consecutive: u32) -> bool {
    consecutive == 1 || consecutive.is_multiple_of(WAIT_REPORT_INTERVAL)
}

/// Tracks repeated startup waits so each episode logs on entry and then about every ten minutes,
/// and a persistently missing canonical hash raises an alert without stopping the node.
#[derive(Debug, Default)]
struct StartupObservations {
    /// Identity of the preceding observation while startup keeps waiting for the same reason.
    previous: Option<WaitIdentity>,
    /// Consecutive observations of that identity; saturates without terminating the node.
    consecutive: u32,
}

impl StartupObservations {
    /// Records a startup step: waits log on transition and periodically, the missing-hash gauge
    /// latches after `MISSING_HASH_ALERT_OBSERVATIONS`, and any other step clears the episode.
    fn observe(&mut self, step: &StartupStep) {
        let gauge = metrics::gauge!("taiko_proof_history_missing_canonical_hash");
        let (identity, best) = match step {
            StartupStep::MissingCanonicalHash { latest, canonical_best } => {
                (WaitIdentity::MissingHash(*latest), *canonical_best)
            }
            StartupStep::Wait(WaitReason::CanonicalEarliest { earliest }) => {
                (WaitIdentity::CanonicalEarliest(*earliest), 0)
            }
            StartupStep::Wait(WaitReason::CanonicalLatest { latest, canonical_best }) => {
                (WaitIdentity::CanonicalLatest(*latest), *canonical_best)
            }
            StartupStep::Wait(WaitReason::FinalizedWindow) => (WaitIdentity::FinalizedWindow, 0),
            StartupStep::Wait(WaitReason::Bootstrap) |
            StartupStep::Progress |
            StartupStep::Ready(_) => {
                *self = Self::default();
                gauge.set(0.0);
                return;
            }
        };
        self.consecutive =
            if self.previous == Some(identity) { self.consecutive.saturating_add(1) } else { 1 };
        self.previous = Some(identity);
        let consecutive = self.consecutive;
        let report = wait_report_due(consecutive);
        match identity {
            WaitIdentity::MissingHash(latest) => {
                let alerted = consecutive >= MISSING_HASH_ALERT_OBSERVATIONS;
                gauge.set(if alerted { 1.0 } else { 0.0 });
                if consecutive == 1 {
                    warn!(target: "reth::taiko::proof_history", latest = latest.number,
                        hash = ?latest.hash, canonical_best = best,
                        "canonical hash missing; historical reads remain paused while retrying");
                } else if consecutive == MISSING_HASH_ALERT_OBSERVATIONS || (alerted && report) {
                    error!(target: "reth::taiko::proof_history", latest = latest.number,
                        hash = ?latest.hash, canonical_best = best, consecutive,
                        "canonical hash remains unavailable; check node synchronization and \
                         headers; retrying without stopping the node");
                }
            }
            WaitIdentity::CanonicalEarliest(earliest) => {
                gauge.set(0.0);
                if report {
                    info!(target: "reth::taiko::proof_history", earliest, consecutive,
                        "waiting for the canonical chain to reach the proof-history anchor");
                }
            }
            WaitIdentity::CanonicalLatest(latest) => {
                gauge.set(0.0);
                if report {
                    warn!(target: "reth::taiko::proof_history", latest, canonical_best = best,
                        consecutive,
                        "waiting for canonical proof-history head; historical reads are paused");
                }
            }
            WaitIdentity::FinalizedWindow => {
                gauge.set(0.0);
                if report {
                    info!(target: "reth::taiko::proof_history", consecutive,
                        "waiting for finality and local execution to reach the backfill window");
                }
            }
        }
    }
}

/// Engine failures caused by a stalled or disconnected persistence thread rather than by block
/// data; dropping the engine and reconciling replaces that thread, so they are not fatal.
const fn engine_stalled(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::PersistenceTimeout |
            EngineError::PersistenceDisconnected |
            EngineError::Persistence(PersistenceError::Disconnected)
    )
}

/// Owns Taiko lifecycle policy while upstream owns indexing, persistence and pruning.
#[derive(Debug, Constructor)]
pub(super) struct ProofHistorySidecar<Evm, Provider> {
    /// Canonical provider supplying notifications and blocks for catch-up.
    provider: Provider,
    /// Taiko EVM configuration used by upstream replay.
    evm_config: Evm,
    /// Metrics-wrapped storage used by live indexing and RPC.
    storage: ProofHistoryStorage,
    /// Unwrapped storage used for bulk initialization and backward backfill.
    init_storage: Arc<ProofHistoryDatabase>,
    /// Retention, verification and maintenance settings.
    config: ProofHistoryConfig,
    /// Read permission published only after canonical reconciliation.
    readiness: ProofHistoryReadiness,
}

/// Returns whether a committed chain starting at `first_block` leaves no gap above the stored
/// proof-history head, so its blocks can be consumed directly from the notification.
const fn committed_chain_is_contiguous(first_block: u64, latest_stored: u64) -> bool {
    first_block <= latest_stored.saturating_add(1)
}

impl<Evm, Provider> ProofHistorySidecar<Evm, Provider>
where
    Evm: ConfigureEvm<Primitives = EthPrimitives> + 'static,
    Provider: BlockHashReader
        + BlockNumReader
        + BlockReader<Block = Block>
        + DatabaseProviderFactory
        + StateReader
        + StateProviderFactory
        + Clone
        + Send
        + Sync
        + CanonicalSnapshotProvider
        + 'static,
    Provider::Provider: BlockNumReader
        + HeaderProvider
        + DBProvider
        + ChainStateBlockReader
        + ChangeSetReader
        + StorageChangeSetReader
        + StorageSettingsCache
        + StageCheckpointReader
        + Sync,
    <Provider::DB as Database>::TX: Sync,
{
    /// Runs until shutdown, keeping all engine operations and final thread joins off Tokio.
    pub(super) async fn run(self, shutdown: GracefulShutdown) -> eyre::Result<()>
    where
        Provider: CanonStateSubscriptions<Primitives = EthPrimitives>,
    {
        if self.config.prune_interval.is_zero() {
            return Err(eyre!("proof-history maintenance interval must be greater than zero"));
        }
        metrics::describe_gauge!(
            "taiko_proof_history_ready",
            "1 while historical proof reads are served; 0 while the sidecar waits, reconciles \
             or is stopped"
        );
        metrics::describe_gauge!(
            "taiko_proof_history_missing_canonical_hash",
            "1 after the sixth consecutive observation of a retained tip whose canonical hash is \
             unavailable at or below the observed head"
        );
        metrics::describe_gauge!(
            "taiko_proof_history_pin_active",
            "1 while a pinned node read transaction (initial copy or backward chunk) is held"
        );
        metrics::describe_gauge!(
            "taiko_proof_history_pin_elapsed_seconds",
            metrics::Unit::Seconds,
            "duration of the current pinned read phase, retaining the last completed duration"
        );
        let this = Arc::new(self);
        let cleanup_guard = shutdown.clone();
        let mut monitor = shutdown.clone();
        let result = {
            let running = Self::run_loop(&this, shutdown, STARTUP_RETRY_INTERVAL);
            tokio::pin!(running);
            tokio::select! {
                biased;
                guard = &mut monitor => {
                    this.init_storage.cancel_bootstrap();
                    // The worker observes cancellation at a write-batch boundary and then joins.
                    let result = running.await;
                    drop(guard);
                    result
                }
                result = &mut running => result,
            }
        };
        this.publish_readiness(false);
        // Cleanup that cannot join (a runtime already shutting down) must not replace the
        // sidecar's own outcome: a clean shutdown stays clean and a real error keeps its guidance.
        if let Err(error) = blocking(move || {
            drop(this);
            Ok(())
        })
        .await
        {
            warn!(target: "reth::taiko::proof_history", error = %format_args!("{error:#}"),
                "proof-history cleanup did not join; reporting the sidecar's own outcome");
        }
        drop(cleanup_guard);
        result
    }

    /// Preserves a pinned copy through reorgs, but rechecks any engine prepared while they arrive.
    /// Closing the notification source cancels work; shutdown cancellation is owned by `run`.
    async fn await_preparation(
        &self,
        preparing: impl std::future::Future<Output = eyre::Result<StartupStep>>,
        notifications: &mut broadcast::Receiver<CanonStateNotification<EthPrimitives>>,
    ) -> (eyre::Result<StartupStep>, bool) {
        tokio::pin!(preparing);
        let mut listen = true;
        let mut reconcile = false;
        let result = loop {
            tokio::select! {
                result = &mut preparing => break result,
                notification = notifications.recv(), if listen => {
                    match notification {
                        Ok(CanonStateNotification::Commit { .. }) => {}
                        Err(broadcast::error::RecvError::Closed) => {
                            listen = false;
                            self.init_storage.cancel_bootstrap();
                        }
                        Ok(CanonStateNotification::Reorg { .. }) |
                        Err(broadcast::error::RecvError::Lagged(_)) => reconcile = true,
                    }
                }
            }
        };
        let result = match result {
            Ok(StartupStep::Ready(handle)) if reconcile => {
                blocking(move || {
                    drop(handle);
                    Ok(StartupStep::Progress)
                })
                .await
            }
            result => result,
        };
        (result, listen)
    }

    /// Drives initialization and canonical updates while the caller owns final resource cleanup.
    /// Preparation errors that do not prove an integrity failure are retried a bounded number of
    /// times before the original error stops the sidecar.
    async fn run_loop(
        this: &Arc<Self>,
        mut shutdown: GracefulShutdown,
        retry_interval: Duration,
    ) -> eyre::Result<()>
    where
        Provider: CanonStateSubscriptions<Primitives = EthPrimitives>,
    {
        let mut notifications = this.provider.subscribe_to_canonical_state();
        let mut engine = None;
        let mut retry = Duration::ZERO;
        let mut observations = StartupObservations::default();
        let mut prepare_failures = 0u32;
        let mut interval = time::interval(this.config.prune_interval);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        loop {
            let notification = tokio::select! {
                biased;
                guard = &mut shutdown => {
                    this.publish_readiness(false);
                    blocking(move || { drop(engine); Ok(()) }).await?;
                    drop(guard);
                    return Ok(());
                }
                _ = time::sleep(retry), if engine.is_none() => {
                    this.init_storage.resume_bootstrap();
                    let worker = Arc::clone(this);
                    let preparing = blocking(move || worker.prepare());
                    let (result, listen) = this
                        .await_preparation(preparing, &mut notifications)
                        .await;
                    if this.init_storage.bootstrap_cancelled() {
                        // Preparation may have returned an engine just as cancellation arrived.
                        blocking(move || { drop(result); Ok(()) }).await?;
                        if !listen { return Ok(()); }
                        retry = Duration::ZERO;
                        continue;
                    }
                    let step = match result {
                        Ok(step) => {
                            prepare_failures = 0;
                            step
                        }
                        Err(error) => {
                            let attempt = prepare_failures.saturating_add(1);
                            if is_fatal_integrity(&error) {
                                return Err(error);
                            }
                            if attempt >= MAX_PREPARE_FAILURES {
                                return Err(error).wrap_err_with(|| format!(
                                    "proof-history preparation stopped after {attempt} failed \
                                     preparation attempts"
                                ));
                            }
                            prepare_failures = attempt;
                            error!(target: "reth::taiko::proof_history", attempt,
                                error = %format_args!("{error:#}"),
                                "proof-history preparation failed; retrying");
                            retry = retry_interval;
                            continue;
                        }
                    };
                    observations.observe(&step);
                    match step {
                        StartupStep::MissingCanonicalHash { .. } |
                        StartupStep::Wait(_) => retry = retry_interval,
                        StartupStep::Progress => retry = Duration::ZERO,
                        StartupStep::Ready(handle) => {
                            engine = Some(handle);
                            this.publish_readiness(true);
                        }
                    }
                    continue;
                }
                notification = notifications.recv(), if engine.is_some() => {
                    match notification {
                        Ok(notification) => Some(notification),
                        Err(error) => {
                            this.publish_readiness(false);
                            let stopped = engine.take();
                            blocking(move || { drop(stopped); Ok(()) }).await?;
                            if matches!(error, broadcast::error::RecvError::Closed) {
                                return Ok(());
                            }
                            warn!(target: "reth::taiko::proof_history", ?error,
                                "canonical notifications lagged; reconciling proof history");
                            notifications = this.provider.subscribe_to_canonical_state();
                            retry = Duration::ZERO;
                            continue;
                        }
                    }
                }
                _ = interval.tick(), if engine.is_some() => None,
            };

            if matches!(notification, Some(CanonStateNotification::Reorg { .. })) {
                this.publish_readiness(false);
            }
            let handle = engine.take().expect("running engine selected");
            let worker = Arc::clone(this);
            let (next_engine, more_replay) = blocking(move || {
                let result = (|| -> eyre::Result<(bool, bool)> {
                    let valid = match notification {
                        Some(notification) => worker.handle_notification(&handle, &notification)?,
                        None => {
                            matches!(worker.startup_action()?, ProofHistoryStartupAction::Ready)
                        }
                    };
                    let (valid, more) =
                        if valid { worker.replay_available(&handle)? } else { (false, false) };
                    if valid {
                        worker.retain_window()?;
                    }
                    Ok((valid, more))
                })();
                if !matches!(result, Ok((true, _))) {
                    worker.publish_readiness(false);
                    // Drop the only handle here: no engine writer may survive reconciliation.
                    drop(handle);
                    return result.map(|_| (None, false));
                }
                Ok((Some(handle), result?.1))
            })
            .await?;
            engine = next_engine;
            if more_replay {
                interval.reset_immediately();
            }
            if engine.is_some() {
                this.publish_readiness(true);
            } else {
                notifications = this.provider.subscribe_to_canonical_state();
                retry = Duration::ZERO;
            }
        }
    }

    /// Reconciles storage and performs a cancellable snapshot or backward backfill job.
    fn prepare(&self) -> eyre::Result<StartupStep> {
        let target_path = self.config.required_storage_path()?.join("backfill-target");
        let pending_target = pending_backfill_target(&target_path)?;
        let action = self.startup_action()?;
        if let Some(step) = action.unavailable_step() {
            return Ok(step);
        }
        match action {
            ProofHistoryStartupAction::Rebuild => {
                let window = self.storage.provider_ro()?.get_proof_window()?;
                let canonical = self.provider.block_hash(window.earliest.number)?;
                warn!(target: "reth::taiko::proof_history", earliest = window.earliest.number,
                    retained = ?window.earliest.hash, ?canonical, latest = window.latest.number,
                    "retained earliest anchor is no longer canonical; discarding the entire \
                     proof-history window and hash journal to rebuild from a fresh snapshot");
                self.init_storage.reset_bootstrap()?;
                return Ok(StartupStep::Progress);
            }
            ProofHistoryStartupAction::Uninitialized => {
                if self.config.backfill_window_only {
                    let db = self.provider.database_provider_ro()?;
                    let Some(finalized) = db.last_finalized_block_number()? else {
                        return Ok(StartupStep::Wait(WaitReason::FinalizedWindow));
                    };
                    if db.best_block_number()? < finalized.saturating_sub(self.config.window) {
                        return Ok(StartupStep::Wait(WaitReason::FinalizedWindow));
                    }
                }
                // An unfinished copy can restart at a newer source head; replace its target too.
                self.init_storage.reset_bootstrap()?;
                finish_backfill(&target_path)?;
                let initialized = initialize_proof_history_storage(
                    &self.provider,
                    self.init_storage.clone(),
                    Some((
                        target_path.as_path(),
                        if self.config.backfill_window_only { self.config.window } else { 0 },
                    )),
                )?;
                return Ok(if initialized {
                    StartupStep::Progress
                } else {
                    StartupStep::Wait(WaitReason::Bootstrap)
                });
            }
            ProofHistoryStartupAction::WaitForCanonicalEarliest { .. } |
            ProofHistoryStartupAction::WaitForCanonicalLatest { .. } |
            ProofHistoryStartupAction::WaitForMissingCanonicalHash { .. } => {
                unreachable!("startup waits are mapped by unavailable_step")
            }
            ProofHistoryStartupAction::ReconcileFork { earliest } => {
                let latest = self.storage.provider_ro()?.get_latest_block()?;
                let mut fork = earliest;
                // Only a hash-identified common block proves the entire preceding chain matches.
                if self.init_storage.indexed_hash(latest.number)? == Some(latest.hash) {
                    for number in (earliest.number..latest.number).rev() {
                        let Some(hash) = self.init_storage.indexed_hash(number)? else { break };
                        if is_canonical(&self.provider, BlockNumHash::new(number, hash))? {
                            fork = BlockNumHash::new(number, hash);
                            break;
                        }
                    }
                }
                warn!(target: "reth::taiko::proof_history", from = latest.number, to = fork.number,
                    "unwinding divergent proof history to its last known common block");
                let first = fork
                    .number
                    .checked_add(1)
                    .ok_or_else(|| eyre!("cannot unwind beyond u64::MAX"))?;
                let rw = self.storage.provider_rw()?;
                rw.unwind_history(BlockWithParent::new(
                    fork.hash,
                    BlockNumHash::new(first, B256::ZERO),
                ))?;
                rw.commit()?;
                self.init_storage.retain_hashes(earliest.number, fork.number)?;
                return Ok(StartupStep::Progress);
            }
            ProofHistoryStartupAction::Ready => {}
        }
        let window = self.storage.provider_ro()?.get_proof_window()?;
        let header = self.provider.sealed_header(window.latest.number)?;
        if !self.validate_snapshot_header(window.latest, header)? {
            return Ok(StartupStep::Wait(WaitReason::Bootstrap));
        }
        if let Some(target) = pending_target &&
            window.earliest.number > target
        {
            let progressed = backfill_proof_history_storage(
                &self.provider,
                self.init_storage.clone(),
                target,
            )
            .wrap_err_with(|| {
                format!(
                    "proof-history bootstrap toward block {target} failed: {REBUILD_AT_NEW_PATH}. \
                     To abandon only older backfill for an otherwise healthy retained window, \
                     remove {} while stopped and restart with shorter coverage; canonical/root \
                     validation and replay prerequisites still apply",
                    target_path.display()
                )
            })?;
            return Ok(match progressed {
                BackfillStep::Progressed | BackfillStep::Reconcile => StartupStep::Progress,
                BackfillStep::Wait => StartupStep::Wait(WaitReason::Bootstrap),
            });
        }
        // No backward work remains, including an operator-abandoned target. Only the auxiliary
        // cache is cleared here, after retained canonical/root validation has succeeded.
        let rw = self.init_storage.provider_rw()?;
        rw.clear_snapshot()?;
        OpProofsProviderRw::commit(rw)?;
        if pending_target.is_some() {
            finish_backfill(&target_path)?;
        }
        let to_prune = window
            .latest
            .number
            .saturating_sub(self.config.window)
            .saturating_sub(window.earliest.number);
        if to_prune > self.config.max_startup_prune_blocks {
            return Err(eyre!(
                "configuration requires pruning {to_prune} proof-history blocks, exceeding {}; \
                 raise --proofs-history.max-startup-prune-blocks or restore --proofs-history.window",
                self.config.max_startup_prune_blocks
            ));
        }
        let pruner = OpProofStoragePruner::new(
            self.storage.clone(),
            self.provider.clone(),
            self.config.window,
        );
        // Prune startup excess before the engine exists; all subsequent pruning is in its save txn.
        let rw = self.storage.provider_rw()?;
        pruner.prune_with_provider(&rw)?;
        rw.commit()?;
        self.retain_window()?;
        self.start_engine(pruner)
    }

    /// Rechecks canonical state after pruning and before creating any engine threads.
    fn start_engine(
        &self,
        pruner: OpProofStoragePruner<ProofHistoryStorage, Provider>,
    ) -> eyre::Result<StartupStep> {
        let action = self.startup_action()?;
        if let Some(step) = action.unavailable_step() {
            return Ok(step);
        }
        match action {
            ProofHistoryStartupAction::Ready => {}
            ProofHistoryStartupAction::Uninitialized |
            ProofHistoryStartupAction::Rebuild |
            ProofHistoryStartupAction::ReconcileFork { .. } => return Ok(StartupStep::Progress),
            ProofHistoryStartupAction::WaitForCanonicalEarliest { .. } |
            ProofHistoryStartupAction::WaitForCanonicalLatest { .. } |
            ProofHistoryStartupAction::WaitForMissingCanonicalHash { .. } => {
                unreachable!("startup waits are mapped by unavailable_step")
            }
        }
        Ok(StartupStep::Ready(EngineHandle::spawn_with_thresholds(
            self.evm_config.clone(),
            self.provider.clone(),
            self.storage.clone(),
            pruner,
            PERSISTENCE_THRESHOLD,
            BACKPRESSURE_THRESHOLD,
        )))
    }

    /// Validates a retained root against a fetched header before publishing historical reads.
    /// Returns false if the header disappeared or changed; inconsistent stored roots fail loudly.
    fn validate_snapshot_header(
        &self,
        latest: BlockNumHash,
        header: Option<reth_primitives_traits::SealedHeader<Provider::Header>>,
    ) -> eyre::Result<bool> {
        let Some(header) = header.filter(|header| header.hash() == latest.hash) else {
            debug!(target: "reth::taiko::proof_history", block = latest.number,
                "snapshot header changed during validation; retrying reconciliation");
            return Ok(false);
        };
        let root = StateRoot::overlay_root(
            self.storage.provider_ro()?,
            latest.number,
            Default::default(),
        )?;
        if root != header.state_root() {
            return Err(fatal_integrity(format!(
                "proof-history state root mismatch at block {} ({:?}): computed {:?}, expected {:?}; \
                 inspect the node's source state and EVM configuration, then {REBUILD_AT_NEW_PATH}",
                latest.number,
                latest.hash,
                root,
                header.state_root()
            )));
        }
        Ok(true)
    }

    /// Replays available canonical blocks, including the in-memory tail of missed notifications.
    /// Returns (canonical branch still valid, more work), keeping cancellation distinct from idle.
    fn replay_available(&self, engine: &EngineHandle<Block>) -> eyre::Result<(bool, bool)> {
        let tip = self.storage.provider_ro()?.get_latest_block()?;
        let best = self.provider.best_block_number()?;
        if tip.number >= best {
            return Ok((true, false));
        }
        let end = tip.number.saturating_add(REPLAY_BATCH_SIZE).min(best);
        let mut parent = tip.hash;
        let mut blocks = Vec::new();
        for number in tip.number + 1..=end {
            if self.init_storage.bootstrap_cancelled() {
                return Ok((false, false));
            }
            let Some(block) =
                self.provider.recovered_block(number.into(), TransactionVariant::NoHash)?
            else {
                return Ok((false, false));
            };
            if !is_canonical(&self.provider, BlockNumHash::new(number, block.hash()))? ||
                block.parent_hash() != parent
            {
                return Ok((false, false));
            }
            parent = block.hash();
            blocks.push(block);
        }
        // Hashes are write-ahead metadata: one bounded batch is safe, while submissions must
        // still wait individually because upstream may successfully skip unavailable parents.
        self.init_storage
            .record_hashes(blocks.iter().map(|block| (block.number(), block.hash())))?;
        for block in blocks {
            if !self.persist_block(BlockNumHash::new(block.number(), block.hash()), || {
                engine.execute_block(&block)?;
                Ok(())
            })? {
                return Ok((false, false));
            }
        }
        Ok((true, end < self.provider.best_block_number()?))
    }

    /// Submits idempotent contiguous work after write-ahead journaling and waits for durability.
    /// Retries successful upstream no-ops (temporarily unavailable parent state); explicit errors
    /// propagate. Canonical changes, cancellation or prolonged stalls request reconciliation.
    fn persist_block(
        &self,
        expected: BlockNumHash,
        submit: impl FnMut() -> eyre::Result<()>,
    ) -> eyre::Result<bool> {
        self.persist_block_until(expected, Instant::now() + PERSISTENCE_TIMEOUT, submit)
    }

    /// Waits for one accepted block until `deadline`; timeout requests canonical reconciliation.
    fn persist_block_until(
        &self,
        expected: BlockNumHash,
        deadline: Instant,
        mut submit: impl FnMut() -> eyre::Result<()>,
    ) -> eyre::Result<bool> {
        let mut retry_at = Instant::now();
        loop {
            if !is_canonical(&self.provider, expected)? {
                return Ok(false);
            }
            if self.init_storage.bootstrap_cancelled() {
                return Ok(false);
            }
            let latest = self.storage.provider_ro()?.get_latest_block()?;
            if latest == expected {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                warn!(target: "reth::taiko::proof_history", block = expected.number, stored = latest.number,
                    "proof-history made no durable progress; reconciling before retry");
                return Ok(false);
            }
            if Instant::now() >= retry_at {
                if let Err(error) = submit() {
                    if !is_canonical(&self.provider, expected)? {
                        return Ok(false);
                    }
                    match error.downcast_ref::<EngineError>() {
                        Some(engine_error) if engine_stalled(engine_error) => {
                            warn!(target: "reth::taiko::proof_history", block = expected.number,
                                error = %engine_error,
                                "proof-history engine persistence stalled; reconciling with a \
                                 fresh engine");
                            return Ok(false);
                        }
                        Some(EngineError::EngineDied) => {
                            return Err(error).wrap_err_with(|| format!(
                                "proof-history engine thread terminated while indexing block {} \
                                 ({:?}); its panic was logged by the engine. Restart the node and, \
                                 if it recurs, preserve the logs and the proof-history directory \
                                 for diagnosis",
                                expected.number, expected.hash
                            ));
                        }
                        _ => {}
                    }
                    return Err(error).wrap_err_with(|| {
                        format!(
                            "proof-history failed to index block {} ({:?}); check retained node \
                         bodies/state and EVM consistency. Restore required pruned history before \
                         replay; then {REBUILD_AT_NEW_PATH}",
                            expected.number, expected.hash
                        )
                    });
                }
                retry_at = Instant::now() + Duration::from_millis(100);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Trims the hash journal to the retained proof window after any change to its bounds.
    fn retain_window(&self) -> eyre::Result<()> {
        let window = self.storage.provider_ro()?.get_proof_window()?;
        self.init_storage.retain_hashes(window.earliest.number, window.latest.number)
    }

    /// Publishes read permission and mirrors it in `taiko_proof_history_ready` for alerting.
    fn publish_readiness(&self, ready: bool) {
        if ready {
            self.readiness.set_ready();
        } else {
            self.readiness.set_not_ready();
        }
        metrics::gauge!("taiko_proof_history_ready").set(if ready { 1.0 } else { 0.0 });
    }

    /// Checks both persisted window anchors against one consistent canonical view.
    fn startup_action(&self) -> eyre::Result<ProofHistoryStartupAction> {
        let ro = self.storage.provider_ro()?;
        let earliest = opt_block(ro.get_earliest_block())?;
        let latest = opt_block(ro.get_latest_block())?;
        // One snapshot serves every hash read, and the executed head is bounded by that
        // snapshot's in-memory head: reth updates the in-memory chain before its head tracker, so
        // the tracker alone can briefly exceed the blocks whose hashes are visible.
        let snapshot = self.provider.canonical_snapshot()?;
        let best = self.provider.best_block_number()?.min(snapshot.best_block_number()?);
        if let (Some((earliest_number, earliest_hash)), Some((latest_number, latest_hash))) =
            (earliest, latest) &&
            latest_number > best &&
            self.init_storage.indexed_hash(latest_number)? == Some(latest_hash) &&
            let (Some(indexed), Some(canonical)) =
                (self.init_storage.indexed_hash(best)?, snapshot.block_hash(best)?) &&
            indexed != canonical &&
            is_canonical(&snapshot, BlockNumHash::new(earliest_number, earliest_hash))?
        {
            return Ok(ProofHistoryStartupAction::ReconcileFork {
                earliest: BlockNumHash::new(earliest_number, earliest_hash),
            });
        }
        proof_history_startup_action(
            earliest,
            latest,
            best,
            earliest.map(|(n, _)| snapshot.block_hash(n)).transpose()?.flatten(),
            latest
                .filter(|(n, _)| *n <= best)
                .map(|(n, _)| snapshot.block_hash(n))
                .transpose()?
                .flatten(),
        )
    }

    /// Applies a current notification, returning false when its branch needs reconciliation.
    fn handle_notification(
        &self,
        engine: &EngineHandle<Block>,
        notification: &CanonStateNotification<EthPrimitives>,
    ) -> eyre::Result<bool> {
        let new = match notification {
            CanonStateNotification::Commit { new } => new,
            CanonStateNotification::Reorg { old, new } => {
                let target = if new.is_empty() {
                    old.fork_block()
                } else {
                    BlockNumHash::new(new.tip().number, new.tip().hash())
                };
                if !is_canonical(&self.provider, target)? ||
                    (new.is_empty() && self.provider.best_block_number()? != target.number)
                {
                    return Ok(false);
                }
                let earliest = self.storage.provider_ro()?.get_earliest_block()?;
                if old.first().number() <= earliest.number {
                    return Ok(false);
                }
                if !new.is_empty() && old.fork_block() != new.fork_block() {
                    // Ancestor FCUs may include the ancestor as `new`; reconcile that shape
                    // against the journal instead of assuming two equal-height replacement forks.
                    return Ok(false);
                }
                // The engine's buffered tip may exceed the persisted tip: always forward unwind.
                if let Err(error) = engine.unwind(old.first().block_with_parent()) {
                    if engine_stalled(&error) {
                        warn!(target: "reth::taiko::proof_history", block = old.first().number(),
                            %error,
                            "proof-history engine persistence stalled during unwind; reconciling \
                             with a fresh engine");
                        return Ok(false);
                    }
                    return Err(error.into());
                }
                self.retain_window()?;
                if new.is_empty() {
                    return Ok(true);
                }
                new
            }
        };
        if !is_canonical(&self.provider, BlockNumHash::new(new.tip().number, new.tip().hash()))? {
            return Ok(false);
        }
        let latest = self.storage.provider_ro()?.get_latest_block()?.number;
        if !committed_chain_is_contiguous(new.first().number(), latest) {
            // Submitting a gap would make upstream replay up to an unpersisted notification tip.
            // The caller explicitly replays the missing canonical prefix first.
            return Ok(true);
        }
        if latest < new.tip().number {
            self.init_storage.record_hashes(
                new.blocks()
                    .iter()
                    .filter(|(number, _)| **number > latest)
                    .map(|(number, block)| (*number, block.hash())),
            )?;
        }
        for (number, block) in new.blocks() {
            let tip = self.storage.provider_ro()?.get_latest_block()?;
            if *number <= tip.number {
                continue;
            }
            if *number != tip.number.saturating_add(1) {
                break;
            }
            if block.parent_hash() != tip.hash {
                return Ok(false);
            }
            let verify = self.config.verification_interval > 0 &&
                number.is_multiple_of(self.config.verification_interval);
            if !self.persist_block(BlockNumHash::new(*number, block.hash()), || {
                if !verify && let Some(data) = new.trie_data_at(*number) {
                    let SortedTrieData { hashed_state, trie_updates } = &data.get().sorted;
                    engine.index_block(
                        block.block_with_parent(),
                        (**trie_updates).clone(),
                        (**hashed_state).clone(),
                    )?;
                } else {
                    engine.execute_block(block)?;
                }
                Ok(())
            })? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProofHistoryDatabase, ProofHistorySidecar, ProofHistoryStartupAction, StartupStep,
        WaitReason, committed_chain_is_contiguous, proof_history_startup_action,
    };
    use crate::proof_history::{
        ProofHistoryConfig, storage_init::initialize_proof_history_storage,
    };
    use alethia_reth_rpc::proof_state::ProofHistoryReadiness;
    use alloy_eips::BlockNumHash;
    use alloy_primitives::B256;
    use reth_chain_state::{ExecutedBlock, NewCanonicalChain};
    use reth_chainspec::{ChainSpec, ChainSpecBuilder};
    use reth_db_common::init::init_genesis;
    use reth_ethereum_primitives::{Block, BlockBody};
    use reth_evm_ethereum::EthEvmConfig;
    use reth_optimism_trie::{OpProofsInitProvider, OpProofsProviderRO, OpProofsStore};
    use reth_primitives_traits::Block as _;
    use reth_provider::{
        providers::BlockchainProvider,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory_with_chain_spec},
    };
    use std::sync::Arc;

    fn sidecar_fixture() -> (
        ProofHistorySidecar<EthEvmConfig, BlockchainProvider<MockNodeTypesWithDB>>,
        Arc<ChainSpec>,
        tempfile::TempDir,
    ) {
        let spec = Arc::new(ChainSpecBuilder::mainnet().paris_activated().build());
        let factory = create_test_provider_factory_with_chain_spec(spec.clone());
        init_genesis(&factory).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        assert!(initialize_proof_history_storage(&factory, storage.clone(), None).unwrap());
        let sidecar = ProofHistorySidecar::new(
            BlockchainProvider::new(factory).unwrap(),
            EthEvmConfig::ethereum(spec.clone()),
            storage.clone().into(),
            storage,
            ProofHistoryConfig {
                storage_path: Some(dir.path().to_path_buf()),
                ..ProofHistoryConfig::disabled()
            },
            ProofHistoryReadiness::new(),
        );
        (sidecar, spec, dir)
    }

    #[test]
    fn prepare_rejects_a_completed_snapshot_with_an_invalid_root() {
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        let invalid_dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(invalid_dir.path()).unwrap());
        let init = storage.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(0, spec.genesis_hash())).unwrap();
        init.commit_initial_state().unwrap();
        init.commit().unwrap();
        sidecar.storage = storage.clone().into();
        sidecar.init_storage = storage;
        assert!(sidecar.prepare().is_err(), "a completed marker cannot bypass root verification");
    }

    #[test]
    fn prepare_restarts_a_pending_bootstrap_after_its_anchor_reorgs() {
        let (mut sidecar, _spec, _dir) = sidecar_fixture();
        let pending = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(pending.path()).unwrap());
        let init = storage.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(0, B256::repeat_byte(7))).unwrap();
        init.commit_initial_state().unwrap();
        init.commit().unwrap();
        std::fs::write(pending.path().join("backfill-target"), "0").unwrap();
        sidecar.storage = storage.clone().into();
        sidecar.init_storage = storage;
        sidecar.config.storage_path = Some(pending.path().to_path_buf());

        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        assert!(sidecar.storage.provider_ro().unwrap().get_earliest_block().is_err());
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Ready(_)));
        assert!(!pending.path().join("backfill-target").exists());
    }

    #[test]
    fn prepare_waits_for_finalized_window_before_snapshot() {
        let (mut sidecar, _spec, _dir) = sidecar_fixture();
        let empty = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(empty.path()).unwrap());
        sidecar.storage = storage.clone().into();
        sidecar.init_storage = storage;
        sidecar.config.storage_path = Some(empty.path().to_path_buf());
        sidecar.config.backfill_window_only = true;
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Wait(_)));
    }

    fn executed(number: u64, parent_hash: B256, state_root: B256, fork: u8) -> ExecutedBlock {
        let block = Block {
            header: alloy_consensus::Header {
                number,
                parent_hash,
                state_root,
                extra_data: vec![fork].into(),
                ..Default::default()
            },
            body: BlockBody::default(),
        }
        .try_into_recovered()
        .unwrap();
        ExecutedBlock { recovered_block: Arc::new(block), ..Default::default() }
    }

    #[test]
    fn reorg_replaces_the_persisted_tip() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let old = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let state = sidecar.provider.canonical_in_memory_state();
        let commit = NewCanonicalChain::Commit { new: vec![old.clone()] };
        let notification = commit.to_chain_notification();
        state.update_chain(commit);
        state.set_canonical_head(old.recovered_block.clone_sealed_header());
        assert!(sidecar.handle_notification(&engine, &notification).unwrap());
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().number, 1);

        let new = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 2);
        let update = NewCanonicalChain::Reorg { old: vec![old], new: vec![new.clone()] };
        let notification = update.to_chain_notification();
        state.update_chain(update);
        state.set_canonical_head(new.recovered_block.clone_sealed_header());
        assert!(sidecar.handle_notification(&engine, &notification).unwrap());
        // A following block must accept the replacement's hash as parent, proving that the
        // previous branch was actually removed before accepting its replacement.
        engine
            .index_block(
                alloy_eips::eip1898::BlockWithParent::new(
                    new.recovered_block.hash(),
                    BlockNumHash::new(2, hash(9)),
                ),
                Default::default(),
                Default::default(),
            )
            .unwrap();
        drop(engine);
    }

    #[test]
    fn stale_reorg_is_reconciled_before_touching_the_engine() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let old = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let stale = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 2);
        let current = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 3);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![current.clone()] });
        state.set_canonical_head(current.recovered_block.clone_sealed_header());
        let stale_update = NewCanonicalChain::Reorg { old: vec![old], new: vec![stale] };
        assert!(
            !sidecar.handle_notification(&engine, &stale_update.to_chain_notification()).unwrap()
        );
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().number, 0);
        drop(engine);
    }

    #[test]
    fn sidecar_persists_an_idle_commit_and_joins_on_shutdown() {
        use reth::tasks::Runtime;
        use std::time::Duration;
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        sidecar.config.prune_interval = Duration::from_millis(20);
        let state = sidecar.provider.canonical_in_memory_state();
        let storage = sidecar.storage.clone();
        let readiness = sidecar.readiness.clone();
        let runtime = Runtime::test();
        let task = runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
            sidecar.run(shutdown).await.unwrap();
        });
        runtime.handle().block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !readiness.is_ready() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
            let expected = block.recovered_block.hash();
            let update = NewCanonicalChain::Commit { new: vec![block.clone()] };
            let notification = update.to_chain_notification();
            state.update_chain(update);
            state.set_canonical_head(block.recovered_block.clone_sealed_header());
            state.notify_canon_state(notification);
            // Repeated head polls must not postpone persistence of a single paused tail block.
            tokio::time::timeout(Duration::from_secs(5), async {
                while storage.provider_ro().unwrap().get_latest_block().unwrap().hash != expected {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        });
        assert!(runtime.graceful_shutdown_with_timeout(Duration::from_secs(5)));
        runtime.handle().block_on(task).unwrap();
        assert!(!readiness.is_ready());
    }

    #[test]
    fn sidecar_replays_canonical_memory_tail_without_notifications() {
        use reth::tasks::Runtime;
        use std::time::Duration;
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        sidecar.config.prune_interval = Duration::from_secs(60);
        sidecar.config.verification_interval = 1;
        let provider = sidecar.provider.clone();
        let state = provider.canonical_in_memory_state();
        let storage = sidecar.storage.clone();
        let readiness = sidecar.readiness.clone();
        let runtime = Runtime::test();
        let mut parent = spec.genesis_hash();
        let mut blocks = Vec::new();
        for number in 1..=65 {
            let block = executed(number, parent, spec.genesis_header().state_root, 1);
            parent = block.recovered_block.hash();
            blocks.push(block);
        }
        let header = blocks.last().unwrap().recovered_block.clone_sealed_header();
        state.update_chain(NewCanonicalChain::Commit { new: blocks });
        state.set_canonical_head(header);
        let task = runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
            sidecar.run(shutdown).await.unwrap();
        });
        runtime.handle().block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !readiness.is_ready() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            // A paused in-memory tail must be replayed even without a notification.
            tokio::time::timeout(Duration::from_secs(5), async {
                while storage.provider_ro().unwrap().get_latest_block().unwrap().number != 65 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        });
        assert!(runtime.graceful_shutdown_with_timeout(Duration::from_secs(5)));
        runtime.handle().block_on(task).unwrap();
    }

    fn hash(byte: u8) -> B256 {
        B256::with_last_byte(byte)
    }

    #[test]
    fn committed_chain_contiguity_accepts_next_block_and_overlaps() {
        // The next block extends stored history directly.
        assert!(committed_chain_is_contiguous(101, 100));
        // A commit whose chain starts at or below the stored head (e.g. buffered notifications
        // overlapping blocks the backfill already stored) leaves no gap either; the per-block
        // loop starts above the stored head and consumes only the new suffix.
        assert!(committed_chain_is_contiguous(95, 100));
    }

    #[test]
    fn committed_chain_contiguity_rejects_gaps() {
        assert!(!committed_chain_is_contiguous(102, 100));
    }

    #[test]
    fn committed_chain_contiguity_saturates_at_max_height() {
        assert!(committed_chain_is_contiguous(u64::MAX, u64::MAX));
    }

    #[test]
    fn proof_history_startup_action_ready_when_latest_is_canonical() {
        let action = proof_history_startup_action(
            Some((10, hash(10))),
            Some((20, hash(20))),
            20,
            Some(hash(10)),
            Some(hash(20)),
        )
        .expect("canonical latest should be ready");

        assert_eq!(action, ProofHistoryStartupAction::Ready);
    }

    #[test]
    fn proof_history_startup_action_rebuilds_when_latest_canonical_but_earliest_noncanonical() {
        let action = proof_history_startup_action(
            Some((10, hash(11))),
            Some((20, hash(20))),
            20,
            Some(hash(10)),
            Some(hash(20)),
        )
        .unwrap();

        assert_eq!(action, ProofHistoryStartupAction::Rebuild);
    }

    #[test]
    fn proof_history_startup_action_unwinds_when_latest_mismatches_and_earliest_is_canonical() {
        let action = proof_history_startup_action(
            Some((10, hash(10))),
            Some((20, hash(21))),
            20,
            Some(hash(10)),
            Some(hash(20)),
        )
        .expect("canonical earliest should allow retained-window rewind");

        assert_eq!(
            action,
            ProofHistoryStartupAction::ReconcileFork { earliest: BlockNumHash::new(10, hash(10)) }
        );
    }

    #[test]
    fn proof_history_startup_action_waits_when_latest_is_ahead_of_canonical_best() {
        // An ungraceful restart leaves the persisted head a few blocks behind the previously
        // indexed in-memory tip. The driver re-derives the same blocks within seconds, so wait
        // for canonical state to catch up instead of discarding the whole retained window.
        let action = proof_history_startup_action(
            Some((10, hash(10))),
            Some((25, hash(25))),
            20,
            Some(hash(10)),
            None,
        )
        .expect("latest ahead of canonical best should wait for the node to catch up");

        assert_eq!(
            action,
            ProofHistoryStartupAction::WaitForCanonicalLatest { latest: 25, canonical_best: 20 }
        );
    }

    #[test]
    fn proof_history_startup_action_waits_when_canonical_has_not_reached_earliest() {
        // A re-synced chain database (with retained proof-history storage) has no header at the
        // stored earliest height yet. Wait for sync instead of failing: the node could never
        // sync past this point if reconciliation kept crashing it.
        let action =
            proof_history_startup_action(Some((10, hash(10))), Some((20, hash(20))), 5, None, None)
                .expect("canonical chain below stored earliest should wait for sync");

        assert_eq!(action, ProofHistoryStartupAction::WaitForCanonicalEarliest { earliest: 10 });
    }

    #[test]
    fn proof_history_startup_action_rebuilds_when_earliest_is_noncanonical() {
        let action = proof_history_startup_action(
            Some((10, hash(11))),
            Some((20, hash(21))),
            20,
            Some(hash(10)),
            Some(hash(20)),
        )
        .unwrap();

        assert_eq!(action, ProofHistoryStartupAction::Rebuild);
    }

    #[test]
    fn proof_history_startup_action_waits_when_latest_hash_disappears_during_revert() {
        let action = proof_history_startup_action(
            Some((10, hash(10))),
            Some((20, hash(20))),
            20,
            Some(hash(10)),
            None,
        )
        .unwrap();
        assert_eq!(
            action,
            ProofHistoryStartupAction::WaitForMissingCanonicalHash {
                latest: BlockNumHash::new(20, hash(20)),
                canonical_best: 20
            }
        );
    }

    use reth_provider::BlockHashReader;

    /// Waits until the persisted proof-history latest block reaches `number`.
    fn wait_for_latest(storage: &super::ProofHistoryStorage, number: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let latest = storage.provider_ro().unwrap().get_latest_block().unwrap().number;
            if latest == number {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "latest stuck at {latest}, wanted {number}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn stale_reorg_preserves_the_canonical_prefix() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let state = sidecar.provider.canonical_in_memory_state();
        let root = spec.genesis_header().state_root;

        // Canonical blocks 1 and 2 are indexed from notifications and persisted (threshold 1).
        let b1 = executed(1, spec.genesis_hash(), root, 1);
        let commit1 = NewCanonicalChain::Commit { new: vec![b1.clone()] };
        let n1 = commit1.to_chain_notification();
        state.update_chain(commit1);
        state.set_canonical_head(b1.recovered_block.clone_sealed_header());
        assert!(sidecar.handle_notification(&engine, &n1).unwrap());
        wait_for_latest(&sidecar.storage, 1);
        let b2 = executed(2, b1.recovered_block.hash(), root, 2);
        let commit2 = NewCanonicalChain::Commit { new: vec![b2.clone()] };
        let n2 = commit2.to_chain_notification();
        state.update_chain(commit2);
        state.set_canonical_head(b2.recovered_block.clone_sealed_header());
        assert!(sidecar.handle_notification(&engine, &n2).unwrap());
        wait_for_latest(&sidecar.storage, 2);

        // Reorg A replaces block 2; reorg B replaces the replacement before the sidecar sees A.
        let b2a = executed(2, b1.recovered_block.hash(), root, 3);
        let reorg_a = NewCanonicalChain::Reorg { old: vec![b2.clone()], new: vec![b2a.clone()] };
        let na = reorg_a.to_chain_notification();
        state.update_chain(reorg_a);
        state.set_canonical_head(b2a.recovered_block.clone_sealed_header());
        let b2b = executed(2, b1.recovered_block.hash(), root, 4);
        let reorg_b = NewCanonicalChain::Reorg { old: vec![b2a.clone()], new: vec![b2b.clone()] };
        state.update_chain(reorg_b);
        state.set_canonical_head(b2b.recovered_block.clone_sealed_header());

        // The stale reorg A is refused; run_loop then drops the engine and reconciles.
        assert!(!sidecar.handle_notification(&engine, &na).unwrap());
        drop(engine);
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        // Equal state roots on every block cannot identify the fork; retain the hash-matched
        // prefix.
        let latest = sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().number;
        assert_eq!(latest, 1);
        assert_eq!(sidecar.provider.block_hash(1).unwrap(), Some(b1.recovered_block.hash()));
    }

    #[test]
    fn stale_revert_preserves_the_canonical_prefix() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let state = sidecar.provider.canonical_in_memory_state();
        let root = spec.genesis_header().state_root;
        let b1 = executed(1, spec.genesis_hash(), root, 1);
        let commit1 = NewCanonicalChain::Commit { new: vec![b1.clone()] };
        let n1 = commit1.to_chain_notification();
        state.update_chain(commit1);
        state.set_canonical_head(b1.recovered_block.clone_sealed_header());
        assert!(sidecar.handle_notification(&engine, &n1).unwrap());
        wait_for_latest(&sidecar.storage, 1);
        let b2 = executed(2, b1.recovered_block.hash(), root, 2);
        let commit2 = NewCanonicalChain::Commit { new: vec![b2.clone()] };
        let n2 = commit2.to_chain_notification();
        state.update_chain(commit2);
        state.set_canonical_head(b2.recovered_block.clone_sealed_header());
        assert!(sidecar.handle_notification(&engine, &n2).unwrap());
        wait_for_latest(&sidecar.storage, 2);

        // Pure revert of block 2 (driver reset), then a new block 2' lands before the sidecar
        // consumes the revert notification.
        let revert = NewCanonicalChain::Reorg { old: vec![b2.clone()], new: vec![] };
        let nr = revert.to_chain_notification();
        state.update_chain(revert);
        state.set_canonical_head(b1.recovered_block.clone_sealed_header());
        let b2a = executed(2, b1.recovered_block.hash(), root, 3);
        let commit2a = NewCanonicalChain::Commit { new: vec![b2a.clone()] };
        state.update_chain(commit2a);
        state.set_canonical_head(b2a.recovered_block.clone_sealed_header());

        assert!(!sidecar.handle_notification(&engine, &nr).unwrap());
        drop(engine);
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        let latest = sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().number;
        assert_eq!(latest, 1);
    }

    #[test]
    fn replayed_root_mismatch_stops_the_sidecar() {
        use reth::tasks::Runtime;
        use reth_provider::{BlockWriter, DBProvider, DatabaseProviderFactory, ExecutionOutcome};
        use std::time::Duration;
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        sidecar.config.prune_interval = Duration::from_secs(5);
        sidecar.config.verification_interval = 1;
        let provider = sidecar.provider.clone();
        let storage = sidecar.storage.clone();
        let readiness = sidecar.readiness.clone();
        let runtime = Runtime::test();
        let task = runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
            sidecar.run(shutdown).await.unwrap();
        });
        runtime.handle().block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !readiness.is_ready() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            // Block 1 persisted on disk with a WRONG state root (divergent execution).
            let block = executed(1, spec.genesis_hash(), B256::repeat_byte(0xAA), 1);
            let rw = provider.database_provider_rw().unwrap();
            rw.append_blocks_with_state(
                vec![(*block.recovered_block).clone()],
                &ExecutionOutcome {
                    first_block: 1,
                    receipts: vec![vec![]],
                    requests: vec![Default::default()],
                    ..Default::default()
                },
                Default::default(),
            )
            .unwrap();
            rw.commit().unwrap();
            // Mirror what reth's engine tree does on every commit so the upstream
            // runner's best_block clamp does not hide the replay.
            let state = provider.canonical_in_memory_state();
            let commit = NewCanonicalChain::Commit { new: vec![block.clone()] };
            let notification = commit.to_chain_notification();
            state.update_chain(commit);
            state.set_canonical_head(block.recovered_block.clone_sealed_header());
            // Real production path: Commit notification whose tip is a verification block.
            state.notify_canon_state(notification);
            tokio::time::timeout(Duration::from_secs(5), async {
                while readiness.is_ready() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let latest = storage.provider_ro().unwrap().get_latest_block().unwrap().number;
            assert!(!readiness.is_ready(), "root mismatch must revoke readiness");
            assert_eq!(latest, 0);
        });
        assert!(
            runtime
                .handle()
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(5), task).await.unwrap()
                })
                .is_err()
        );
        assert!(runtime.graceful_shutdown_with_timeout(Duration::from_secs(5)));
    }

    #[test]
    fn snapshot_waits_for_pipeline_stages_to_agree() {
        use reth_provider::{DBProvider, DatabaseProviderFactory, StageCheckpointWriter};
        use reth_stages_types::{StageCheckpoint, StageId};
        for stage in [
            StageId::Execution,
            StageId::AccountHashing,
            StageId::StorageHashing,
            StageId::MerkleExecute,
        ] {
            let (sidecar, _spec, _dir) = sidecar_fixture();
            // Simulate the execution stage committing ahead of Finish.
            let db = sidecar.provider.database_provider_rw().unwrap();
            db.save_stage_checkpoint(stage, StageCheckpoint::new(1)).unwrap();
            db.commit().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
            let mut sidecar = sidecar;
            sidecar.storage = storage.clone().into();
            sidecar.init_storage = storage;
            sidecar.config.storage_path = Some(dir.path().to_path_buf());
            assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Wait(_)));
            assert!(sidecar.storage.provider_ro().unwrap().get_latest_block().is_err());
        }
    }

    #[test]
    fn default_snapshot_recovers_when_its_anchor_is_reorged() {
        let (mut sidecar, _spec, _dir) = sidecar_fixture();
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        let init = storage.initialization_provider().unwrap();
        init.set_initial_state_anchor(BlockNumHash::new(0, B256::repeat_byte(7))).unwrap();
        init.commit_initial_state().unwrap();
        init.commit().unwrap();
        sidecar.storage = storage.clone().into();
        sidecar.init_storage = storage;
        sidecar.config.storage_path = Some(dir.path().to_path_buf());
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        assert!(sidecar.storage.provider_ro().unwrap().get_latest_block().is_err());
    }

    #[test]
    fn live_verification_does_not_force_subsequent_commits_into_disk_replay() {
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        sidecar.config.verification_interval = 2;
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let state = sidecar.provider.canonical_in_memory_state();
        let mut parent = spec.genesis_hash();
        for number in 1..=4 {
            let block = executed(number, parent, spec.genesis_header().state_root, number as u8);
            parent = block.recovered_block.hash();
            let commit = NewCanonicalChain::Commit { new: vec![block.clone()] };
            let notification = commit.to_chain_notification();
            state.update_chain(commit);
            state.set_canonical_head(block.recovered_block.clone_sealed_header());
            assert!(sidecar.handle_notification(&engine, &notification).unwrap());
            assert_eq!(
                sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().hash,
                parent
            );
        }
    }
    fn index_empty_chain(
        sidecar: &ProofHistorySidecar<EthEvmConfig, BlockchainProvider<MockNodeTypesWithDB>>,
        spec: &ChainSpec,
        count: u64,
    ) -> Vec<ExecutedBlock> {
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let state = sidecar.provider.canonical_in_memory_state();
        let mut parent = spec.genesis_hash();
        let mut blocks = Vec::new();
        for number in 1..=count {
            let block = executed(number, parent, spec.genesis_header().state_root, number as u8);
            parent = block.recovered_block.hash();
            let commit = NewCanonicalChain::Commit { new: vec![block.clone()] };
            let notification = commit.to_chain_notification();
            state.update_chain(commit);
            state.set_canonical_head(block.recovered_block.clone_sealed_header());
            assert!(sidecar.handle_notification(&engine, &notification).unwrap());
            blocks.push(block);
        }
        blocks
    }

    #[test]
    fn successful_noop_submission_is_retried_until_durable() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![block.clone()] });
        state.set_canonical_head(block.recovered_block.clone_sealed_header());
        let expected = BlockNumHash::new(1, block.recovered_block.hash());
        sidecar.init_storage.record_hashes([(1, expected.hash)]).unwrap();
        let mut attempts = 0;
        assert!(
            sidecar
                .persist_block(expected, || {
                    attempts += 1;
                    // Model upstream's recoverable StateForHashNotFound -> Ok(()) contract.
                    if attempts > 1 {
                        engine.execute_block(&block.recovered_block)?;
                    }
                    Ok(())
                })
                .unwrap()
        );
        assert!(attempts > 1);
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap(), expected);
    }

    #[test]
    fn pending_backfill_retains_history_when_only_its_latest_anchor_reorgs() {
        let (sidecar, spec, dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        std::fs::write(dir.path().join("backfill-target"), "0").unwrap();
        let new =
            executed(2, blocks[0].recovered_block.hash(), spec.genesis_header().state_root, 9);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Reorg {
            old: vec![blocks[1].clone()],
            new: vec![new.clone()],
        });
        state.set_canonical_head(new.recovered_block.clone_sealed_header());
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        let window = sidecar.storage.provider_ro().unwrap().get_proof_window().unwrap();
        assert_eq!(window.earliest.number, 0);
        assert_eq!(window.latest.number, 1);
        assert_eq!(window.latest.hash, blocks[0].recovered_block.hash());
        assert!(dir.path().join("backfill-target").exists());
    }

    #[test]
    fn startup_prune_enforces_limit_and_prunes_the_hash_journal() {
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        sidecar.config.window = 0;
        sidecar.config.max_startup_prune_blocks = 1;
        assert!(sidecar.prepare().err().unwrap().to_string().contains("requires pruning 2"));
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_earliest_block().unwrap().number, 0);
        sidecar.config.window = 1;
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Ready(_)));
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_earliest_block().unwrap().number, 1);
        assert_eq!(sidecar.init_storage.indexed_hash(0).unwrap(), None);
        assert_eq!(
            sidecar.init_storage.indexed_hash(1).unwrap(),
            Some(blocks[0].recovered_block.hash())
        );
    }

    #[test]
    fn snapshot_waits_for_partial_merkle_work_and_preserves_a_bad_copy() {
        use reth_db::{tables, transaction::DbTxMut};
        use reth_provider::{DBProvider, DatabaseProviderFactory, StageCheckpointWriter};
        use reth_stages_types::StageId;
        let (sidecar, _spec, _dir) = sidecar_fixture();
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        let db = sidecar.provider.database_provider_rw().unwrap();
        db.save_stage_checkpoint_progress(StageId::MerkleExecute, vec![1]).unwrap();
        db.commit().unwrap();
        assert!(
            !initialize_proof_history_storage(&sidecar.provider, storage.clone(), None).unwrap()
        );
        assert!(storage.provider_ro().unwrap().get_latest_block().is_err());
        let db = sidecar.provider.database_provider_rw().unwrap();
        db.save_stage_checkpoint_progress(StageId::MerkleExecute, vec![]).unwrap();
        // Matching checkpoints alone do not validate corrupted or inconsistent state tables.
        db.tx_ref().clear::<tables::AccountsTrie>().unwrap();
        db.tx_ref().clear::<tables::HashedAccounts>().unwrap();
        db.commit().unwrap();
        let error = initialize_proof_history_storage(&sidecar.provider, storage.clone(), None)
            .expect_err("a corrupt pinned source must fail instead of copying forever");
        assert!(error.to_string().contains("state root mismatch"));
        assert!(
            error.to_string().contains("new, empty --proofs-history.storage-path"),
            "{error:#}"
        );
        assert_eq!(storage.provider_ro().unwrap().get_latest_block().unwrap().number, 0);
        assert!(storage.indexed_hash(0).unwrap().is_some());
        // A repaired source does not overwrite the completed, invalid copy on restart.
        let (mut repaired, _spec, _repaired_dir) = sidecar_fixture();
        repaired.storage = storage.clone().into();
        repaired.init_storage = storage.clone();
        repaired.config.storage_path = Some(dir.path().to_path_buf());
        let error =
            repaired.prepare().err().expect("the old path must still reject its invalid root");
        assert!(
            error.to_string().contains("new, empty --proofs-history.storage-path"),
            "{error:#}"
        );
        assert_eq!(storage.provider_ro().unwrap().get_latest_block().unwrap().number, 0);
    }

    #[test]
    fn replay_uses_proof_state_while_pipeline_state_is_ahead_of_finish() {
        use reth_db::{tables, transaction::DbTxMut};
        use reth_provider::{DBProvider, DatabaseProviderFactory, StageCheckpointWriter};
        use reth_stages_types::{StageCheckpoint, StageId};
        let (sidecar, spec, _dir) = sidecar_fixture();
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![block.clone()] });
        state.set_canonical_head(block.recovered_block.clone_sealed_header());
        // The canonical provider's latest state is deliberately wrong for the parent. The
        // upstream proof provider must continue to supply the copied genesis accounts and root.
        let db = sidecar.provider.database_provider_rw().unwrap();
        db.tx_ref().clear::<tables::PlainAccountState>().unwrap();
        db.tx_ref().clear::<tables::HashedAccounts>().unwrap();
        db.save_stage_checkpoint(StageId::Execution, StageCheckpoint::new(2)).unwrap();
        db.commit().unwrap();
        assert_eq!(sidecar.replay_available(&engine).unwrap(), (true, false));
        assert_eq!(
            sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().hash,
            block.recovered_block.hash()
        );
    }

    #[test]
    fn shorter_divergent_chain_reconciles_before_regrowing_to_stored_tip() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 3);
        let replacement =
            executed(2, blocks[0].recovered_block.hash(), spec.genesis_header().state_root, 9);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Reorg {
            old: blocks[1..].to_vec(),
            new: vec![replacement.clone()],
        });
        state.set_canonical_head(replacement.recovered_block.clone_sealed_header());
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        let window = sidecar.storage.provider_ro().unwrap().get_proof_window().unwrap();
        assert_eq!(window.earliest.number, 0);
        assert_eq!(window.latest.number, 1);
    }

    #[test]
    fn journal_gaps_conservatively_retain_the_earliest_anchor() {
        for (low, high) in [(0, 2), (3, 3)] {
            let (sidecar, spec, _dir) = sidecar_fixture();
            let blocks = index_empty_chain(&sidecar, &spec, 3);
            sidecar.init_storage.retain_hashes(low, high).unwrap();
            let replacement =
                executed(3, blocks[1].recovered_block.hash(), spec.genesis_header().state_root, 9);
            let state = sidecar.provider.canonical_in_memory_state();
            state.update_chain(NewCanonicalChain::Reorg {
                old: vec![blocks[2].clone()],
                new: vec![replacement.clone()],
            });
            state.set_canonical_head(replacement.recovered_block.clone_sealed_header());
            assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
            assert_eq!(
                sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().number,
                0
            );
        }
    }

    #[test]
    fn persistence_stops_before_submitting_cancelled_or_reorged_work() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let expected = BlockNumHash::new(1, B256::repeat_byte(1));
        assert!(!sidecar.persist_block(expected, || panic!("must not submit stale work")).unwrap());
        sidecar.init_storage.cancel_bootstrap();
        assert!(
            !sidecar
                .persist_block(BlockNumHash::new(0, spec.genesis_hash()), || panic!(
                    "must not submit cancelled work"
                ))
                .unwrap()
        );
    }
    #[test]
    fn reorg_during_snapshot_preparation_does_not_cancel_the_copy() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let runtime = reth::tasks::Runtime::test();
        runtime.handle().block_on(async {
            let (sender, mut notifications) = tokio::sync::broadcast::channel(4);
            let old = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
            let new = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 2);
            sender
                .send(
                    NewCanonicalChain::Reorg { old: vec![old], new: vec![new] }
                        .to_chain_notification(),
                )
                .unwrap();
            let (proceed, waiting) = tokio::sync::oneshot::channel();
            let preparation = sidecar.await_preparation(
                async {
                    waiting.await.unwrap();
                    Ok(StartupStep::Progress)
                },
                &mut notifications,
            );
            tokio::pin!(preparation);
            assert!(futures_util::poll!(&mut preparation).is_pending());
            assert!(
                !sidecar.init_storage.bootstrap_cancelled(),
                "a buffered reorg must not abandon the pinned initial copy"
            );
            proceed.send(()).unwrap();
            let (result, listen) = preparation.await;
            assert!(listen);
            assert!(matches!(result.unwrap(), StartupStep::Progress));
        });
    }

    #[test]
    fn preparation_rechecks_a_ready_engine_after_a_buffered_reorg() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let runtime = reth::tasks::Runtime::test();
        runtime.handle().block_on(async {
            let (sender, mut notifications) = tokio::sync::broadcast::channel(4);
            let old = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
            sender
                .send(
                    NewCanonicalChain::Reorg { old: vec![old], new: vec![] }
                        .to_chain_notification(),
                )
                .unwrap();
            let (proceed, waiting) = tokio::sync::oneshot::channel();
            let preparation = sidecar.await_preparation(
                async {
                    waiting.await.unwrap();
                    Ok(StartupStep::Ready(engine))
                },
                &mut notifications,
            );
            tokio::pin!(preparation);
            assert!(futures_util::poll!(&mut preparation).is_pending());
            proceed.send(()).unwrap();
            let (result, listen) = preparation.await;
            assert!(listen);
            assert!(
                matches!(result.unwrap(), StartupStep::Progress),
                "readiness must wait for another canonical check"
            );
        });
    }

    #[test]
    fn snapshot_header_reorg_is_retried_without_claiming_corruption() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let latest = BlockNumHash::new(0, spec.genesis_hash());
        let replacement = executed(0, B256::ZERO, B256::repeat_byte(0xaa), 9);
        assert!(
            !sidecar
                .validate_snapshot_header(
                    latest,
                    Some(replacement.recovered_block.clone_sealed_header())
                )
                .unwrap()
        );
        assert!(!sidecar.validate_snapshot_header(latest, None).unwrap());
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap(), latest);
    }

    #[test]
    fn replay_error_includes_block_and_non_destructive_recovery_guidance() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![block.clone()] });
        state.set_canonical_head(block.recovered_block.clone_sealed_header());
        let error = sidecar
            .persist_block(BlockNumHash::new(1, block.recovered_block.hash()), || {
                Err(eyre::eyre!("execution failed"))
            })
            .unwrap_err();
        let report = format!("{error:#}");
        assert!(report.contains("block 1"));
        assert!(report.contains("--proofs-history.storage-path"));
        assert!(report.contains("execution failed"));
    }

    #[test]
    fn ancestor_fcu_notification_requests_reconciliation() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Reorg { old: vec![blocks[1].clone()], new: vec![] });
        state.set_canonical_head(blocks[0].recovered_block.clone_sealed_header());
        let notification =
            NewCanonicalChain::Reorg { old: vec![blocks[1].clone()], new: vec![blocks[0].clone()] }
                .to_chain_notification();
        assert!(!sidecar.handle_notification(&engine, &notification).unwrap());
    }

    #[test]
    fn persistence_timeout_requests_reconciliation_without_resubmitting() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![block.clone()] });
        state.set_canonical_head(block.recovered_block.clone_sealed_header());
        assert!(
            !sidecar
                .persist_block_until(
                    BlockNumHash::new(1, block.recovered_block.hash()),
                    std::time::Instant::now(),
                    || panic!("must not submit after the deadline")
                )
                .unwrap()
        );
    }

    #[test]
    fn closed_notifications_cancel_preparation_and_join_its_worker() {
        let (sidecar, _spec, _dir) = sidecar_fixture();
        let runtime = reth::tasks::Runtime::test();
        runtime.handle().block_on(async {
            let (sender, mut notifications) = tokio::sync::broadcast::channel(4);
            drop(sender);
            let (proceed, waiting) = tokio::sync::oneshot::channel();
            let preparation = sidecar.await_preparation(
                async {
                    waiting.await.unwrap();
                    Ok(StartupStep::Progress)
                },
                &mut notifications,
            );
            tokio::pin!(preparation);
            assert!(futures_util::poll!(&mut preparation).is_pending());
            assert!(sidecar.init_storage.bootstrap_cancelled());
            proceed.send(()).unwrap();
            let (result, listen) = preparation.await;
            assert!(!listen);
            assert!(matches!(result.unwrap(), StartupStep::Progress));
        });
    }
    #[test]
    fn pending_snapshot_corruption_preserves_evidence_across_reopen() {
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(ProofHistoryDatabase::open(dir.path()).unwrap());
        let anchor = BlockNumHash::new(0, spec.genesis_hash());
        let init = storage.initialization_provider().unwrap();
        init.set_initial_state_anchor(anchor).unwrap();
        init.commit_initial_state().unwrap();
        init.commit().unwrap();
        storage.record_hashes([(0, anchor.hash)]).unwrap();
        std::fs::write(dir.path().join("backfill-target"), "0").unwrap();
        sidecar.storage = storage.clone().into();
        sidecar.init_storage = storage.clone();
        sidecar.config.storage_path = Some(dir.path().to_path_buf());
        let error = sidecar.prepare().err().unwrap();
        assert!(error.to_string().contains("preserve the old"));
        assert_eq!(storage.provider_ro().unwrap().get_latest_block().unwrap(), anchor);
        drop(sidecar);
        drop(storage);
        let storage = ProofHistoryDatabase::open(dir.path()).unwrap();
        assert_eq!(storage.provider_ro().unwrap().get_latest_block().unwrap(), anchor);
        assert_eq!(storage.indexed_hash(0).unwrap(), Some(anchor.hash));
        assert!(dir.path().join("backfill-target").exists());
    }

    #[test]
    fn preparation_rechecks_after_lag_even_if_the_remaining_notification_is_a_commit() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let runtime = reth::tasks::Runtime::test();
        runtime.handle().block_on(async {
            let (sender, mut notifications) = tokio::sync::broadcast::channel(1);
            let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
            sender
                .send(
                    NewCanonicalChain::Reorg { old: vec![block.clone()], new: vec![] }
                        .to_chain_notification(),
                )
                .unwrap();
            sender
                .send(NewCanonicalChain::Commit { new: vec![block] }.to_chain_notification())
                .unwrap();
            let (proceed, waiting) = tokio::sync::oneshot::channel();
            let preparation = sidecar.await_preparation(
                async {
                    waiting.await.unwrap();
                    Ok(StartupStep::Ready(engine))
                },
                &mut notifications,
            );
            tokio::pin!(preparation);
            assert!(futures_util::poll!(&mut preparation).is_pending());
            assert!(!sidecar.init_storage.bootstrap_cancelled());
            proceed.send(()).unwrap();
            let (result, listen) = preparation.await;
            assert!(listen);
            assert!(matches!(result.unwrap(), StartupStep::Progress));
        });
    }

    #[test]
    fn submission_error_after_a_reorg_requests_reconciliation() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let old = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let new = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 2);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![old.clone()] });
        state.set_canonical_head(old.recovered_block.clone_sealed_header());
        assert!(
            !sidecar
                .persist_block(BlockNumHash::new(1, old.recovered_block.hash()), || {
                    state.update_chain(NewCanonicalChain::Reorg {
                        old: vec![old.clone()],
                        new: vec![new.clone()],
                    });
                    state.set_canonical_head(new.recovered_block.clone_sealed_header());
                    Err(eyre::eyre!("parent disappeared during execution"))
                })
                .unwrap()
        );
    }

    #[test]
    fn shorter_chain_waits_without_journal_evidence_of_divergence() {
        for missing_tip_hash in [false, true] {
            let (sidecar, spec, _dir) = sidecar_fixture();
            let blocks = index_empty_chain(&sidecar, &spec, 3);
            let state = sidecar.provider.canonical_in_memory_state();
            let new = if missing_tip_hash {
                executed(2, blocks[0].recovered_block.hash(), spec.genesis_header().state_root, 9)
            } else {
                blocks[1].clone()
            };
            state.update_chain(NewCanonicalChain::Reorg {
                old: blocks[1..].to_vec(),
                new: vec![new.clone()],
            });
            state.set_canonical_head(new.recovered_block.clone_sealed_header());
            if missing_tip_hash {
                sidecar.init_storage.retain_hashes(0, 2).unwrap();
            }
            assert!(
                matches!(
                    sidecar.startup_action().unwrap(),
                    ProofHistoryStartupAction::WaitForCanonicalLatest {
                        latest: 3,
                        canonical_best: 2
                    }
                ),
                "missing_tip_hash={missing_tip_hash}"
            );
        }
    }

    #[test]
    fn pending_backfill_waits_when_the_canonical_anchor_is_only_in_memory() {
        use reth_optimism_trie::OpProofsProviderRw;
        let (sidecar, spec, dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 1);
        let anchor = BlockNumHash::new(1, blocks[0].recovered_block.hash());
        let rw = sidecar.storage.provider_rw().unwrap();
        rw.prune_earliest_state(alloy_eips::eip1898::BlockWithParent::new(
            spec.genesis_hash(),
            anchor,
        ))
        .unwrap();
        rw.commit().unwrap();
        std::fs::write(dir.path().join("backfill-target"), "0").unwrap();
        assert!(matches!(sidecar.startup_action().unwrap(), ProofHistoryStartupAction::Ready));
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Wait(_)));
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_earliest_block().unwrap(), anchor);
        assert!(dir.path().join("backfill-target").exists());
    }

    #[test]
    fn a_reorg_during_journal_batch_does_not_corrupt_fork_reconciliation() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        let StartupStep::Ready(engine) = sidecar.prepare().unwrap() else { panic!("ready") };
        let third =
            executed(3, blocks[1].recovered_block.hash(), spec.genesis_header().state_root, 3);
        let fourth = executed(4, third.recovered_block.hash(), spec.genesis_header().state_root, 4);
        let replacement =
            executed(2, blocks[0].recovered_block.hash(), spec.genesis_header().state_root, 9);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![third.clone(), fourth.clone()] });
        state.set_canonical_head(fourth.recovered_block.clone_sealed_header());
        sidecar
            .init_storage
            .record_hashes(
                [(3, third.recovered_block.hash()), (4, fourth.recovered_block.hash())]
                    .into_iter()
                    .inspect(|(number, _)| {
                        if *number == 4 {
                            state.update_chain(NewCanonicalChain::Reorg {
                                old: vec![blocks[1].clone(), third.clone(), fourth.clone()],
                                new: vec![replacement.clone()],
                            });
                            state.set_canonical_head(
                                replacement.recovered_block.clone_sealed_header(),
                            );
                        }
                    }),
            )
            .unwrap();
        assert!(
            !sidecar
                .persist_block(BlockNumHash::new(3, third.recovered_block.hash()), || panic!(
                    "must not submit the obsolete batch"
                ))
                .unwrap()
        );
        drop(engine);
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().number, 1);
        for number in 2..=4 {
            assert_eq!(sidecar.init_storage.indexed_hash(number).unwrap(), None);
        }
    }
    #[test]
    fn missing_hash_alert_tracks_the_retained_tip_and_resets_after_recovery() {
        use super::super::test_utils::TestMetrics;
        let recorder = TestMetrics::default();
        metrics::with_local_recorder(&recorder, || {
            let mut observations = super::StartupObservations::default();
            for best in 20..25 {
                observations.observe(&StartupStep::MissingCanonicalHash {
                    latest: BlockNumHash::new(20, hash(20)),
                    canonical_best: best,
                });
                assert_eq!(
                    recorder.value("taiko_proof_history_missing_canonical_hash", &[]),
                    Some(0.0)
                );
            }
            observations.observe(&StartupStep::MissingCanonicalHash {
                latest: BlockNumHash::new(20, hash(20)),
                canonical_best: 25,
            });
            assert_eq!(
                recorder.value("taiko_proof_history_missing_canonical_hash", &[]),
                Some(1.0)
            );
            observations.observe(&StartupStep::Progress);
            assert_eq!(
                recorder.value("taiko_proof_history_missing_canonical_hash", &[]),
                Some(0.0)
            );
            observations.observe(&StartupStep::MissingCanonicalHash {
                latest: BlockNumHash::new(21, hash(21)),
                canonical_best: 25,
            });
            assert_eq!(observations.consecutive, 1);
        });
    }

    #[test]
    fn canonical_change_before_engine_start_is_reconciled_without_spawning() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        assert_eq!(sidecar.startup_action().unwrap(), ProofHistoryStartupAction::Ready);
        let replacement =
            executed(2, blocks[0].recovered_block.hash(), spec.genesis_header().state_root, 9);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Reorg {
            old: vec![blocks[1].clone()],
            new: vec![replacement.clone()],
        });
        state.set_canonical_head(replacement.recovered_block.clone_sealed_header());
        let pruner = reth_optimism_trie::OpProofStoragePruner::new(
            sidecar.storage.clone(),
            sidecar.provider.clone(),
            sidecar.config.window,
        );
        assert!(matches!(sidecar.start_engine(pruner).unwrap(), StartupStep::Progress));
        assert_eq!(
            sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().hash,
            blocks[1].recovered_block.hash()
        );
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Progress));
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_latest_block().unwrap().number, 1);
    }
    #[test]
    fn a_missing_hash_remains_nonfatal_with_a_moving_or_frozen_head() {
        for moving in [false, true] {
            let mut observations = super::StartupObservations::default();
            for attempt in 0..8 {
                let best = if moving { 20 + attempt } else { 20 };
                observations.observe(&StartupStep::MissingCanonicalHash {
                    latest: BlockNumHash::new(20, hash(20)),
                    canonical_best: best,
                });
            }
            assert_eq!(observations.consecutive, 8, "moving={moving}");
        }
    }

    #[test]
    fn missing_hash_run_loop_alerts_keeps_syncing_and_recovers() {
        use super::super::test_utils::TestMetrics;
        use reth::tasks::Runtime;
        use std::time::Duration;
        let (sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        let provider = sidecar.provider.clone();
        let state = provider.canonical_in_memory_state();
        // Replace the tip with an orphan whose parent exists nowhere: the in-memory chain and the
        // head tracker agree on block 3 while block 2 has no canonical hash at all.
        let orphan =
            executed(3, blocks[1].recovered_block.hash(), spec.genesis_header().state_root, 3);
        state.update_chain(NewCanonicalChain::Reorg {
            old: vec![blocks[1].clone()],
            new: vec![orphan.clone()],
        });
        state.set_canonical_head(orphan.recovered_block.clone_sealed_header());
        assert!(matches!(
            sidecar.startup_action().unwrap(),
            ProofHistoryStartupAction::WaitForMissingCanonicalHash { .. }
        ));
        let readiness = sidecar.readiness.clone();
        let recorder = Arc::new(TestMetrics::default());
        let capture = recorder.clone();
        let this = Arc::new(sidecar);
        let runtime = Runtime::test();
        let task = runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
            let mut future =
                Box::pin(ProofHistorySidecar::run_loop(&this, shutdown, Duration::from_millis(2)));
            std::future::poll_fn(|cx| {
                metrics::with_local_recorder(&*capture, || {
                    std::future::Future::poll(future.as_mut(), cx)
                })
            })
            .await
            .unwrap();
        });
        runtime.handle().block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    assert!(
                        !task.is_finished(),
                        "a missing hash must not stop the execution client"
                    );
                    if recorder.value("taiko_proof_history_missing_canonical_hash", &[]) ==
                        Some(1.0)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            assert!(!readiness.is_ready());
            state.update_chain(NewCanonicalChain::Reorg {
                old: vec![orphan.clone()],
                new: vec![blocks[1].clone()],
            });
            state.set_canonical_head(blocks[1].recovered_block.clone_sealed_header());
            tokio::time::timeout(Duration::from_secs(5), async {
                while !readiness.is_ready() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                recorder.value("taiko_proof_history_missing_canonical_hash", &[]),
                Some(0.0)
            );
        });
        assert!(runtime.graceful_shutdown_with_timeout(Duration::from_secs(5)));
        runtime.handle().block_on(task).unwrap();
    }

    #[test]
    fn abandoning_backfill_clears_auxiliary_state_and_keeps_the_retained_window() {
        use reth_db::{Database, transaction::DbTx};
        use reth_optimism_trie::{
            OpProofsBackfillStore, OpProofsSnapshotInitProvider, SnapshotInitJob,
            SnapshotInitStatus,
        };
        let (sidecar, spec, dir) = sidecar_fixture();
        index_empty_chain(&sidecar, &spec, 2);
        SnapshotInitJob::new(&sidecar.provider, sidecar.init_storage.clone()).run(0).unwrap();
        let window = sidecar.storage.provider_ro().unwrap().get_proof_window().unwrap();
        assert!(!dir.path().join("backfill-target").exists());
        assert!(matches!(sidecar.prepare().unwrap(), StartupStep::Ready(_)));
        assert_eq!(sidecar.storage.provider_ro().unwrap().get_proof_window().unwrap(), window);
        assert_eq!(
            sidecar
                .init_storage
                .env
                .tx()
                .unwrap()
                .entries::<reth_optimism_trie::db::V2HashedAccountsSnapshot>()
                .unwrap(),
            0,
            "the auxiliary data tables must be cleared, not only the meta row"
        );
        assert!(matches!(
            sidecar
                .init_storage
                .snapshot_initialization_provider()
                .unwrap()
                .snapshot_init_anchor()
                .unwrap()
                .status,
            SnapshotInitStatus::NotStarted
        ));
    }

    /// Runs the pre-engine recheck after `setup` reshapes canonical state around a two-block
    /// window, returning the classification alongside the step it produced.
    fn engine_start_after(
        setup: impl FnOnce(
            &ProofHistorySidecar<EthEvmConfig, BlockchainProvider<MockNodeTypesWithDB>>,
            &ChainSpec,
            &[ExecutedBlock],
        ),
    ) -> (ProofHistoryStartupAction, StartupStep) {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        setup(&sidecar, &spec, &blocks);
        let pruner = reth_optimism_trie::OpProofStoragePruner::new(
            sidecar.storage.clone(),
            sidecar.provider.clone(),
            sidecar.config.window,
        );
        (sidecar.startup_action().unwrap(), sidecar.start_engine(pruner).unwrap())
    }

    #[test]
    fn engine_start_preserves_each_unavailable_canonical_wait() {
        use reth_optimism_trie::OpProofsProviderRw;
        let (action, step) = engine_start_after(|sidecar, _spec, blocks| {
            let state = sidecar.provider.canonical_in_memory_state();
            state.update_chain(NewCanonicalChain::Reorg {
                old: vec![blocks[1].clone()],
                new: vec![],
            });
            state.set_canonical_head(blocks[0].recovered_block.clone_sealed_header());
        });
        assert!(matches!(
            action,
            ProofHistoryStartupAction::WaitForCanonicalLatest { latest: 2, canonical_best: 1 }
        ));
        assert!(matches!(
            step,
            StartupStep::Wait(WaitReason::CanonicalLatest { latest: 2, canonical_best: 1 })
        ));

        let (action, step) = engine_start_after(|sidecar, spec, blocks| {
            let state = sidecar.provider.canonical_in_memory_state();
            let orphan =
                executed(3, blocks[1].recovered_block.hash(), spec.genesis_header().state_root, 3);
            state.update_chain(NewCanonicalChain::Reorg {
                old: vec![blocks[1].clone()],
                new: vec![orphan.clone()],
            });
            state.set_canonical_head(orphan.recovered_block.clone_sealed_header());
        });
        assert!(matches!(
            action,
            ProofHistoryStartupAction::WaitForMissingCanonicalHash { latest, canonical_best: 3 }
                if latest.number == 2
        ));
        assert!(matches!(
            step,
            StartupStep::MissingCanonicalHash { latest, canonical_best: 3 } if latest.number == 2
        ));

        let (action, step) = engine_start_after(|sidecar, spec, blocks| {
            let rw = sidecar.storage.provider_rw().unwrap();
            rw.prune_earliest_state(alloy_eips::eip1898::BlockWithParent::new(
                spec.genesis_hash(),
                BlockNumHash::new(1, blocks[0].recovered_block.hash()),
            ))
            .unwrap();
            rw.commit().unwrap();
            let state = sidecar.provider.canonical_in_memory_state();
            state.update_chain(NewCanonicalChain::Reorg { old: blocks.to_vec(), new: vec![] });
            state.set_canonical_head(spec.sealed_genesis_header());
        });
        assert!(matches!(
            action,
            ProofHistoryStartupAction::WaitForCanonicalEarliest { earliest: 1 }
        ));
        assert!(matches!(step, StartupStep::Wait(WaitReason::CanonicalEarliest { earliest: 1 })));
    }

    #[test]
    fn bootstrap_failure_names_both_recovery_paths_and_keeps_its_cause() {
        use reth_optimism_trie::OpProofsProviderRw;
        let (sidecar, spec, dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        let rw = sidecar.storage.provider_rw().unwrap();
        rw.prune_earliest_state(alloy_eips::eip1898::BlockWithParent::new(
            spec.genesis_hash(),
            BlockNumHash::new(1, blocks[0].recovered_block.hash()),
        ))
        .unwrap();
        rw.commit().unwrap();
        let target = dir.path().join("backfill-target");
        std::fs::write(&target, "0").unwrap();
        sidecar.init_storage.cancel_bootstrap();
        let error = sidecar.prepare().err().unwrap();
        let report = format!("{error:#}");
        assert!(report.contains("bootstrap cancelled"), "{report}");
        assert!(report.contains("new, empty --proofs-history.storage-path"), "{report}");
        assert!(report.contains(&target.display().to_string()), "{report}");
        assert!(report.contains("healthy retained window"), "{report}");
        assert!(target.exists());
    }

    #[test]
    fn stalled_engine_persistence_requests_reconciliation_instead_of_failing() {
        use reth_optimism_trie::engine::{EngineError, persistence::error::PersistenceError};
        let (sidecar, spec, _dir) = sidecar_fixture();
        let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![block.clone()] });
        state.set_canonical_head(block.recovered_block.clone_sealed_header());
        let expected = BlockNumHash::new(1, block.recovered_block.hash());
        for stalled in [
            EngineError::PersistenceTimeout,
            EngineError::PersistenceDisconnected,
            EngineError::Persistence(PersistenceError::Disconnected),
        ] {
            let mut error = Some(stalled);
            assert!(
                !sidecar
                    .persist_block(expected, || Err(error.take().expect("submitted once").into()))
                    .unwrap(),
                "a stalled engine must reconcile instead of stopping the node"
            );
        }
    }

    #[test]
    fn a_dead_engine_thread_is_reported_without_storage_rebuild_advice() {
        use reth_optimism_trie::engine::EngineError;
        let (sidecar, spec, _dir) = sidecar_fixture();
        let block = executed(1, spec.genesis_hash(), spec.genesis_header().state_root, 1);
        let state = sidecar.provider.canonical_in_memory_state();
        state.update_chain(NewCanonicalChain::Commit { new: vec![block.clone()] });
        state.set_canonical_head(block.recovered_block.clone_sealed_header());
        let expected = BlockNumHash::new(1, block.recovered_block.hash());
        let error = sidecar
            .persist_block(expected, || Err(EngineError::EngineDied.into()))
            .expect_err("a dead engine thread is not recoverable by resubmitting");
        let report = format!("{error:#}");
        assert!(report.contains("engine thread"), "{report}");
        assert!(!report.contains("--proofs-history.storage-path"), "{report}");
    }

    #[test]
    fn a_lagging_persisted_source_does_not_enter_the_pinned_backfill_phase() {
        use super::super::{
            storage_init::{BackfillStep, backfill_proof_history_storage},
            test_utils::TestMetrics,
        };
        use reth_optimism_trie::OpProofsProviderRw;
        let (sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 1);
        let rw = sidecar.storage.provider_rw().unwrap();
        rw.prune_earliest_state(alloy_eips::eip1898::BlockWithParent::new(
            spec.genesis_hash(),
            BlockNumHash::new(1, blocks[0].recovered_block.hash()),
        ))
        .unwrap();
        rw.commit().unwrap();
        let recorder = TestMetrics::default();
        let step = metrics::with_local_recorder(&recorder, || {
            backfill_proof_history_storage(&sidecar.provider, sidecar.init_storage.clone(), 0)
        })
        .unwrap();
        assert_eq!(step, BackfillStep::Wait);
        // Waiting for the persisted source is not a pinned phase: no start/end lines, no gauge.
        assert_eq!(
            recorder.value("taiko_proof_history_pin_active", &[("phase", "backfill")]),
            None
        );
    }

    #[test]
    fn transient_preparation_failures_are_retried_before_stopping() {
        use reth::tasks::Runtime;
        use std::time::Duration;
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        index_empty_chain(&sidecar, &spec, 3);
        // A retention window shorter than the retained history with automatic startup pruning
        // disabled is a configuration error, not a storage-integrity failure.
        sidecar.config.window = 1;
        sidecar.config.max_startup_prune_blocks = 0;
        // The test provider owns a runtime; keep a clone so the sidecar's last reference is not
        // dropped inside the task.
        let provider = sidecar.provider.clone();
        let this = Arc::new(sidecar);
        let runtime = Runtime::test();
        let (report_tx, report_rx) = tokio::sync::oneshot::channel();
        let task = runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
            let result =
                ProofHistorySidecar::run_loop(&this, shutdown, Duration::from_millis(2)).await;
            let _ = report_tx.send(result.err().map(|error| format!("{error:#}")));
        });
        let report = runtime
            .handle()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), report_rx).await.unwrap().unwrap()
            })
            .expect("bounded retries must end in the original error");
        assert!(report.contains("after 6 failed preparation attempts"), "{report}");
        assert!(report.contains("configuration requires pruning"), "{report}");
        runtime.handle().block_on(task).unwrap();
        drop(provider);
    }

    #[test]
    fn a_torn_head_tracker_is_classified_as_ordinary_catch_up() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        let state = sidecar.provider.canonical_in_memory_state();
        // reth updates the in-memory chain before its head tracker; a poll landing between the
        // two statements sees a tracker above the in-memory head, not a missing canonical hash.
        state.update_chain(NewCanonicalChain::Reorg { old: vec![blocks[1].clone()], new: vec![] });
        assert!(matches!(
            sidecar.startup_action().unwrap(),
            ProofHistoryStartupAction::WaitForCanonicalLatest { latest: 2, canonical_best: 1 }
        ));
    }

    #[test]
    fn a_hash_missing_below_a_consistent_head_is_classified_as_missing() {
        let (sidecar, spec, _dir) = sidecar_fixture();
        let blocks = index_empty_chain(&sidecar, &spec, 2);
        let state = sidecar.provider.canonical_in_memory_state();
        let orphan =
            executed(3, blocks[1].recovered_block.hash(), spec.genesis_header().state_root, 3);
        // The in-memory chain and the head tracker agree on block 3, but block 2 exists nowhere.
        state.update_chain(NewCanonicalChain::Reorg {
            old: vec![blocks[1].clone()],
            new: vec![orphan.clone()],
        });
        state.set_canonical_head(orphan.recovered_block.clone_sealed_header());
        assert!(matches!(
            sidecar.startup_action().unwrap(),
            ProofHistoryStartupAction::WaitForMissingCanonicalHash { latest, canonical_best: 3 }
                if latest.number == 2
        ));
    }

    #[test]
    fn a_cancelled_preparation_failure_is_not_reported_as_a_crash() {
        use reth::tasks::Runtime;
        use std::time::{Duration, Instant};
        let (mut sidecar, spec, _dir) = sidecar_fixture();
        index_empty_chain(&sidecar, &spec, 3);
        // prepare() fails only after its first proof-store write (see
        // transient_preparation_failures_are_retried_before_stopping).
        sidecar.config.window = 1;
        sidecar.config.max_startup_prune_blocks = 0;
        let init_storage = sidecar.init_storage.clone();
        let storage = sidecar.storage.clone();
        // Hold the proof-store writer so preparation blocks after the run loop has cleared
        // cancellation, then let shutdown cancel it before the failure surfaces.
        let writer = storage.provider_rw().unwrap();
        init_storage.cancel_bootstrap();
        let runtime = Runtime::test();
        let task = runtime.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
            sidecar.run(shutdown).await.expect("a cancelled preparation must not fail the sidecar");
        });
        let wait_for = |condition: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !condition() {
                assert!(Instant::now() < deadline, "timed out waiting for the run loop");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        // The run loop clears cancellation right before it spawns prepare().
        wait_for(&|| !init_storage.bootstrap_cancelled());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                assert!(runtime.graceful_shutdown_with_timeout(Duration::from_secs(10)));
            });
            // Shutdown re-arms cancellation while prepare() is still blocked on the writer.
            wait_for(&|| init_storage.bootstrap_cancelled());
            drop(writer);
        });
        runtime.handle().block_on(task).unwrap();
    }

    #[test]
    fn repeated_waits_log_on_entry_and_every_report_interval() {
        use super::{WAIT_REPORT_INTERVAL, wait_report_due};
        assert!(wait_report_due(1));
        assert!(!wait_report_due(2));
        assert!(!wait_report_due(WAIT_REPORT_INTERVAL - 1));
        assert!(wait_report_due(WAIT_REPORT_INTERVAL));
        assert!(wait_report_due(2 * WAIT_REPORT_INTERVAL));
    }

    #[test]
    fn readiness_is_mirrored_in_a_gauge() {
        use super::super::test_utils::TestMetrics;
        let (sidecar, _spec, _dir) = sidecar_fixture();
        let recorder = TestMetrics::default();
        metrics::with_local_recorder(&recorder, || {
            assert_eq!(recorder.value("taiko_proof_history_ready", &[]), None);
            sidecar.publish_readiness(true);
            assert!(sidecar.readiness.is_ready());
            assert_eq!(recorder.value("taiko_proof_history_ready", &[]), Some(1.0));
            sidecar.publish_readiness(false);
            assert!(!sidecar.readiness.is_ready());
            assert_eq!(recorder.value("taiko_proof_history_ready", &[]), Some(0.0));
        });
    }

    #[test]
    fn integrity_failures_stay_fatal_through_added_context() {
        use super::super::{fatal_integrity, is_fatal_integrity};
        let fatal = fatal_integrity("root mismatch".to_string()).wrap_err("while preparing");
        assert!(is_fatal_integrity(&fatal));
        let transient = eyre::eyre!("provider read failed").wrap_err("while preparing");
        assert!(!is_fatal_integrity(&transient));
    }
}
