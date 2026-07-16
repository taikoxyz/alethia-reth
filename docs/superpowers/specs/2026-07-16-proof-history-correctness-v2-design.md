# Proof-History Correctness and Upstream Reuse Design

**Status:** Approved

**Date:** 2026-07-16

**Stack base:** `feat/reth-v2.4.0-jit` at `7f50977`

## Context

Alethia's proof-history implementation currently uses the V1
`MdbxProofsStorage`, a copied `LiveTrieCollector`, and custom current/historical
initialization and synchronization logic. Reth 2.4 and the pinned
`reth-optimism-trie` revision now provide the lower-level pieces Alethia needs:

- `MdbxProofsStorageV2`
- `InitializationJob`
- `BackfillJob`
- `EngineHandle`

The stack-base commit `7f50977` repairs a narrow V1 compatibility problem and a
reorg whose common ancestor is the retained earliest block. Those fixes improve
the current implementation, but they do not address the larger correctness
risks: number-only RPC selection, stale MDBX snapshots during reorgs,
restart-unsafe historical initialization, non-finality-aware pruning, mixed
canonical snapshots, or periodic-verification bypass on precomputed reorgs.

This change replaces the custom data-plane implementation with upstream V2
components while retaining a small Alethia safety and lifecycle layer.

## Goals

1. Never combine canonical state from one branch with proof-history data from
   another branch.
2. Handle canonical commits, reverts, and reorgs within the retained window,
   including a common ancestor equal to the retained earliest block.
3. Fail closed when a reorg replaces the retained earliest block; never serve a
   stale proof or witness while recovery is impossible from retained data.
4. Make initialization and restart behavior deterministic and crash-safe.
5. Prune only history that is older than the persisted finalized fault window.
6. Reuse upstream V2 initialization, backfill, engine, and state-provider
   implementations instead of maintaining copies.
7. Reuse Reth's configured proof permit and blocking runtime for proof and
   witness RPC work.

## Non-goals

- Transparently converting V1 proof-history data into V2. Operators must wipe
  and reinitialize the proof-history database once.
- Serving pending state through proof history. Pending requests use the normal
  Reth state provider without a proof-history overlay.
- Recovering a reorg that replaces the retained earliest block. The data needed
  for that recovery has already been pruned.
- Adopting the complete upstream `OpProofsExEx`. It lacks Alethia's startup
  initialization, reconciliation/readiness contract, finality-aware pruning,
  and RPC canonicality guard.
- Making in-memory `EngineHandle` updates immediately visible to RPC. RPC sees
  committed proof storage and safely falls back to canonical state near the
  tip while the engine buffer is awaiting persistence.

## Correctness invariants

1. A proof-history read snapshot is usable only when its exact stored latest
   `BlockNumHash` is canonical both before and after the RPC computation.
2. A requested canonical block or witness target is validated by exact hash,
   never by block number alone.
3. Every append, backfill, reorg, and prune operation preserves parent-hash
   continuity. Therefore a canonical stored latest hash attests to the entire
   retained branch back to the stored earliest block.
4. `ProofHistoryReadiness == true` means the committed proof window was
   reconciled against a single canonical database snapshot. It does not replace
   per-request exact-hash validation.
5. A prune transaction derives its state and retained bounds from one proof DB
   write transaction and derives every canonical/finality input from one main
   DB read snapshot.
6. No RPC request uses proof-history data while initialization, reorg repair,
   notification-lag recovery, or unrecoverable-window handling is in progress.

## Storage V2 cutover

`ProofHistoryStorage` changes to
`OpProofsStorage<Arc<MdbxProofsStorageV2>>`. Metrics and all RPC/sidecar generic
types follow that alias.

V1 and V2 use separate proof-window and history tables in the same MDBX
environment. Simply opening V2 on an existing V1 path would silently present an
empty V2 database. Startup must therefore probe the V1 store before opening the
V2 runtime store:

- An empty V1 store with `InitialStateStatus::NotStarted` is allowed.
- Any V1 proof-window bound, completed anchor, or in-progress initialization is
  rejected.
