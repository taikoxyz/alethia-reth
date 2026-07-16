# Proof-History Correctness V2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move Alethia proof history to the upstream V2 store and engine while making initialization, reorgs, pruning, and RPC reads correct under concurrent canonical-chain changes.

**Architecture:** Keep Alethia-specific orchestration at the boundaries where upstream lacks finality or notification policy. Use upstream `MdbxProofsStorageV2`, `InitializationJob`, `BackfillJob`, and `EngineHandle` for storage mechanics. Pin each decision to one main-DB/proof-DB snapshot, restart the sole engine on notification lag, prune only behind persisted finality, and make RPC work attest the exact canonical branch before returning.

**Tech Stack:** Rust, Tokio, MDBX, Reth `f2eecc6`, Optimism trie `4f21ce6`, jsonrpsee, Cargo nextest.

## Global Constraints

- Keep the branch stacked on `origin/feat/reth-v2.4.0-jit`.
- Follow red-green-refactor for every behavior change; do not delete an old implementation before its replacement test is red.
- Document every non-test production Rust symbol, field, method, and associated item.
- Use raw `Arc<MdbxProofsStorageV2>` for initialization/backfill and metrics. Use the metrics-wrapped `OpProofsStorage<Arc<MdbxProofsStorageV2>>` for engine, pruning, and RPC reads.
- Never hold V1 and V2 MDBX environments open on the same path at the same time.
- Read all related canonical facts from one `database_provider_ro()` snapshot and all related proof bounds from one proof provider transaction.
- Readiness means the committed proof database was reconciled. It never promises that the upstream engine's private buffer is persisted.
- Never clone the production `EngineHandle`; dropping its sole handle is the lag-recovery reset mechanism.
- Preserve `max_startup_prune_blocks` CLI compatibility, but derive exposure from persisted finality rather than proof latest.
- Remove `docs/superpowers` before publishing, as explicitly requested by the user.

---

## Task 1: Refuse V1 data and open only the V2 store

**Files:**

- Modify: `crates/node/src/proof_history/mod.rs`
- Modify: `crates/node/src/proof_history/storage_init.rs`
- Modify: `crates/node/src/proof_history/sidecar.rs`

- [ ] Add red tests in `storage_init.rs`:

  - `empty_v1_storage_allows_v2_cutover`
  - `completed_v1_storage_is_refused_with_configured_path`
  - `in_progress_v1_storage_is_refused_with_configured_path`
  - `v1_storage_with_any_window_bound_is_refused`
  - `v1_refusal_does_not_delete_storage`

  Seed in-progress/completed stores with `MdbxProofsStorage` and `OpProofsInitProvider`. Seed a one-sided bound with raw `init_db_for::<_, Tables>` and `ProofWindow`. Every refusal must contain the exact configured path and the instruction `remove the directory or use a fresh path and restart`.

- [ ] Run `cargo test -p alethia-reth-node v1_ -- --nocapture` and confirm the new cases fail because startup currently accepts V1.

- [ ] Implement `refuse_legacy_v1_storage(path: &Path) -> eyre::Result<()>`.

  Open V1, inspect `initial_state_anchor()`, drop the initializer, then inspect earliest/latest independently. Allow only `InitialStateStatus::NotStarted` with both bounds absent. Drop the V1 store before returning.

  ```rust
  let legacy = MdbxProofsStorage::new(path)?;
  let anchor = legacy.initialization_provider()?.initial_state_anchor()?;
  let provider = legacy.provider_ro()?;
  let earliest = provider.get_earliest_block();
  let latest = provider.get_latest_block();
  let populated = !matches!(anchor.status, InitialStateStatus::NotStarted) ||
      !matches!(earliest, Err(OpProofsStorageError::NoBlocksFound)) ||
      !matches!(latest, Err(OpProofsStorageError::NoBlocksFound));
  drop(provider);
  drop(legacy);
  ```

- [ ] Change the production aliases and constructors to V2.

  ```rust
  pub type ProofHistoryStorage =
      OpProofsStorage<Arc<MdbxProofsStorageV2>>;
  ```

  Call the V1 probe before `MdbxProofsStorageV2::new`. Change the metrics task to accept `Arc<MdbxProofsStorageV2>`. Remove `migrate_legacy_proof_history_storage`, its startup call, and its V1 latest-row migration tests.

- [ ] Run `cargo test -p alethia-reth-node v1_ -- --nocapture` and the existing proof-history suite. Expect all tests to pass with fixtures converted to V2.

