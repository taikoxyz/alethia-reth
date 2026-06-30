# Proof-history local-head backfill + RPC fallback — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make proof-history backfill up to the node's locally executed head regardless of the live canonical-notification stream, and make `eth_getProof` fall back to canonical state (instead of erroring) while proof-history is catching up.

**Architecture:** Two independent changes. (1) In the proof-history sidecar `sync_loop`, derive the backfill target from the on-disk executed head rather than the last notified canonical tip, and poll that head on a timer instead of parking indefinitely on the notification channel. (2) In the proof-history RPC state-provider factory, when the requested block is outside the retained `[earliest, latest]` range, return the already-fetched canonical state provider (with a WARN) instead of `StateForNumberNotFound`.

**Tech Stack:** Rust (toolchain pinned via `rust-toolchain.toml`/`justfile`), Reth v2.0.0 APIs, `tokio`, `tracing`, `eyre`, `cargo nextest`.

## Global Constraints

- Toolchain and commands run through the pinned toolchain; verify with `just fmt`, `just clippy`, `just test`. Targeted unit runs use `cargo test -p <package> <filter>`.
- `just clippy` treats warnings as errors — code must be clippy-clean.
- Crate packages: `alethia-reth-node` (path `crates/node`), `alethia-reth-rpc` (path `crates/rpc`).
- Conventional-commit messages. End every commit message with the trailer:
  `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`
- Out of scope (do NOT attempt here): optimizing the re-execution/state-root catch-up cost; the upstream consensus-feed (driver→engine API) stall; the deeper refactor that drops the notified-target value entirely.
- Branch: `fix/proof-history-backfill-local-head` (already created).

---

### Task 1: Pure backfill-target decision that tracks the executed head

**Files:**
- Modify: `crates/node/src/proof_history/storage_init.rs` (add function after `proof_history_backfill_target`, ~line 186; add tests in the existing `#[cfg(test)] mod tests`)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: existing `proof_history_backfill_target(latest_stored: u64, requested_target: u64, executed_head: u64) -> Option<u64>` (unchanged).
- Produces: `proof_history_sync_target(latest_stored: u64, notified_target: u64, executed_head: u64) -> Option<u64>` — `Some(target)` means backfill toward `target` (never above `executed_head`); `None` means caught up / nothing locally executed to add. Used by Task 2.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `mod tests` in `crates/node/src/proof_history/storage_init.rs`:

```rust
#[test]
fn proof_history_sync_target_tracks_executed_head_when_notification_is_stale() {
    // Incident: the last notified canonical tip is frozen at the pre-stall height, but the node
    // pipeline-synced ahead. Proof-history must still backfill up to the executed head.
    assert_eq!(proof_history_sync_target(8_108_771, 8_108_771, 8_110_008), Some(8_110_008));
}

#[test]
fn proof_history_sync_target_waits_when_caught_up_to_executed_head() {
    assert_eq!(proof_history_sync_target(8_110_008, 8_110_008, 8_110_008), None);
}

#[test]
fn proof_history_sync_target_never_runs_ahead_of_executed_head() {
    // Notified tip is ahead of what is executed locally; do not backfill unexecuted blocks.
    assert_eq!(proof_history_sync_target(100, 200, 100), None);
}

#[test]
fn proof_history_sync_target_backfills_to_executed_head_below_notified_tip() {
    // Executed head sits between latest_stored and the notified tip.
    assert_eq!(proof_history_sync_target(100, 200, 150), Some(150));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alethia-reth-node proof_history_sync_target`
Expected: FAIL to compile — `cannot find function proof_history_sync_target in this scope`.

- [ ] **Step 3: Implement the function**

Insert immediately after `proof_history_backfill_target` (after its closing brace, ~line 186) in `crates/node/src/proof_history/storage_init.rs`:

```rust
/// Returns the next block proof-history should backfill toward, tracking the node's locally
/// executed head.
///
/// A target derived only from the last canonical notification stalls when the node advances via
/// pipeline/staged sync without delivering a live notification (e.g. after a restart) or when the
/// consensus feed is down: `notified_target` then lags `executed_head`, yet proof-history must still
/// index everything the node has executed. Folding in `executed_head` fixes that, while the inner
/// `proof_history_backfill_target` still clamps the result to `executed_head` so re-execution only
/// ever reads persisted blocks. `None` means caught up (nothing new to backfill).
pub(super) fn proof_history_sync_target(
    latest_stored: u64,
    notified_target: u64,
    executed_head: u64,
) -> Option<u64> {
    let effective_target = notified_target.max(executed_head);
    proof_history_backfill_target(latest_stored, effective_target, executed_head)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p alethia-reth-node proof_history_sync_target`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/node/src/proof_history/storage_init.rs
