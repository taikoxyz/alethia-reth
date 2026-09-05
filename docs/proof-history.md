# Proof history

Enable the optional history database with `--proofs-history` and
`--proofs-history.storage-path <path>`. It supplies retained historical state to
`eth_getProof` and the debug execution-witness RPCs, including witnesses for
arbitrary transaction lists.

## Initialization and retention

An empty database copies one consistent snapshot of the node's persisted current
state. The snapshot root is checked against its block header before use. Without
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
transaction no longer exists. If a pending bootstrap's anchor is reorged, its
never-served V2 data is cleared atomically and copied again. Once backfill
completes, the target file is removed before indexing and historical reads start.

Live commits use precomputed trie updates. Missing updates and periodic
verification blocks are replayed by the upstream engine once execution reaches
the node's on-disk database. Idle polling also catches up without new canonical
notifications. Pruning runs in the engine's persistence transaction;
`--proofs-history.prune-interval` controls idle maintenance polling.
`--proofs-history.max-startup-prune-blocks` still limits automatic pruning after a
retention configuration change.

Canonical reconciliation gates historical reads during startup and reorg
recovery. Reorgs below a completed retained anchor require rebuilding history.
Shutdown joins the indexing threads; accepted blocks not yet persisted are
recovered from the node on restart.

## Upgrading a V1 proof database

V1 data is rejected with an explicit rebuild message. It is neither interpreted
as V2 nor deleted. Point the upgraded node at a **new, empty**
`--proofs-history.storage-path` and add `--proofs-history.backfill-window-only` to
rebuild retained history from the node's unpruned changesets. Keep the old V1
directory if rollback is required. Do not point an older binary at the V2 path.

Until initialization and canonical reconciliation finish, historical requests
within 1,024 blocks of the tip can use canonical fallback. Deeper uncovered
requests return an error rather than constructing an unbounded revert overlay.
