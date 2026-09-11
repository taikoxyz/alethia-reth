# Proof history

Enable the optional history database with `--proofs-history` and
`--proofs-history.storage-path <path>`. It supplies retained historical state to
`eth_getProof` and the debug execution-witness RPCs, including arbitrary
transaction lists.

## Initialization and retention

An empty database copies persisted state after execution, hashing and Merkle
stages agree with Finish and no partial Merkle work remains. Missing Finish
headers cause a logged retry. Without `--proofs-history.backfill-window-only`,
indexing starts at this snapshot and history grows as the node advances.

With `--proofs-history.backfill-window-only`, initialization waits until finality
is known and execution reaches `finalized - window`. The initial backward target
is `max(executed, finalized) - window`, bounded at genesis and the snapshot
height. Account and storage changesets needed for reconstruction must remain
available. The `backfill-target` file pins unfinished work across restarts;
committed batches resume from the retained earliest block.

The first snapshot-assisted step copies the account trie, storage trie, hashed
accounts and hashed storages into auxiliary tables before reconstructing older
blocks. Its node-header lookups are short; the auxiliary copy itself can be
substantial and keeps readiness false. Upstream reports copy progress. A fresh
range of at most one upstream backfill batch uses plain backfill instead. An
existing auxiliary snapshot stays synchronized through the final chunk and
restart. Stale auxiliary anchors clear only that cache and retry immediately.

Backward reconstruction processes at most 10,000 blocks per source transaction
and writes journal hashes in batches of at most 1,000 entries. Each chunk reports
progress/ETA and the retained height, target and remaining blocks. Changed
canonical evidence requests reconciliation; a lagging persisted view waits.
Pruning, journal and storage errors remain fatal, with the original cause and
any failed recovery probe preserved. Auxiliary-copy errors have their own context.

## Pinned reads and shutdown

Normal reth unwinds commit MDBX, wait for older readers, then commit static-file
truncation. A pinned reader protects its visible header prefix and state; disabling
the long-read timeout does not remove that barrier. Such readers prevent page
reuse and can delay persisted unwind completion, allowing in-memory blocks to
accumulate. Chunking bounds work per backward pin, not its wall-clock duration.
The initial copy still holds one reader throughout; making that copy bounded and
resumable remains a follow-up. Production duration and memory use are unbenchmarked.

Both initial copies and backward chunks expose
`taiko_proof_history_pin_active` and `taiko_proof_history_pin_elapsed_seconds`,
with `phase="initial_copy"` or `phase="backfill"`. Elapsed time updates once per
minute while active and at completion. Start/end messages use INFO; long phases
report again every ten minutes at INFO. Upstream retains detailed per-table and
per-chunk progress. Failure to start periodic reporting does not abort indexing; unwind/panic
cleanup releases the guard and its resources.

Shutdown or a closed notification source cancels new work at batch boundaries;
accepted transactions finish atomically. The outer reth CLI has a default
five-second graceful phase, which does not guarantee completion of all work or
total process exit within five seconds. Restart uses the last committed batch.

## Canonical reconciliation and live indexing

Historical reads wait for canonical validation. The `IndexedBlockHashes` journal
finds the last retained common block after shallow reorgs, missed notifications
or restart. Older windows without journal coverage use the earliest-anchor
fallback. A non-canonical earliest anchor requires rebuilding; divergence above
it preserves the canonical prefix. A shorter chain reconciles immediately when
the journal proves divergence; ordinary catch-up otherwise waits.

A missing hash at or below the observed head is a separate condition. The sixth
observation of the same retained tip raises one ERROR and sets
`taiko_proof_history_missing_canonical_hash` to 1. With the five-second retry
interval, that is five retry delays (about 25 seconds after the first observation,
plus work time). Advancing `canonical_best` does not reset this identity. The
sidecar keeps waiting with historical readiness false; it does not panic the
execution client. This condition can occur during resynchronization, so check
sync and canonical headers first. A resolved condition or different retained tip
clears/restarts the episode and resets the gauge. Normal catch-up above the
canonical head is excluded. Waiting for an unavailable earliest anchor is logged
at INFO.

Live notifications normally supply precomputed trie updates. Verification and
catch-up execute synchronously and confirm each submission is durable. Hashes
are journaled once per bounded replay batch or notification suffix. Submissions
remain serialized because upstream can report success without accepting an
unavailable parent. Idle polling recovers an in-memory tail without another
notification. Pruning runs with engine persistence; `--proofs-history.prune-interval`
controls idle polling and must be positive. `--proofs-history.max-startup-prune-blocks`
limits automatic pruning after a retention change.

## Failures and recovery

Root validation follows the initial job's release of its reader. A later header
change can permit discarding a failed copy and retrying; this does not imply a
header tear under the earlier pin. A stable mismatch or failed header probe keeps
the copy, journal, anchor and root diagnostics. **Repairing the source and
restarting at that same path does not replace the failed copy.** Stop the node,
keep the failed directory offline for diagnosis, repair the cause, and restart
with a **new, empty** `--proofs-history.storage-path`. Evidence is retained at
failure; running normal reorg recovery against the old path can still reset a
non-canonical anchor.

A failed backward chunk keeps its pending target and resumes the same work on
restart. After investigating its original error, either rebuild at a new empty
path or, for an otherwise healthy retained window, stop the node and remove only
`<storage-path>/backfill-target`. Restarting then abandons the requested older
coverage and continues live indexing from the shorter retained window. Startup
clears auxiliary snapshot tables even when the marker is absent, while preserving
retained proofs and their journal. This escape does not bypass canonical/root
validation or repair a corrupt initial copy or unavailable forward-replay data.

Replay needs retained bodies, bytecode, block hashes and a resolvable parent-state
provider. Account/storage reads and trie roots come from proof history, but node
pruning can still make those prerequisites unavailable after downtime. Restore
missing history or repair source/EVM inconsistencies before rebuilding. Stable
execution/root errors stop the critical sidecar and therefore the node; their
messages include the failed block, original cause and recovery guidance.

## Upgrading a V1 proof database

V1 data is rejected without migration or deletion. Select a **new, empty**
`--proofs-history.storage-path` and use `--proofs-history.backfill-window-only`
with the required node changesets retained. Keep the V1 directory for rollback;
do not point an older binary at the V2 path.

Until initialization and reconciliation finish, requests within 1,024 blocks of
the tip can use canonical fallback. Deeper uncovered requests fail rather than
constructing an unbounded revert overlay.