git commit -m "fix(proof-history): add sync target that tracks executed head" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Drive `sync_loop` from the executed head and poll for staged-sync gaps

**Files:**
- Modify: `crates/node/src/proof_history/exex.rs`
  - import block (~lines 3-8): swap `proof_history_backfill_target` → `proof_history_sync_target`
  - constants (~after line 82): add `PROOF_HISTORY_HEAD_POLL_INTERVAL`
  - `sync_loop` (~lines 647-731): executed-head source, single decision via `proof_history_sync_target`, poll-or-notify wait, INFO progress log
  - `process_batch` (~lines 735-753): return the last processed block number

**Interfaces:**
- Consumes: `proof_history_sync_target` (Task 1); existing `proof_history_storage_needs_initialization` etc. (unchanged); `DatabaseProviderFactory::database_provider_ro` + `BlockNumReader::best_block_number` (already used in this file at the `finalized_window_initialization_action` site).
- Produces: no new public surface; behavior change only.

- [ ] **Step 1: Update the storage_init import**

In `crates/node/src/proof_history/exex.rs`, change the `use super::storage_init::{...}` block (lines 3-8). Replace the `proof_history_backfill_target,` entry with `proof_history_sync_target,`. Resulting block:

```rust
use super::storage_init::{
    DelayedProofHistoryStart, PROOF_HISTORY_MAX_STARTUP_PRUNE_BLOCKS,
    ProofHistoryInitializationAction, delayed_proof_history_start, finalized_block_number,
    initialize_historical_proof_history_storage, initialize_proof_history_storage,
    proof_history_storage_needs_initialization, proof_history_sync_target,
};
```

- [ ] **Step 2: Add the poll-interval constant**

Insert after `PROOF_HISTORY_DELAYED_START_RETRY_INTERVAL` (after line 82) in `crates/node/src/proof_history/exex.rs`:

```rust
/// Delay between polls of the node's executed head while proof-history is caught up, so a
/// staged-sync gap is backfilled even when no live canonical notification arrives.
const PROOF_HISTORY_HEAD_POLL_INTERVAL: Duration = Duration::from_secs(5);
```

- [ ] **Step 3: Replace the `sync_loop` body**

In `crates/node/src/proof_history/exex.rs`, replace the entire `loop { ... }` inside `sync_loop` (lines 647-731) with:

```rust
        loop {
            let requested_target = *sync_target_rx.borrow_and_update();
            let write_guard = write_lock.lock().await;
            let latest = match storage.provider_ro().and_then(|p| p.get_latest_block_number()) {
                Ok(Some((number, _))) => number,
                Ok(None) => {
                    error!(target: "reth::taiko::proof_history", "proof-history sync loop found no stored blocks; stopping sync loop");
                    return;
                }
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "failed to read proof-history latest block");
                    drop(write_guard);
                    time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
                    continue;
                }
            };

            // Track the node's on-disk executed head, not just the last notified canonical tip.
            // Using the on-disk head (rather than the in-memory tip) guarantees the blocks the
            // backfill re-executes are persisted, and lets proof-history catch up across a
            // staged-sync gap even when no live notification advances `requested_target`.
            let executed_head =
                match provider.database_provider_ro().and_then(|p| p.best_block_number()) {
                    Ok(number) => number,
                    Err(error) => {
                        error!(target: "reth::taiko::proof_history", ?error, "failed to read executed head for proof-history sync");
                        drop(write_guard);
                        time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
                        continue;
                    }
                };

            let Some(target) = proof_history_sync_target(latest, requested_target, executed_head)
            else {
                // Caught up to the locally executed head. Wake on the next live notification (fast
                // path) or after a poll delay, so a staged-sync gap is still picked up with no
                // notifications.
                drop(write_guard);
                tokio::select! {
                    result = sync_target_rx.changed() => {
                        if result.is_err() {
                            debug!(
                                target: "reth::taiko::proof_history",
                                "proof-history sync target sender dropped; stopping sync loop"
                            );
                            return;
                        }
                    }
                    _ = time::sleep(PROOF_HISTORY_HEAD_POLL_INTERVAL) => {}
                }
                continue;
            };

            let batch_provider = provider.clone();
            let batch_storage = storage.clone();
            let batch_evm_config = evm_config.clone();
            // Each block write commits independently; if this batch fails part-way through, the
            // next loop rereads `latest` and resumes after the last committed block.
            let batch_task = task::spawn_blocking(move || {
                let collector_storage = batch_storage.clone();
                let collector = LiveTrieCollector::new(
                    batch_evm_config,
                    batch_provider.clone(),
                    &collector_storage,
                );
                Self::process_batch(
                    latest,
                    target,
                    &batch_provider,
                    &collector,
                    PROOF_HISTORY_SYNC_BATCH_SIZE,
                )
            });
            let batch_result = blocking_join_result(batch_task.await, "proof-history batch worker")
                .and_then(|result| result);
            drop(write_guard);

            match batch_result {
                Ok(backfilled_to) => {
                    info!(
                        target: "reth::taiko::proof_history",
                        backfilled_to,
                        head = executed_head,
                        "proof-history backfill batch committed"
                    );
                }
                Err(error) => {
                    error!(target: "reth::taiko::proof_history", ?error, "proof-history batch processing failed");
                    time::sleep(PROOF_HISTORY_SYNC_IDLE_SLEEP).await;
                }
            }

            task::yield_now().await;
        }
```