- [ ] Commit as `refactor(proofs): cut over proof history to V2`.

## Task 2: Replace custom initialization with upstream init and backfill

**Files:**

- Delete: `crates/node/src/proof_history/init.rs`
- Modify: `crates/node/src/proof_history/mod.rs`
- Rewrite: `crates/node/src/proof_history/storage_init.rs`
- Modify: `crates/node/src/proof_history/sidecar.rs`

- [ ] Add red tests:

  - `fresh_v2_initialization_records_exact_persisted_head`
  - `interrupted_v2_initialization_resumes_at_same_source_anchor`
  - `interrupted_v2_initialization_refuses_moved_source_anchor`
  - `finalized_window_waits_without_persisted_finality`
  - `finalized_window_waits_when_execution_is_below_target`
  - `finalized_window_initializes_at_executed_head_and_backfills_to_target`
  - `interrupted_backfill_resumes_from_committed_earliest`

  Use a real test provider factory and V2 store. For the full backfill case, create blocks through height 5, persist finalized 4, configure window 2, and assert `[earliest, latest] == [2, 5]`.

- [ ] Run `cargo test -p alethia-reth-node initialization_ -- --nocapture`. Confirm failures expose the custom reverse-overlay path and moved-anchor acceptance.

- [ ] Implement exact-anchor current-state initialization with one pinned main-DB provider.

  ```rust
  let db_provider = provider.database_provider_ro()?;
  let number = db_provider.best_block_number()?;
  let header = db_provider
      .sealed_header(number)?
      .ok_or_else(|| eyre!("missing persisted header {number}"))?;
  let anchor = header.num_hash();
  let layout = if db_provider.cached_storage_settings().is_v2() {
      RethTrieStorageLayout::Packed
  } else {
      RethTrieStorageLayout::Legacy
  };
  validate_in_progress_anchor(&storage, anchor, storage_path)?;
  InitializationJob::new(storage, db_provider.into_tx(), layout)
      .run(anchor.number, anchor.hash)?;
  ```

  `validate_in_progress_anchor` must reject a moved source head with the proof path and wipe/fresh-path guidance before calling upstream.

- [ ] Implement finalized-window preparation.

  From one pinned provider read persisted best and finalized. Wait when finalized is absent or best is below `finalized.saturating_sub(window)`. Initialize at persisted best. Then open a fresh pinned provider, require the completed anchor/latest to remain canonical, recompute the target, and run:

  ```rust
  BackfillJob::new(db_provider, raw_v2_storage).run(target_earliest)?;
  ```

  Use upstream's default batch size 25. Run this preparation for already initialized V2 so a partially committed backfill resumes.

- [ ] Delete the custom `ProofHistoryInitializationJob`, historical reverse-changeset overlay, sidecar metadata file, encode/decode helpers, and associated tests. Remove `mod init`.

- [ ] Split sidecar startup into `prepare_storage_or_wait`, then `reconcile_or_wait`; no initialization branch may set readiness directly.

- [ ] Run `cargo test -p alethia-reth-node initialization_ -- --nocapture` and `cargo test -p alethia-reth-node proof_history --lib`. Expect pass.

- [ ] Commit as `refactor(proofs): reuse upstream initialization jobs`.

## Task 3: Reconcile the committed range from pinned snapshots

**Files:**

- Modify: `crates/node/src/proof_history/sidecar.rs`

- [ ] Add red tests:

  - `startup_action_ready_when_both_window_endpoints_are_canonical`
  - `startup_action_refuses_noncanonical_earliest`
  - `startup_action_waits_when_latest_is_above_snapshot_best`
  - `startup_action_unwinds_when_latest_mismatches`
  - `startup_reconciliation_opens_one_canonical_snapshot`
  - `startup_unwind_uses_child_hash_from_reconciliation_snapshot`
  - `post_init_reconciliation_detects_noncanonical_anchor_before_readiness`

- [ ] Run `cargo test -p alethia-reth-node startup_ -- --nocapture` and confirm current mixed provider reads fail the counting/snapshot cases.

- [ ] Refactor reconciliation to read `get_proof_window()` once and open exactly one main `database_provider_ro()`.

  The pure decision receives the proof range, snapshot best, canonical hashes for both endpoints, and a pre-resolved unwind marker. Carry the marker in the action:

  ```rust
  enum ProofHistoryStartupAction {
      Uninitialized,
      Ready,
      WaitForCanonicalLatest { latest: u64, canonical_best: u64 },
      Unwind { first_removed: BlockWithParent },
  }
  ```

  Resolve the child at `earliest + 1` from the same snapshot and require its parent hash to equal the stored earliest hash before constructing `BlockWithParent`.