- The error names the configured proof-history path and tells the operator to
  remove that directory, or configure a fresh path, and restart.
- Startup never deletes the directory automatically.

The V1 missing-`LatestBlock` migration added by `7f50977` and its V1-only tests
are removed because valid V1 data is now deliberately refused rather than
mutated.

## Initialization and backward backfill

The sidecar subscribes to canonical notifications before starting any long
initialization job. Readiness remains false throughout initialization.

### Current-state initialization

1. Open one long-lived read transaction on the main Reth database.
2. Resolve the persisted executed head number and sealed header from that same
   transaction.
3. Select the Reth trie layout from the transaction's storage settings.
4. Run upstream `InitializationJob` against V2 using that exact number and
   hash.

An interrupted upstream initialization can resume only when the current source
head is exactly the stored initialization anchor. If the node advanced or
reorged while the copy was interrupted, source current-state tables no longer
represent the stored anchor. Startup fails with the same wipe/reinitialize
instruction instead of attempting a mixed-anchor resume.

### Finalized-window initialization

When `backfill_window_only` is enabled:

1. Read the persisted finalized head and persisted executed head from a pinned
   main DB snapshot.
2. If no finalized head exists, or execution has not reached
   `finalized.number.saturating_sub(window)`, stay not-ready and retry.
3. Initialize V2 at the persisted executed head with `InitializationJob`.
4. Open a fresh pinned main DB snapshot, verify that the stored initialization
   anchor is still canonical, recompute the finalized-window target, and run
   upstream `BackfillJob` down to that target.

`BackfillJob` writes at most its upstream default batch of 25 blocks per MDBX
transaction, validates the reconstructed state root per block, and resumes from
the committed earliest block after a crash. The custom reverse-changeset
overlay initializer, its metadata file, and `init.rs` are deleted.

### Post-initialization reconciliation

Initialization or backfill may take long enough for canonical state to move.
After either job completes, the sidecar performs the full startup reconciliation
again using a fresh pinned main DB snapshot. It sets readiness only after the
stored earliest and latest hashes pass that reconciliation.

If the single-block initialization anchor became noncanonical, no older proof
history exists from which to unwind. The sidecar fails closed with the
wipe/reinitialize instruction.

## Upstream engine adapter

The copied `LiveTrieCollector`, manual `process_batch` loop, and custom sync
worker are replaced by one owned upstream `EngineHandle`.

`EngineHandle` requires an `OpProofStoragePruner`. It is constructed with
`u64::MAX`, which makes the engine's built-in latest-relative pruner a no-op.
Alethia's finality-aware pruner runs separately.

The sidecar polls the persisted executed head every five seconds and calls
`engine.sync_to(executed_head)`. Canonical notifications remain the fast path.
The engine owns its uncommitted memory buffer and persists it using upstream
threshold and idle-flush behavior.

### Commits

For each exact notification block in ascending order:

- Use `engine.index_block` when precomputed trie data exists and the configured
  verification interval is not due.
- Use `engine.execute_block` for a verification height or when trie data is
  absent.
- If a notification starts beyond the engine tip, use `sync_to` so the engine
  fills the canonical gap from persisted Reth data.

### Reorgs and reverts

Readiness is cleared before applying a reorg or revert. The old and replacement
chains must have the same fork block, and the replacement blocks must form a
contiguous parent-linked sequence.

- If every replacement block has precomputed trie data and no replacement
  height is due for verification, call `engine.reorg`.
- Otherwise call `engine.unwind(old.first().block_with_parent())`, then process
  the exact replacement blocks in ascending order with `index_block` or
  `execute_block`. Blocks are never refetched by number from a concurrently
  moving provider.
- A reorg whose common ancestor equals the retained earliest block is
  recoverable. The engine unwinds persisted history to that anchor and buffers
  the replacement suffix.
- A reorg or revert whose first removed block is at or below the retained
  earliest block replaces the anchor itself. Readiness remains false and the
  critical sidecar returns an actionable error.

