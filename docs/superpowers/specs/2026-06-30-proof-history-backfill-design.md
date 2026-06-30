# Proof-history: track the local canonical head (backfill + RPC fallback)

- **Date:** 2026-06-30
- **Status:** Approved design, pending implementation plan
- **Crates touched:** `crates/node` (`proof_history/exex.rs`), `crates/rpc` (`proof_state.rs`)

## Background

Proof-history maintains a separate MDBX store of historical trie nodes/hashed leaves so
`eth_getProof` / debug witness RPCs can serve proofs for blocks deeper than reth's normal
state retention. It runs as a sidecar (post-#174) that:

- subscribes to canonical-state notifications,
- on each new tip either applies the notification's trie data directly (near tip) or hands a
  target to a background **sync loop** that backfills by re-executing blocks
  (`execute_and_store_block_updates`),
- serves RPC reads through `ProofHistoryStateProviderFactory`, which gates every request to
  proof-history's own stored range `[earliest_stored, latest_stored]`.

## The incident (root cause)

On mainnet (`l2-node-reth-go-driver-0`), `eth_getProof(addr, [], 0x7bbe83)` (block 8109699)
returned `-32603 "no state found for block number 8109699"` while the chain tip was 8110008.
Disabling proof-history made the call succeed.

Established from Cloud Logging + code:

1. The taiko-client driver → reth engine-API **consensus feed stalled on 06-24 07:09**, freezing
   the node at block 8108771 for ~5 days. Proof-history's last successful store was 8108771
   (it correctly had nothing further to index).
2. A **restart on 06-29 13:39** pipeline-synced the node forward **8108771 → 8110008** (the blocks
   are now present and executed locally on disk).
3. Proof-history **did not backfill the gap**: `latest_stored` stayed at 8108771. So every
   `eth_getProof` for a block above 8108771 — including 8109699, which the node had full
   canonical state for — was rejected with the misleading "no state found".

### Why the backfill stalled (precise)

In `sync_loop` ([`crates/node/src/proof_history/exex.rs`](../../../crates/node/src/proof_history/exex.rs)):

- The backfill target comes **only** from the watch channel `sync_target_rx`
  (`let requested_target = *sync_target_rx.borrow_and_update();`), which is fed **only** by live
  canonical notifications via `handle_chain_committed`.
- When `latest >= requested_target`, the loop blocks **indefinitely** on
  `sync_target_rx.changed().await` — it never re-reads the node's executed head.
- `proof_history_backfill_target(latest, requested_target, executed_head)` returns
  `min(requested_target, executed_head)`, so even past the guard the stale notified value caps the
  backfill.

At sidecar startup the initial target is `best_block_number()` captured *before* the pipeline sync
(8108771). The pipeline sync then advanced the node to 8110008 via staged sync, which delivered no
canonical notification the loop acted on, and with the consensus feed dead no further notifications
arrived. The target stayed pinned at 8108771 == `latest_stored`, so the loop parked forever even
though the node's on-disk head was 8110008.

**Core defect:** proof-history's catch-up is hostage to the live notification stream. It must
instead be driven by what the node has executed locally.

## Goals

1. Proof-history backfills up to the node's **local canonical head**, independent of whether any
   live canonical notification arrives (so a restart-over-a-gap, or a dead consensus feed, still
   results in a fully-indexed proof-history up to local head).
2. `eth_getProof` returns a usable answer while proof-history is legitimately catching up, instead
   of a misleading "no state found".
3. The lag is observable, so a future stall is detectable rather than silent.

## Non-goals (follow-ups)

- Making the re-execution catch-up path cheaper / bounding its memory (the
  "Attempt to calculate state root for an old block might result in OOM" path is pre-existing; this
  change makes catch-up *run*, not run *cheaper*).
- Fixing the upstream consensus-feed stall (driver → engine API) — the incident trigger, tracked
  separately.
- The deeper refactor (make the on-disk head the sole authoritative target and demote the watch
  channel to a valueless wake-ping) — noted as a future cleanup.

## Design

### Component 1 — Backfill driven by the on-disk head (`sync_loop`)

Three changes, localized to `sync_loop`; the pure helper `proof_history_backfill_target` keeps its
signature and body (fix is at the call site), so its existing unit tests stay valid.

1. **Read the executed head from disk, before the caught-up guard.** Replace the in-memory
   `provider.best_block_number()` with the on-disk head
   `provider.database_provider_ro()?.best_block_number()?`, and compute it above the caught-up
   check. Rationale: backfill re-executes blocks via `recovered_block(..)`, which requires them
   persisted; the in-memory tip can outrun disk by `engine.persistence-threshold` (the same
   reasoning already documented in `finalized_window_initialization_action`). The required
   `DatabaseProviderFactory` bounds are already present on `Node::Provider`.

2. **Make the executed head the effective target.** Compute
   `effective_target = requested_target.max(executed_head)` and pass it as the `requested_target`
   argument to `proof_history_backfill_target(latest, effective_target, executed_head)`. Since
   `min(effective_target, executed_head) == executed_head`, the backfill now targets
   "everything executed locally"; a stale notified value can no longer cap it.

3. **Poll instead of parking when caught up.** Replace the unconditional
   `sync_target_rx.changed().await` with a `tokio::select!` over:
   - `sync_target_rx.changed()` — fast-wake on a live notification (returns/stops loop if the
     sender is dropped, as today), and
   - `time::sleep(PROOF_HISTORY_HEAD_POLL_INTERVAL)` — new constant, 5s.

   On either wake the loop re-reads the on-disk head and resumes backfill if it advanced. The
   write-lock is dropped before waiting (existing lock discipline preserved).

Result on the incident: after the pipeline sync to 8110008, the next 5s poll observes on-disk head
8110008 > `latest_stored` 8108771 and backfills 8108772→8110008 with no notification required.

### Component 2 — RPC fallback to canonical state (`proof_state.rs`)

`ProofHistoryStateProviderFactory::state_provider` already fetches
`historical_provider = eth_api.state_at_block_id(block_id)` before the range check. Change the
out-of-range branch (and the empty-storage branch) so that, instead of returning
`Err(ProviderError::StateForNumberNotFound)`, it:

- emits a `WARN` (`block_number`, `earliest_block_number`, `latest_block_number`) —
  "proof-history does not cover requested block; serving from canonical state", and
- returns the already-fetched `historical_provider` (a plain canonical-state provider).

For recent blocks above `latest_stored` the node has the state → valid proof. For blocks below
`earliest_stored` the canonical provider returns its own not-found error. The WARN keeps the lag
visible so a real stall is not silently masked.

### Observability

Today `process_batch` only `debug!`-logs the batch *start* ("processing proof-history batch") and
there is no committed-progress signal at `INFO`. Add an `INFO` log after each committed batch:
`proof-history backfilled latest=<N> head=<M>`. A persistent gap between `latest` and `head` is then
directly visible and alertable — the signal that was missing during the incident.

## Error handling / edge cases

- **Reorgs:** the on-disk canonical head only reflects committed canonical blocks; live reorgs are
  still handled on the notification path (`handle_chain_reorged` / `handle_chain_reverted`), and the
  earliest-anchor guards (`ensure_canonical_update_above_earliest`) are untouched.
- **Startup ordering:** the poll only *adds* a catch-up trigger; `try_start` / `reconcile_or_wait`
  reconciliation runs first, unchanged.
- **Empty / uninitialized storage:** the `found no stored blocks` path and initialization gating are
  unchanged — backfill still requires initialization first.
- **At the tip:** when `latest_stored == head`, the poll finds no work and re-waits.
- **RPC below earliest:** falls through to the canonical provider, which errors if the node lacks
  that deep history (acceptable — it is genuinely unavailable).

## Testing

- **Pure helper:** keep existing `proof_history_backfill_target` tests; add a case capturing the
  incident via the new call-site semantics — `latest=8108771, notified=8108771, head=8110008` →
  effective target 8110008 → backfill to 8110008. If the loop's wait-vs-backfill decision is hard to
  exercise through the generic loop, extract that choice into a small pure function and unit-test it
  exhaustively, keeping the loop thin.
- **RPC (`proof_state.rs`):** block above `latest_stored` returns the canonical provider (no error);
  block within range returns the overlay provider; empty storage falls back to canonical.
- **Gates:** `just test`, `just clippy`, `just fmt`.

## Affected files

- `crates/node/src/proof_history/exex.rs` — `sync_loop` (head source, effective target, poll-wake),
  new `PROOF_HISTORY_HEAD_POLL_INTERVAL`, INFO progress log.
- `crates/rpc/src/proof_state.rs` — fallback + WARN in `state_provider`.
- Tests alongside both.