- [ ] Keep startup waiting when proof latest is above persisted canonical best after an ungraceful restart. Treat a noncanonical earliest or a noncanonical one-block initialization anchor as fatal with wipe/fresh-path guidance. Never set readiness until the fresh post-init reconciliation returns `Ready`.

- [ ] Run `cargo test -p alethia-reth-node startup_ -- --nocapture` and `cargo test -p alethia-reth-node proof_history --lib`. Expect pass.

- [ ] Commit as `fix(proofs): reconcile storage from pinned snapshots`.

## Task 4: Add one finality-aware prune transaction per tick

**Files:**

- Create: `crates/node/src/proof_history/prune.rs`
- Modify: `crates/node/src/proof_history/mod.rs`
- Modify: `crates/node/src/proof_history/sidecar.rs`

- [ ] Add red tests in `prune.rs`:

  - `prune_without_persisted_finality_is_noop`
  - `prune_target_uses_finalized_head_not_latest`
  - `one_prune_tick_advances_earliest_by_at_most_fifty`
  - `prune_does_not_reach_or_cross_latest`
  - `noncanonical_earliest_aborts_without_commit`
  - `noncanonical_latest_aborts_without_commit`
  - `missing_target_header_aborts_without_commit`
  - `broken_target_parent_continuity_aborts_without_commit`
  - `prune_uses_one_pinned_canonical_snapshot`

  Use real V2 MDBX and a real test provider. After every mismatch, reopen proof RO and assert earliest is unchanged.

- [ ] Run `cargo test -p alethia-reth-node prune_ -- --nocapture`. Confirm stock latest-relative pruning fails finality and snapshot expectations.

- [ ] Implement a documented `FinalityPruneOutcome` and `FinalityProofHistoryPruner::run_once`.

  Open proof RW first and read the window from that transaction. Open one main RO snapshot, read persisted finalized, and validate stored earliest/latest hashes. Compute:

  ```rust
  let desired = finalized.number.saturating_sub(window);
  if desired <= proof_window.earliest.number || desired >= proof_window.latest.number {
      return Ok(FinalityPruneOutcome::UpToDate);
  }
  let target = proof_window.earliest.number.saturating_add(50).min(desired);
  let parent = canonical_header(target - 1)?;
  let target_header = canonical_header(target)?;
  if target_header.parent_hash() != parent.hash() {
      return Ok(FinalityPruneOutcome::CanonicalMismatch);
  }
  proof_rw.prune_earliest_state(BlockWithParent::new(
      parent.hash(),
      BlockNumHash::new(target, target_header.hash()),
  ))?;
  proof_rw.commit()?;
  ```

  Missing finality/header or any canonical mismatch drops RW without commit and retries on a later tick. Do not loop through multiple 50-block batches.

- [ ] Construct the stock engine pruner with `u64::MAX` in the later engine factory. Replace the sidecar's stock pruner task with `run_once`. Recompute startup prune safety from the finalized target, capped below proof latest.

- [ ] Run `cargo test -p alethia-reth-node prune_ -- --nocapture` and the proof-history suite. Expect pass.

- [ ] Commit as `fix(proofs): prune only behind persisted finality`.

## Task 5: Wrap upstream `EngineHandle` and prove its storage behavior

**Files:**

- Create: `crates/node/src/proof_history/engine.rs`
- Modify: `crates/node/src/proof_history/mod.rs`
- Modify: `crates/node/Cargo.toml`

- [ ] Add a test-only `reth-optimism-trie` dependency with feature `test-utils`, then add red adapter tests:

  - `engine_executes_and_persists_an_empty_block`
  - `engine_rejects_a_wrong_state_root`
  - `engine_indexes_precomputed_updates`
  - `engine_unwinds_to_the_retained_earliest`
  - `engine_reorgs_at_the_retained_earliest_with_non_empty_updates`