After a successful unwind, the committed proof DB may end at the common
ancestor while replacement updates remain in the engine buffer. This state is
safe: reconciliation validates the committed head, readiness can be restored,
and near-tip RPC requests fall back to canonical state until persistence makes
the replacement suffix visible.

### Buffered notifications and lag

Notifications buffered during initialization can describe work already
incorporated by post-init reconciliation. Before applying a reorg/revert, if the
committed proof latest hash is already canonical, the notification is treated
as consumed and the engine is synced to the persisted executed head. Commit
handling also skips an already-covered committed suffix.

On broadcast lag:

1. Replace the lagged receiver first so new events are buffered.
2. Clear readiness.
3. Drop the sole `EngineHandle`. This joins the upstream engine, waits for any
   in-flight persistence, and discards the remaining private buffer.
4. Reconcile the committed V2 window against a fresh canonical DB snapshot.
5. Spawn a new engine and `sync_to` the persisted executed head.
6. Restore readiness only after reconciliation succeeds.

Startup and live-lag recovery intentionally treat a proof head above the main
DB's persisted head differently. Startup may wait not-ready because an
independently persisted proof head can legitimately precede primary-DB catch-up
after an ungraceful restart. During live lag, a backward canonical move is known
to have happened; recovery discards engine memory and reconciles immediately
rather than waiting on an old target.

## Finality-aware pruning

The upstream latest-relative pruner cannot be used directly: it can remove
unfinalized history and obtains proof bounds, canonical hashes, and its write
transaction at different times.

Each Alethia prune tick performs at most one 50-block batch:

1. Open the proof DB write transaction and read the current proof window from
   that transaction.
2. Open one main DB read snapshot.
3. Read the persisted finalized head from that snapshot. With no finalized
   head, do nothing.
4. Validate the proof earliest and latest `BlockNumHash` values against that
   same canonical snapshot. If either endpoint is unavailable or mismatched,
   abort without committing; reorg reconciliation owns repair.
5. Compute
   `desired_earliest = finalized.number.saturating_sub(config.window)`.
   If it is not above the stored earliest, or it is at/above the stored latest,
   do nothing.
6. Set the batch target to
   `min(stored_earliest + 50, desired_earliest)`.
7. Resolve the target sealed header and its parent hash from the same canonical
   snapshot, and verify the parent is the canonical block at `target - 1`.
8. Call `provider_rw.prune_earliest_state(BlockWithParent)` and commit the proof
   transaction.

This retains the complete configured fault window behind finalized state even
when the canonical tip is much farther ahead. MDBX's single-writer behavior
serializes this transaction with engine persistence; the pinned canonical
snapshot keeps the state/hash label internally consistent if a reorg starts
concurrently.

The startup prune safety calculation, if retained for CLI compatibility, uses
the finalized-window target rather than `latest - window`.

## RPC canonical snapshot guard

### Exact block resolution

Canonical requests resolve one sealed header and retain its exact
`BlockNumHash`. State is loaded by that hash, not by re-resolving the original
tag or number. The provider's canonical hash at that height is checked before
and after state acquisition.

Pending requests bypass proof history and use the normal Reth pending state
provider.

### Proof snapshot selection

Opening a proof-history RO provider returns both the state-provider overlay and
a guard containing:

- the exact requested or parent `BlockNumHash`; and
- the exact `ProofWindowRange.latest` captured by that same MDBX snapshot.

Proof history is selected only when readiness is true, the requested number is
inside the snapshot's numeric bounds, and both captured hashes are canonical.
Uncovered near-tip requests retain the bounded canonical-state fallback;
uncovered deep-history requests retain the existing refusal.

After the complete proof or witness—including trie walks and ancestor-header
assembly—the guard validates the requested/parent hash and snapshot-latest hash
again. A mismatch returns a deterministic transient "canonical state changed;
retry" RPC error. The operation is not automatically retried, avoiding
duplicate expensive work during an active reorg.

Both debug witness paths also capture and pre/post-validate the exact target
block hash, preventing a by-number or by-hash target from silently becoming a
noncanonical sibling during execution.