- [ ] **Step 4: Make `process_batch` return the last processed block**

In `crates/node/src/proof_history/exex.rs`, change `process_batch` (lines 735-753) signature and final expression to return the batch end:

```rust
    /// Processes a bounded batch of canonical blocks into proof-history storage.
    ///
    /// Returns the highest block number processed in this batch.
    fn process_batch(
        start: u64,
        target: u64,
        provider: &Node::Provider,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        batch_size: usize,
    ) -> eyre::Result<u64> {
        let end = start.saturating_add(batch_size as u64).min(target);
        debug!(target: "reth::taiko::proof_history", start, end, "processing proof-history batch");

        for block_num in (start + 1)..=end {
            let block = provider
                .recovered_block(block_num.into(), TransactionVariant::NoHash)?
                .ok_or_else(|| eyre!("missing block {block_num}"))?;
            collector.execute_and_store_block_updates(&block)?;
        }

        Ok(end)
    }
```

- [ ] **Step 5: Verify it compiles and is clippy-clean**

Run: `cargo clippy -p alethia-reth-node --all-features`
Expected: no warnings/errors. (`proof_history_backfill_target` must NOT be reported as unused — it is still called by `proof_history_sync_target`; if clippy flags an unused import in `exex.rs`, ensure the import swap in Step 1 was applied.)

- [ ] **Step 6: Run the node test suite**