- [ ] Define the narrow seam and forward it to upstream.

  ```rust
  pub(super) type ReorgBlockUpdates = Vec<(
      BlockWithParent,
      Arc<TrieUpdatesSorted>,
      Arc<HashedPostStateSorted>,
  )>;

  pub(super) trait ProofHistoryEngine<Block>: Send + 'static
  where
      Block: reth_primitives_traits::Block,
  {
      fn execute_block(&self, block: &RecoveredBlock<Block>) -> eyre::Result<()>;
      fn index_block(
          &self,
          block: BlockWithParent,
          trie_updates: TrieUpdatesSorted,
          post_state: HashedPostStateSorted,
      ) -> eyre::Result<()>;
      fn reorg(&self, updates: ReorgBlockUpdates) -> eyre::Result<()>;
      fn unwind(&self, from: BlockWithParent) -> eyre::Result<()>;
      fn sync_to(&self, target: u64) -> eyre::Result<()>;
  }
  ```

  Spawn production with `EngineHandle::spawn(evm, provider, storage.clone(), OpProofStoragePruner::new(storage, provider, u64::MAX))`. Tests may use low thresholds or `flush()` before inspecting MDBX.

- [ ] Run `cargo test -p alethia-reth-node proof_history::engine::tests --all-features`. Expect pass.

- [ ] Commit as `refactor(proofs): add upstream engine adapter`.

## Task 6: Route exact notification blocks and verification-aware reorgs

**Files:**

- Modify: `crates/node/src/proof_history/sidecar.rs`

- [ ] Add a `RecordingEngine` test double and red commit tests:

  - `commit_indexes_precomputed_notification_data`
  - `commit_executes_at_verification_height`
  - `commit_executes_when_trie_data_is_missing`
  - `commit_execution_uses_exact_notification_block`
  - `overlapping_commit_processes_only_uncovered_suffix`
  - `duplicate_commit_is_a_noop`
  - `gapped_commit_requests_engine_sync`

- [ ] Implement `process_notification_block`. Obtain the recovered block from `chain.blocks()`, never provider lookup by number. Index precomputed data only when verification is not due; otherwise execute the exact notification block.

- [ ] Add red reorg/revert tests:

  - `precomputed_reorg_uses_single_engine_reorg`
  - `verification_height_reorg_unwinds_then_replays_in_order`
  - `missing_trie_data_reorg_unwinds_then_replays_in_order`
  - `reorg_replay_uses_exact_notification_blocks`
  - `reorg_rejects_mismatched_fork_blocks`
  - `reorg_rejects_non_contiguous_replacement`
  - `reorg_rejects_broken_replacement_parent_link`
  - `reorg_with_common_ancestor_at_earliest_is_allowed`
  - `reorg_replacing_earliest_fails_closed`
  - `revert_above_earliest_unwinds`
  - `revert_replacing_earliest_fails_closed`
  - `engine_failure_after_unwind_leaves_readiness_false`
  - `successful_reorg_reconciles_before_becoming_ready`
  - `commit_engine_failure_clears_readiness`

- [ ] Validate old/new fork equality, first replacement parent, consecutive numbers, and every replacement parent link before action. Clear readiness before every reorg/revert and before propagating an engine error.

- [ ] Route reorgs:

  ```rust
  let can_reorg_directly = replacement_blocks.iter().all(has_trie_data) &&
      replacement_blocks.iter().all(|block| !verification_due(block.number()));
  if can_reorg_directly {
      engine.reorg(precomputed_updates)?;
  } else {
      engine.unwind(old.first().block_with_parent())?;
      for number in replacement_numbers_in_order {
          self.process_notification_block(number, new, engine)?;
      }
  }
  ```

  An empty replacement always unwinds. After action, reconcile committed storage, then set ready. Errors leave not-ready.

- [ ] Add critical duplicate regressions:

  - `duplicate_reorg_covered_by_committed_canonical_suffix_is_skipped`
  - `duplicate_reorg_at_committed_common_ancestor_is_forwarded`
  - `duplicate_revert_at_committed_common_ancestor_is_forwarded`

  Skip a reorg only when the committed canonical proof suffix reaches at least the replacement tip. A committed latest equal to the common ancestor does not consume the notification; forward reorg/unwind to clear a possible private old-fork buffer. Forward safe retained-range reverts unconditionally.

- [ ] Run the commit/reorg/revert/duplicate test filters and the complete proof-history suite. Expect pass.

- [ ] Commit as `fix(proofs): make engine reorg handling correct`.

## Task 7: Own one engine, restart it on lag, and remove manual sync

**Files:**

- Modify: `crates/node/src/proof_history/sidecar.rs`
- Modify: `crates/node/src/proof_history/storage_init.rs`
- Delete: `crates/node/src/proof_history/live.rs`
- Modify: `crates/node/src/proof_history/mod.rs`