### Reth execution resources

`eth_getProof` and debug witness methods remove their hardcoded semaphores. They
acquire an owned permit through Reth's `SpawnBlocking::acquire_owned_tracing`
and submit work through the Eth API's configured blocking runtime. The permit
is moved into the non-abortable blocking closure so client cancellation cannot
release capacity while work continues.

## Error behavior

- **Populated/in-progress V1 DB:** startup error with exact path and
  wipe/reinitialize instructions.
- **Interrupted V2 init with a moved source anchor:** startup error with
  wipe/reinitialize instructions.
- **No persisted finalized head:** finalized-window initialization and pruning
  wait/no-op while readiness stays false for initialization.
- **Canonical movement during RPC:** transient retryable RPC error; never a
  mixed-branch result.
- **Recoverable in-window reorg:** clear readiness, repair/unwind through the
  engine, reconcile committed storage, then restore readiness.
- **Reorg replacing retained earliest:** readiness remains false and the
  critical sidecar fails with an actionable wipe/reinitialize error.
- **Engine action or reconciliation failure:** readiness remains false and the
  critical sidecar propagates the error.
- **Pruner canonical mismatch:** no proof transaction commit; log and retry on
  the next tick after sidecar reconciliation.

## Test strategy

### Migration and initialization

- Empty V1 path permits V2 startup.
- Completed or in-progress V1 data is refused with the configured path in the
  error.
- Fresh V2 current-state initialization records matching earliest/latest.
- Interrupted V2 initialization resumes only at the exact same source anchor;
  a moved anchor fails closed.
- Finalized-window initialization waits without finality, initializes at the
  persisted head, and backfills to `finalized - window`.
- A canonical change during initialization is detected by the mandatory
  post-init reconciliation before readiness.

### Engine and reorg handling

- Normal commit uses precomputed data except at a verification height.
- An all-precomputed reorg with no verification height uses `engine.reorg`.
- Missing trie data or a due verification height uses unwind plus ordered
  index/execute of the exact notification blocks.
- A production notification-path reorg with common ancestor equal to earliest
  succeeds with non-empty state changes.
- A replacement that does not descend from the retained anchor fails.
- A failure after unwind leaves readiness false and never exposes the old
  branch.
- A reorg replacing earliest fails closed.
- Duplicate buffered notifications are idempotent.
- Lag recovery drops private engine state, reconciles committed storage, and
  starts a fresh sync target.

### Pruning

- No persisted finalized head is a no-op.
- The target is `finalized - window`, not `latest - window`.
- One tick advances earliest by no more than 50 blocks.
- Noncanonical stored earliest/latest, a missing target header, or broken
  parent continuity causes no commit.
- A concurrent branch change cannot label proof state with hashes from a
  different canonical snapshot.

### RPC

- `latest`/tag resolution returns one exact number/hash/state tuple.
- Pending bypasses proof history.
- A covered V2 request validates the requested hash and snapshot latest hash.
- An old MDBX RO snapshot opened on branch A fails postvalidation after branch
  B becomes canonical, even if the sidecar has already repaired the live DB.
- Reorgs above the snapshot latest do not falsely invalidate an otherwise
  canonical historical request.
- `eth_getProof`, normal debug witness, and tx-list witness all postvalidate.
- Proof and witness work uses the Reth guard/runtime and keeps the owned permit
  inside the blocking closure.

### Verification commands

Run focused crate tests during TDD, followed by:

```text
just fmt
just clippy
just test
```

## Resulting upstream/custom boundary

After this change, upstream owns the V2 schema, state reads, initialization,
backward backfill, execution/index buffering, persistence, unwind, and reorg
mechanics. Alethia owns only the pieces determined by its operational contract:

- V1 refusal and V2 lifecycle wiring
- startup/post-init/lag reconciliation and readiness
- notification verification policy
- persisted-finality pruning policy
- exact-hash RPC canonicality guards

Proof history is therefore still required after the Reth 2.4 bump for deep
historical proof and witness service, but most of its custom implementation is
not.