Run: `cargo test -p alethia-reth-node`
Expected: PASS (includes Task 1's `proof_history_sync_target` tests and existing `proof_history_backfill_target`/startup tests).

> **Note on test coverage:** `sync_loop` is a generic async task that cannot be unit-tested without a full node harness; its wait-vs-backfill decision is covered by the pure `proof_history_sync_target` tests (Task 1). This task wires that decision in and is verified by compilation, clippy, and the existing suite.

- [ ] **Step 7: Commit**

```bash
git add crates/node/src/proof_history/exex.rs
git commit -m "fix(proof-history): backfill from executed head with head polling" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: RPC fallback to canonical state when a block is outside the retained range

**Files:**
- Modify: `crates/rpc/src/proof_state.rs` (add `use tracing::warn;`; add `proof_history_covers` free fn; rework the range check in `state_provider`; add `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `OpProofsProviderRO::get_latest_block_number` / `get_earliest_block_number`; the already-fetched `historical_provider: Box<dyn StateProvider>` (from `eth_api.state_at_block_id`).
- Produces: `fn proof_history_covers(block_number: u64, bounds: Option<(u64, u64)>) -> bool` (module-private); behavior change in `ProofHistoryStateProviderFactory::state_provider`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/rpc/src/proof_state.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::proof_history_covers;

    #[test]
    fn covers_block_within_bounds() {
        assert!(proof_history_covers(150, Some((100, 200))));
    }

    #[test]
    fn covers_inclusive_boundaries() {
        assert!(proof_history_covers(100, Some((100, 200))));
        assert!(proof_history_covers(200, Some((100, 200))));
    }

    #[test]
    fn does_not_cover_block_above_latest() {
        // The incident: requested block sits above latest_stored -> fall back to canonical state.
        assert!(!proof_history_covers(8_109_699, Some((7_503_971, 8_108_771))));
    }

    #[test]
    fn does_not_cover_block_below_earliest() {
        assert!(!proof_history_covers(50, Some((100, 200))));
    }

    #[test]
    fn does_not_cover_when_storage_empty() {
        assert!(!proof_history_covers(150, None));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p alethia-reth-rpc proof_history_covers`
Expected: FAIL to compile — `cannot find function proof_history_covers in this scope`.

- [ ] **Step 3: Add the `warn` import and the predicate**

In `crates/rpc/src/proof_state.rs`, add to the imports (after line 9, `use reth_rpc_eth_types::EthApiError;`):

```rust
use tracing::warn;
```

Add this free function above the `ProofHistoryStateProviderFactory` struct (after line 10, before the `#[derive(Debug, Clone)]`):

```rust
/// Returns whether proof-history storage can serve `block_number` given its retained bounds.
///
/// `bounds` is `Some((earliest, latest))` when storage is initialized, `None` when it is empty.
fn proof_history_covers(block_number: u64, bounds: Option<(u64, u64)>) -> bool {
    matches!(bounds, Some((earliest, latest)) if block_number >= earliest && block_number <= latest)
}
```

- [ ] **Step 4: Run the predicate tests to verify they pass**

Run: `cargo test -p alethia-reth-rpc proof_history_covers`
Expected: PASS (5 tests).

- [ ] **Step 5: Wire the fallback into `state_provider`**

In `crates/rpc/src/proof_state.rs`, replace the body from `let provider_ro = ...` through the final `Ok(Box::new(...))` (lines 51-64) with:

```rust
        let provider_ro = self.storage.provider_ro().map_err(ProviderError::from)?;

        let latest = provider_ro.get_latest_block_number().map_err(ProviderError::from)?;
        let earliest = provider_ro.get_earliest_block_number().map_err(ProviderError::from)?;
        let bounds = latest
            .zip(earliest)
            .map(|((latest_number, _), (earliest_number, _))| (earliest_number, latest_number));

        if !proof_history_covers(block_number, bounds) {
            warn!(
                target: "reth::taiko::proof_history",
                block_number,
                ?bounds,
                "proof-history does not cover requested block; serving from canonical state"
            );
            return Ok(historical_provider);
        }

        Ok(Box::new(OpProofsStateProviderRef::new(historical_provider, provider_ro, block_number)))
```

Also update the doc comment on `state_provider` (lines 36-37): replace the line
`/// Returns [`ProviderError::StateForNumberNotFound`] when the requested block is outside the`
`/// retained proof-history window.`
with:
`/// Falls back to the canonical historical state provider (logging a warning) when the requested`
`/// block is outside the retained proof-history window, so a lagging sidecar does not block proofs`
`/// the node can still serve from canonical state.`

- [ ] **Step 6: Verify compile, clippy, and tests**

Run: `cargo clippy -p alethia-reth-rpc --all-features`
Expected: no warnings/errors. (If clippy reports `ProviderError`-related imports or `EthApiError` as now-unused because the `StateForNumberNotFound`/`HeaderNotFound` paths changed, confirm they are still used — `HeaderNotFound` is still used for the `block_number_for_id` lookup above, and `ProviderError` is still used via `map_err(ProviderError::from)`/`ProviderError::other`. Remove an import only if clippy proves it unused.)

Run: `cargo test -p alethia-reth-rpc proof_history_covers`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/rpc/src/proof_state.rs
git commit -m "fix(rpc): fall back to canonical state when proof-history lacks block" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Format**

Run: `just fmt`
Expected: completes; re-run `git diff --stat` and commit any formatting-only changes if produced:

```bash
git add -A
git commit -m "style(proof-history): apply just fmt" -m "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```
(Skip the commit if `git status` is clean.)

- [ ] **Step 2: Clippy across the workspace**

Run: `just clippy`
Expected: no warnings (warnings are errors).

- [ ] **Step 3: Full test suite**

Run: `just test`
Expected: PASS (whole workspace, all features).

---

## Self-Review

**1. Spec coverage**
- Spec "Component 1 — backfill driven by the on-disk head": Task 1 (decision fn) + Task 2 (on-disk `database_provider_ro().best_block_number()`, `effective_target = max`, poll-or-notify wait, `PROOF_HISTORY_HEAD_POLL_INTERVAL`). ✓
- Spec "Component 2 — RPC fallback": Task 3 (`proof_history_covers`, return `historical_provider`, WARN). ✓
- Spec "Observability — INFO after each committed batch": Task 2 Step 3-4 (`process_batch` returns end; `info!(backfilled_to, head, …)`). ✓
- Spec "Testing": Task 1 pure tests incl. incident case; Task 3 predicate tests incl. incident case; `just fmt/clippy/test` in Task 4. ✓
- Spec "Edge cases" (reorg/startup/empty/tip/below-earliest): preserved — startup reconciliation, `found no stored blocks`, and reorg/revert notification handlers are untouched; below-earliest falls through to the canonical provider's own error. ✓
- Spec non-goals untouched (catch-up cost, consensus-feed stall, Approach-3 refactor). ✓

**2. Placeholder scan:** No TBD/TODO; every code step shows complete code; every run step shows command + expected result. ✓

**3. Type consistency:** `proof_history_sync_target(u64,u64,u64) -> Option<u64>` defined in Task 1, called identically in Task 2. `process_batch` return type changed to `eyre::Result<u64>` and consumed as `Ok(backfilled_to)` in Task 2. `proof_history_covers(u64, Option<(u64,u64)>) -> bool` defined and used consistently in Task 3; `bounds` built as `(earliest, latest)` matching the predicate's `Some((earliest, latest))`. ✓
