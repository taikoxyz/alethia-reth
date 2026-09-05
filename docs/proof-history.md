# Proof history

Enable the optional history database with `--proofs-history` and
`--proofs-history.storage-path <path>`. It supplies retained historical state to
`eth_getProof` and the debug execution-witness RPCs, including witnesses for
arbitrary transaction lists.

## Initialization and retention

An empty database copies one consistent snapshot of the node's persisted current
state after execution, hashing and Merkle stages agree with the Finish checkpoint
and no partial Merkle work remains. The copied root is checked against its block
header; an invalid copy is discarded and retried. Without
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
against canonical hashes. If either diverges, its V2 data is cleared atomically
and copied again. Backfill uses one upstream snapshot-assisted job with aggregate
progress and ETA; shutdown or a reorg cancels it at a write-batch boundary. Once
backfill completes, the auxiliary snapshot and target file are removed before
indexing and historical reads start. Cancellation waits for in-flight work and
does not impose a fixed shutdown deadline.

Live commits use precomputed trie updates. Missing updates and periodic
verification blocks are executed through the upstream engine's synchronous API.
Execution and root errors stop the sidecar and revoke readiness; temporarily
unavailable parent state is retried. Each submission is confirmed durable before
advancing, preserving per-block persistence and RPC freshness. Idle polling also
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
conservatively fall back to the earliest retained anchor. Reorgs replacing that
anchor automatically rebuild the snapshot with historical reads paused. Shutdown
joins the indexing threads; accepted blocks not yet persisted are recovered from
the node on restart.

## Upgrading a V1 proof database

V1 data is rejected with an explicit rebuild message. It is neither interpreted
as V2 nor deleted. Point the upgraded node at a **new, empty**
`--proofs-history.storage-path` and add `--proofs-history.backfill-window-only` to
rebuild retained history from the node's unpruned changesets. Keep the old V1
directory if rollback is required. Do not point an older binary at the V2 path.

Until initialization and canonical reconciliation finish, historical requests
within 1,024 blocks of the tip can use canonical fallback. Deeper uncovered
requests return an error rather than constructing an unbounded revert overlay.
