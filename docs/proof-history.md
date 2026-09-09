# Proof history

Enable the optional history database with `--proofs-history` and
`--proofs-history.storage-path <path>`. It supplies retained historical state to
`eth_getProof` and the debug execution-witness RPCs, including witnesses for
arbitrary transaction lists.

## Initialization and retention

An empty database copies one consistent snapshot of the node's persisted current
state after execution, hashing and Merkle stages agree with the Finish checkpoint
and no partial Merkle work remains. The copied root is checked against its block
header. State tables use a pinned MDBX transaction, while headers come from shared
static files. An invalid copy is discarded. A fresh header read determines whether
to wait after a header change or fail immediately with source-repair guidance; a
stable mismatch does not trigger another full copy. Missing Finish headers are
logged while initialization waits. Without
`--proofs-history.backfill-window-only`, indexing starts at that snapshot and
history grows as the node advances.

With `--proofs-history.backfill-window-only`, initialization waits until finality
is known and local execution reaches `finalized - window`. It then copies the
current state and reconstructs older state from changesets using the upstream V2
backfill job. The initial target is `max(executed, finalized) - window`, bounded
at genesis and the snapshot height. This avoids requesting an extra window while
the node is still syncing, or building history that retention would immediately
remove. Required account and storage changesets must remain unpruned during
backfill; missing history fails initialization.

The `backfill-target` file beside the database pins an unfinished bootstrap's
target across restarts. Committed backward batches resume from the stored earliest
block. An interrupted initial snapshot copy restarts because its original source
transaction no longer exists. Both bounds of a pending bootstrap are checked
against canonical hashes. A non-canonical earliest anchor requires an atomic
reset and new copy. A divergent latest anchor unwinds to the journal's last common
block, preserving already-backfilled history when the earliest anchor still
matches. Reorg notifications do not cancel a pinned initial copy; its anchor is
rechecked before publishing readiness.

Backward jobs run in chunks of at most 10,000 blocks, releasing the node read
transaction and rechecking canonical bounds between chunks. This bounds the
period during which backward backfill pins pages freed by ongoing node writes;
the initial full-state copy still needs a long read transaction. The auxiliary
snapshot is built or resumed through short header lookups before opening a
backfill chunk's pinned node transaction. A stale auxiliary anchor is discarded
and rebuilt without clearing retained proofs or their journal. Progress and ETA are reported per chunk,
with the overall retained height, target and remaining blocks logged at each
checkpoint. A fresh range of at most one upstream backfill batch uses plain backfill to
avoid duplicating the full state. Larger ranges use an auxiliary snapshot, and
an existing snapshot stays active through the final chunk and restart. These are
conservative operational limits, not benchmarked performance crossover points.
After completion, the auxiliary snapshot and target file are removed before
indexing and historical reads start. If persisted and canonical bounds disagree,
backfill uses a delayed retry instead of reporting progress. Only
header/root errors accompanied by changed canonical or auxiliary anchors are
retried, with the
error logged; missing changesets, journal hashes and storage failures remain fatal.

Shutdown or a closed notification source cancels new work at batch boundaries.
Accepted writes remain atomic and the sidecar waits for workers to join. The
outer reth CLI has a default five-second graceful-shutdown timeout, so process
shutdown can outlast that graceful phase without all sidecar work having joined;
restart resumes from the last committed batch.

Live commits use precomputed trie updates. Missing updates and periodic
verification blocks are executed through the upstream engine's synchronous API.
Execution and root errors stop the sidecar and revoke readiness; temporarily
unavailable parent state is retried. Each submission is confirmed durable before
advancing, preserving per-block persistence and RPC freshness. Write-ahead hashes
are recorded once per replay batch or notification suffix. Submissions remain
serialized because upstream can return success without accepting an unavailable
parent; later blocks must not trigger implicit gap replay. Throughput and memory
have not been benchmarked. Idle polling also
catches up to the canonical in-memory tail without new notifications. Pruning
runs in the engine's persistence transaction;
`--proofs-history.prune-interval` controls idle maintenance polling and must be
greater than zero.
`--proofs-history.max-startup-prune-blocks` still limits automatic pruning after a
retention configuration change.

Canonical reconciliation gates historical reads during startup and reorg
recovery. A durable height-to-block-hash journal identifies the last retained
common block, so shallow reorgs preserve the canonical prefix even when their
notifications are stale or missing. Older V2 stores without journal entries
conservatively fall back to the earliest retained anchor. The journal now uses
`IndexedBlockHashes`. A shorter canonical chain can reconcile
immediately when the journal proves divergence at its tip; a matching prefix
still waits for catch-up. Reorgs replacing that
anchor automatically rebuild the snapshot with historical reads paused. Shutdown
joins the indexing threads; accepted blocks not yet persisted are recovered from
the node on restart.

## Replay failures

Live catch-up and verification still need reth to resolve parent-state providers,
block bodies, bytecode and block hashes. Although account/storage reads and trie
roots come from the proof database, node pruning can make those prerequisites
unavailable after downtime. Keep the required node history available for both
backfill and replay; `--full` pruning may prevent recovery across an old gap.

Execution/root failures revoke readiness and stop the sidecar. The error identifies
the failed block and recovery path. Check pruning, source-state integrity and EVM
configuration first; preserve the failing proof database for diagnosis. Restore
missing node history or repair the underlying cause, then use a new empty
`--proofs-history.storage-path` when a rebuild is needed. Repeatedly restarting
against the same deterministic error cannot repair it, and automatic rebuilding
would hide an execution or proof divergence.

## Upgrading a V1 proof database

V1 data is rejected with an explicit rebuild message. It is neither interpreted
as V2 nor deleted. Point the upgraded node at a **new, empty**
`--proofs-history.storage-path` and add `--proofs-history.backfill-window-only` to
rebuild retained history from the node's unpruned changesets. Keep the old V1
directory if rollback is required. Do not point an older binary at the V2 path.

Until initialization and canonical reconciliation finish, historical requests
within 1,024 blocks of the tip can use canonical fallback. Deeper uncovered
requests return an error rather than constructing an unbounded revert overlay.