- [ ] Add an injected engine factory and ordered-event test doubles. Add red tests:

  - `periodic_poll_syncs_to_persisted_executed_head`
  - `periodic_poll_does_not_use_in_memory_canonical_tip`
  - `lag_recovery_drops_old_engine_before_reconciliation`
  - `lag_recovery_spawns_a_new_engine_generation`
  - `lag_recovery_syncs_new_engine_to_persisted_head`
  - `lag_recovery_restores_readiness_only_after_reconciliation_and_sync`
  - `lag_reconciliation_failure_leaves_readiness_false_and_no_engine`
  - `lag_engine_spawn_failure_leaves_readiness_false`
  - `lag_initial_sync_failure_leaves_readiness_false`
  - `shutdown_drops_the_sole_engine_handle`

- [ ] Keep the engine in a non-cloneable owned slot. Startup order is reconcile, spawn, sync to persisted best, ready, and spawn the external pruner once. Add a five-second interval with `MissedTickBehavior::Delay`; every tick rereads persisted best and calls `sync_to`.

- [ ] Implement lag recovery in this exact order:

  1. Replace the broadcast receiver.
  2. Set readiness false.
  3. Take and drop the sole engine handle.
  4. Reconcile committed V2 against a fresh canonical snapshot without startup's wait-on-stale-engine behavior.
  5. Spawn a new engine.
  6. Sync it to persisted executed head.
  7. Set readiness true.

  Any failure leaves no engine and readiness false.

- [ ] Delete `LiveTrieCollector`, `spawn_sync_task`, `sync_loop`, `sleep_or_shutdown`, `process_batch`, `proof_history_sync_target`, sync wake/batch constants, and the old Tokio writer lock. Preserve meaningful execute/index/reorg tests in `engine.rs` and sidecar tests.

- [ ] Run:

  ```text
  cargo test -p alethia-reth-node proof_history --all-features
  rg -n "LiveTrieCollector|spawn_sync_task|sync_loop|process_batch|proof_history_sync_target|sync_wake" crates/node/src
  ```

  Expect all tests to pass and `rg` to return no matches.

- [ ] Commit as `refactor(proofs): replace manual collector with engine`.

## Task 8: Resolve exact RPC state and guard one proof snapshot

**Files:**

- Rewrite: `crates/rpc/src/proof_state.rs`
- Modify: `crates/rpc/Cargo.toml`

- [ ] Add red exact-resolution tests:

  - `exact_hash_match_is_accepted`
  - `changed_or_missing_canonical_hash_returns_retry_error`
  - `postvalidation_checks_snapshot_then_state_then_target`
  - `reorg_above_snapshot_latest_does_not_invalidate_guard`
  - `latest_resolution_retains_exact_num_hash`
  - `pending_resolution_returns_pending_variant`

- [ ] Introduce exact identity types and deterministic transient error text.

  ```rust
  pub(crate) enum ResolvedBlockState {
      Pending(StateProviderBox),
      Canonical { block: BlockNumHash, state: StateProviderBox },
  }

  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  pub(crate) struct ProofStateGuard {
      state_block: BlockNumHash,
      snapshot_latest: Option<BlockNumHash>,
  }
  ```

  Canonical resolution is `sealed_header_by_id` once, `state_by_block_hash(hash)`, with canonical hash validation before and after state loading. Pending branches first and uses normal Reth pending state without opening proof storage. Mismatches return `canonical state changed; retry` with no automatic retry.

- [ ] Add red snapshot tests:

  - `pending_bypasses_empty_proof_store_and_tip_lookup`
  - `covered_request_captures_exact_snapshot_latest`
  - `canonical_fallback_guard_has_no_snapshot_hash`
  - `covered_noncanonical_snapshot_fails_closed`
  - `old_v2_snapshot_fails_after_live_store_reorg`

  The last test must keep an old V2 RO/state provider alive across A5 to B5 replacement, then prove postvalidation rejects the old A5 snapshot while a reorg only above the captured latest passes.

- [ ] Implement overlay selection from one `get_proof_window()` call. Before overlay, check readiness, numeric coverage, exact requested canonical hash, and exact snapshot-latest canonical hash. Near-tip fallback keeps exact canonical state and a guard without snapshot latest. Deep misses remain refused.

- [ ] Implement postvalidation in order: captured snapshot latest, state block, then optional debug target. The target check is the final linearization point.

- [ ] Run `cargo test -p alethia-reth-rpc proof_state -- --nocapture`. Expect pass.

- [ ] Commit as `fix(rpc): pin proof state to an exact branch`.

## Task 9: Reuse Reth's proof permit and blocking runtime

**Files:**

- Modify: `crates/rpc/src/proof_state.rs`
- Modify: `crates/rpc/src/eth/proofs.rs`
- Modify: `crates/rpc/src/debug.rs`
- Modify: `crates/rpc/Cargo.toml`

- [ ] Add `aborted_rpc_future_keeps_reth_permit_until_blocking_task_finishes`. Build a real test Eth API with one proof permit, abort the waiting RPC future after its closure starts, and prove a second acquisition blocks until the first non-abortable closure exits.

- [ ] Add the shared helper:

  ```rust
  pub(crate) async fn run_proof_task<Eth, F, T>(
      eth_api: &Eth,
      task: F,
  ) -> Result<T, Eth::Error>
  where
      Eth: FullEthApi + Send + Sync + 'static,
      F: FnOnce() -> Result<T, Eth::Error> + Send + 'static,
      T: Send + 'static,
  {
      let permit = eth_api.acquire_owned_tracing().await
          .map_err(|_| EthApiError::InternalEthError)?;
      eth_api.spawn_blocking_io(move |_| {
          let _permit = permit;
          task()
      }).await
  }
  ```

  Import `SpawnBlocking`. Keep the owned permit inside the non-abortable closure.

- [ ] Delete `flatten_blocking_task`, hardcoded proof/witness semaphore limits, semaphore fields/imports, and raw `tokio::task::spawn_blocking` calls.

- [ ] Run the cancellation test and `cargo test -p alethia-reth-rpc --lib`. Expect pass.

- [ ] Commit as `refactor(rpc): reuse Reth proof execution limits`.

## Task 10: Postvalidate `eth_getProof` and both debug witness paths

**Files:**

- Modify: `crates/rpc/src/eth/proofs.rs`
- Modify: `crates/rpc/src/debug.rs`

- [ ] Add red eth tests:

  - `get_proof_postvalidates_after_trie_walk`
  - `pending_get_proof_does_not_open_proof_history`

  Resolve exact state, run the complete trie proof inside `run_proof_task`, then call `validate_after_work(guard, None)` before converting to EIP-1186.

- [ ] Add red debug tests:

  - `witness_target_reorg_is_rejected_after_parent_work`
  - `tx_list_witness_target_reorg_is_rejected_after_parent_work`
  - `noncanonical_by_hash_target_is_rejected_before_execution`

- [ ] For both witness paths, capture target identity from the original recovered canonical block, ensure it is canonical before work, resolve the exact parent by parent hash, and require the returned parent identity to equal `(target.number - 1, target.parent_hash)`. Keep tx-list target identity from the original block, not the synthetic transaction-replaced block.

- [ ] Run all execution, trie reads, and witness assembly inside `run_proof_task`. After the complete witness is assembled, call `validate_after_work(parent_guard, Some(target))`, with target validated last.

- [ ] Run `cargo test -p alethia-reth-rpc proof --lib`, `cargo test -p alethia-reth-rpc witness --lib`, and the entire RPC test suite. Expect pass.

- [ ] Commit as `fix(rpc): reject proofs across canonical reorgs`.

## Task 11: Whole-branch review, verification, and publication cleanup

**Files:**

- Review every changed production/test file.
- Delete: `docs/superpowers/`

- [ ] Run a task-scoped code review after every task and resolve all Critical/Important findings before moving on.

- [ ] Run a final whole-branch review against `origin/feat/reth-v2.4.0-jit`. Resolve all Critical/Important findings with one focused fix pass, then re-review.

- [ ] Run fresh verification in this order:

  ```text
  cargo test -p alethia-reth-node proof_history --all-features
  cargo test -p alethia-reth-rpc --lib
  just fmt
  just clippy
  just test
  git diff --check origin/feat/reth-v2.4.0-jit...HEAD
  ```

  Record exact outcomes for the PR body.

- [ ] Remove the entire temporary `docs/superpowers` tree with `apply_patch`. Confirm:

  ```text
  test ! -e docs/superpowers
  git diff --name-only origin/feat/reth-v2.4.0-jit...HEAD | rg '^docs/superpowers/'
  ```

  The first command must succeed and the second must return no matches.

- [ ] Ensure the final commit series contains only intentional production/tests changes. Commit cleanup/final fixes with a Conventional Commit subject no longer than 72 characters.

- [ ] Push `codex/proof-history-correctness-v2` and open a draft PR with base `feat/reth-v2.4.0-jit`. The PR body must summarize V2 cutover, upstream reuse, reorg/lag semantics, finality-safe pruning, RPC snapshot guards, and every verification command.
