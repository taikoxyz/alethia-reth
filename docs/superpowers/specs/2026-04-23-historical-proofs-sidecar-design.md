# Historical Proofs Sidecar — Design

**Status:** Approved for implementation planning
**Date:** 2026-04-23
**Author:** David (david@taiko.xyz)

## Problem

`debug_executionWitness` and `eth_getProof` against relatively-old blocks on alethia-reth take 10+ seconds (observed: ~15s). The cause is reth's historical-state model: to serve a query at block N, reth walks state changesets backward from the chain tip and reconstructs state in memory. This is fast for recent blocks and increasingly expensive — and OOM-prone — for older ones. The issue is tracked upstream as [paradigmxyz/reth#18070](https://github.com/paradigmxyz/reth/issues/18070) with no fix landed yet.

Consumers (raiko-style provers, external infra issuing `eth_getProof` during a challenge window) need sub-second historical-block proof latency within a bounded window.

## Goal

Introduce a bounded-history sidecar that serves `debug_executionWitness` and `eth_getProof` for blocks within a configurable window with single-digit-millisecond-to-low-hundreds-of-milliseconds latency.

Pattern adapted from op-rs's [`reth-optimism-exex` + `reth-optimism-trie`](https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-reth) crates. Directly importing those crates was ruled out — op-reth pins reth v2.0.0 while alethia-reth is on v2.1.0, and no upstream bump is in flight.

## Non-goals

- Arbitrary-depth historical proofs (queries older than the window fail explicitly).
- Backfilling proof history before the sidecar's init block (the sidecar is forward-only by design).
- Overriding `debug_executePayload` / future-payload simulation (op-reth ships this; Taiko does not need it yet).
- Replacing reth's native historical-state machinery — we sit alongside it and take over only when the flag is on.

## Decisions locked in

| Decision | Value |
|---|---|
| RPC scope | `debug_executionWitness` + `eth_getProof` |
| Default retention window | 259,200 blocks (≈ 72 hours at 1s block time) |
| Window is configurable | Yes, via `--proofs-history.window` |
| Rollout model | Brand-new nodes as documented path; retrofit of existing nodes is free operationally |
| Delivery | Single end-to-end PR |
| Code organization | Two new crates (`proofs-trie`, `proofs-exex`) + extensions to existing `rpc`/`cli`/`bin` |
| Default state | Off — feature activated only via `--proofs-history` |

---

## 1 / Architecture overview

A secondary MDBX database populated by a live ExEx that writes versioned trie data as blocks are executed. Historical-block RPC calls read from the sidecar instead of reverting changesets.

```
┌─────────────────────────────────────────────────────────────────┐
│                     Main reth node (TaikoNode)                  │
│                                                                 │
│  Engine/Consensus ──► BlockExecutor ──► CanonicalStateNotifier  │
│                              │                   │              │
│                              ▼                   ▼              │
│                   TrieUpdates +         ExEx notification       │
│                   HashedPostState              │                │
│                                                ▼                │
│                                    ┌────────────────────────┐   │
│                                    │    proofs-exex          │  │
│                                    │  (LiveTrieCollector)    │  │
│                                    │                         │  │
│                                    │  writes versioned rows  │  │
│                                    │  + runs pruner          │  │
│                                    └───────────┬─────────────┘  │
│                                                │                │
│                                                ▼                │
│                                    ┌────────────────────────┐   │
│                                    │   proofs-trie (MDBX)    │  │
│   RPC override: ◄──────────────────┤  AccountTrieHistory     │  │
│     debug_executionWitness         │  StorageTrieHistory     │  │
│     eth_getProof                   │  HashedAccountHistory   │  │
│                                    │  HashedStorageHistory   │  │
│                                    └────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

**Invariants:**

- **Forward-only.** Sidecar can only serve blocks ≥ init-block.
- **Bounded.** Blocks older than `tip − window_size` are pruned.
- **Off by default.** When `--proofs-history` is unset, no code path is active; behavior is byte-identical to today.

**Code layout:**

```
crates/
├── proofs-trie/          # NEW: MDBX schema, cursors, proof generation, init/prune logic
├── proofs-exex/          # NEW: Live ExEx; writes to proofs-trie
├── cli/                  # EXTEND: `alethia-reth proofs init|prune|unwind` + flags
└── rpc/                  # EXTEND: debug + eth overrides bound on Taiko primitives
bin/alethia-reth/
├── src/main.rs           # EXTEND: call install_proofs_history before existing wiring
└── src/proofs_history.rs # NEW: helper that installs the ExEx + RPC overrides
```

---

## 2 / Storage layer (`proofs-trie` crate)

Dedicated MDBX environment separate from the main reth DB. Every row tagged with `block_number`; reads select the latest version ≤ target block.

### Tables

| Table | Key | SubKey (dup) | Value | Purpose |
|---|---|---|---|---|
| `AccountTrieHistory` | `StoredNibbles` | `u64` block | `VersionedValue<BranchNodeCompact>` | Account trie branches |
| `StorageTrieHistory` | `(hashed_addr ‖ nibbles)` | `u64` | `VersionedValue<BranchNodeCompact>` | Storage trie branches |
| `HashedAccountHistory` | `B256` hashed addr | `u64` | `VersionedValue<Account>` | Account leaves |
| `HashedStorageHistory` | `(hashed_addr ‖ hashed_slot)` | `u64` | `VersionedValue<U256>` | Storage slot leaves |
| `BlockChangeSet` | `u64` block | — | bincode(`ChangeSet`) | Reverse index for prune |
| `ProofWindow` | `EarliestBlock`/`LatestBlock` tag | — | `BlockNumberHash` (40 B) | Active window bounds |
| `SchemaVersion` | `()` | — | `u32` | Sidecar env format version |

`VersionedValue<T> = block_number (u64 BE) ‖ MaybeDeleted<T>` where an empty value encodes deletion (tombstone).

All four history tables are `DupSort` — MDBX natively returns the latest version ≤ target subkey.

### Cursor design

- **Hashed cursors** read account/slot leaves from `HashedAccountHistory` / `HashedStorageHistory`.
- **Trie cursors** read branch nodes from `AccountTrieHistory` / `StorageTrieHistory`.

Both wrap MDBX dup-sort cursors and implement the rule "latest version ≤ target block, skipping tombstones." They plug into reth's existing `TrieCursor` / `TrieStorageCursor` traits — the standard proof-generation logic in `reth-trie` works unchanged against this store.

### Providers

- `ProofsProviderRO` — read-only queries and proof generation.
- `ProofsProviderRw` — batch writes from the ExEx.
- `ProofsInitProvider` — resumable init with anchor tracking.

### Baseline encoding

Block `0` is the baseline version for all init-copied entries. Any real ExEx write at block N > 0 naturally shadows the baseline via dup-sort ordering — no special-case logic.

### Module layout

```
proofs-trie/src/
├── api.rs              # Public traits (ProofsStore, ProofsProviderRO, …)
├── db/
│   ├── cursor.rs       # BlockNumberVersionedCursor + MdbxTrieCursor impls
│   ├── store.rs        # MdbxProofsStorage — owns the MDBX env
│   └── models/         # Table definitions, VersionedValue, MaybeDeleted
├── cursor_factory.rs   # Factory wiring cursors to reth-trie's TrieCursor trait
├── provider.rs         # StateProvider impl that reth-trie can feed
├── proof.rs            # eth_getProof + debug_executionWitness proof assembly
├── initialize.rs       # Backfill from main DB → sidecar (block 0 baseline)
├── live.rs             # LiveTrieCollector — consumed by proofs-exex
├── prune/              # Window-based pruning using BlockChangeSet
├── error.rs
└── metrics.rs
```

### Differences from op-reth

Schema: none. Rename-only (`OpProofs*` → `Proofs*`). The `SchemaVersion` table is new — cheap insurance for future schema evolution.

---

## 3 / ExEx (`proofs-exex` crate)

### Main loop

```
1. ensure_initialized()       — verify sidecar has been seeded; fail fast with a clear
                                message pointing to `alethia-reth proofs init`
2. spawn pruner task          — background task, runs every `prune_interval`, deletes
                                blocks older than `tip − window_size` using BlockChangeSet
3. spawn sync/catchup task    — if ExEx lags tip, batches up to 50 blocks at a time
4. stream ExExNotifications   — per canonical chain delta:
                                  • ChainCommitted → write TrieUpdates + HashedPostState
                                  • ChainReorged   → unwind above common ancestor, then write
                                  • ChainReverted  → unwind only
                                acknowledge FinishedHeight only after sidecar commit
```

### Safety invariants

- **FinishedHeight acks after commit.** reth may prune changesets ≤ acked height. Premature ack = unrecoverable.
- **Startup safety threshold.** If `tip − earliest_stored > window + MAX_PRUNE_BLOCKS_STARTUP` (1000), refuse to start; instruct operator to run `proofs prune` manually. Prevents multi-hour prune during boot.
- **Reorgs first-class.** Unwind path uses `BlockChangeSet[N]` to find exactly which versioned rows to delete above the common ancestor.

### What gets written per block

From the ExEx notification's `ExecutionOutcome`:

- `TrieUpdates` → `AccountTrieHistory` + `StorageTrieHistory`
- `HashedPostState` → `HashedAccountHistory` + `HashedStorageHistory`
- Union of all modified keys → `BlockChangeSet[block_number]`
- `ProofWindow[LatestBlock]` pointer updated

### Module layout

```
proofs-exex/src/
├── lib.rs              # ProofsExEx<Node, Storage> + builder + run loop
├── sync.rs             # Batched catch-up for lag scenarios
└── (prune lives in proofs-trie::prune, consumed here)
```

### Generic over node type

`ProofsExEx<Node: FullNodeComponents, Storage: ProofsStore>`. No Taiko coupling at this layer. Taiko's anchor transaction and raiko2 anchor-witness recording (from PR #161) are captured naturally via normal block execution.

---

## 4 / CLI subcommands

Added under new `alethia-reth proofs` namespace, matching op-reth's `op-reth proofs`.

### `alethia-reth proofs init`

Seeds sidecar from main reth DB's current state. Reads `HashedAccounts`, `HashedStorages`, `AccountsTrie`, `StoragesTrie`; writes each entry as a `VersionedValue` at block `0`.

```
alethia-reth proofs init \
  --datadir /data/alethia-reth \
  --proofs-history.storage-path /data/alethia-reth/proofs
```

- **Resumable** via `InitialStateAnchor`. Crashed init picks up from last written key.
- **Idempotent.** If `earliest_block` already set, exits cleanly.
- **Blocking foreground command.** Operator controls when it runs.
- **Duration on hoodi scale:** minutes.

### `alethia-reth proofs prune`

Manually triggers pruner. For space reclamation after a window change or if background pruner was disabled.

```
alethia-reth proofs prune \
  --datadir /data/alethia-reth \
  --proofs-history.storage-path /data/alethia-reth/proofs \
  --proofs-history.window 259200 \
  --proofs-history.prune-batch-size 10000
```

Uses `BlockChangeSet` as reverse index: for each block `N < tip − window`, read its changeset, delete pointed-to versioned rows, delete the changeset. Batch-size bounds memory.

### `alethia-reth proofs unwind`

Manual unwind for disaster recovery. Deletes all versioned rows with block > target, updates `ProofWindow[LatestBlock]`.

```
alethia-reth proofs unwind \
  --datadir /data/alethia-reth \
  --proofs-history.storage-path /data/alethia-reth/proofs \
  --target 12345
```

### Integration

- Lives under `crates/cli/src/commands/proofs/{mod,init,prune,unwind}.rs`.
- Registered in `TaikoCli` command enum at `crates/cli/src/command.rs`.
- Reuses reth's standard `DatadirArgs` parsing.

---

## 5 / RPC overrides

Two endpoints replaced via reth's `replace_configured`: `debug_executionWitness` and `eth_getProof`. Both route historical block IDs through a common state-provider factory.

### State provider factory

```
ProofsStateProviderFactory<Eth, Storage>
├── state_provider(BlockId) → Box<dyn StateProvider>
│   ├── if historical (block ≤ sidecar latest_stored):
│   │     return ProofsStateProvider::new(storage.provider_ro(), block)
│   │       ↳ implements reth's StateProvider + StorageRootProvider traits
│   │       ↳ backed by versioned-trie cursors from proofs-trie
│   └── else:
│         delegate to eth_api's native state_provider
```

**Threshold choice:** aggressive routing — any block ≤ sidecar's `latest_stored` uses the sidecar path. The ExEx writes every block, so this matches query behavior uniformly. The only fall-through case is a brief race when the ExEx is mid-catchup.

### `debug_executionWitness`

Reth's existing algorithm with the state provider swapped:

```rust
async fn execution_witness(&self, block_id: BlockNumberOrTag) -> RpcResult<ExecutionWitness> {
    let block = self.eth_api.recovered_block(block_id).await?;
    let parent = block.parent_num_hash().number;

    let state_provider = self.proofs_state_factory
        .state_provider(BlockId::Number(parent.into())).await?;

    let db = StateProviderDatabase::new(&state_provider);
    let executor = self.evm_config.executor(db);

    let mut record = ExecutionWitnessRecord::default();
    executor.execute_with_state_closure(&block, |s| record.record_executed_state(s))?;

    let ExecutionWitnessRecord { hashed_state, codes, keys, lowest_block_number } = record;
    let state = state_provider.witness(Default::default(), hashed_state)?;

    let smallest = lowest_block_number.unwrap_or(block.number().saturating_sub(1));
    let headers = self.provider.headers_range(smallest..block.number())?;

    Ok(ExecutionWitness { state, codes, keys, headers: encode(headers), ..Default::default() })
}
```

Bound on Taiko's primitives (not op-reth's `OpPayloadPrimitives`/`OpHardforks`). Uses alethia-reth's EVM, so Taiko anchor + raiko2 witness recording kicks in during re-execution automatically.

### `eth_getProof`

Read-and-prove, no re-execution:

```rust
async fn get_proof(&self, addr: Address, slots: Vec<B256>, block: BlockId)
    -> RpcResult<EIP1186AccountProofResponse>
{
    let state_provider = self.proofs_state_factory.state_provider(block).await?;
    state_provider.proof(Default::default(), addr, &slots)
        .map(|proof| proof.into_eip1186_response(slots))
        .map_err(EthApiError::from)
}
```

Proof assembly (versioned cursor walks) lives in `proofs-trie::proof` and implements reth's standard `StateProofProvider` trait — RPC handler is agnostic to versioning.

### Interaction with `--rpc.eth-proof-window`

When `--proofs-history` is on, `--rpc.eth-proof-window` is **ignored for historical blocks the sidecar covers**. One-time INFO log at startup:

```
INFO proofs-history enabled; historical eth_getProof window is now controlled by
     --proofs-history.window (259200 blocks), superseding --rpc.eth-proof-window
```

The native window still applies if the sidecar doesn't cover a block (before init, after prune), preserving backward-compatible error behavior.

### Module layout

```
crates/rpc/src/
└── proofs/
    ├── mod.rs
    ├── state_factory.rs    # ProofsStateProviderFactory<Eth, Storage>
    ├── state_provider.rs   # ProofsStateProvider — implements reth StateProvider traits
    ├── debug.rs            # execution_witness override
    └── eth.rs              # get_proof override
```

---

## 6 / Node wiring

ExEx install and RPC overrides plug into `NodeBuilder` chain in `bin/alethia-reth/src/main.rs`. Sidecar activates only when `--proofs-history` is set.

### Helper module (bin crate)

New file `bin/alethia-reth/src/proofs_history.rs` wraps the builder chain:

```rust
pub async fn install_proofs_history<B>(
    builder: B,
    args: &ProofsHistoryArgs,
) -> eyre::Result<impl BuilderChain>
{
    if !args.enabled {
        return Ok(builder);  // no-op passthrough
    }

    let path = args.storage_path.as_ref().ok_or_else(||
        eyre!("--proofs-history.storage-path required"))?;
    let storage = Arc::new(MdbxProofsStorage::new(path)?);

    Ok(builder
        .on_node_started({
            let storage = storage.clone();
            move |node| {
                spawn_proofs_db_metrics(
                    node.task_executor,
                    storage,
                    node.config.metrics.push_gateway_interval,
                );
                Ok(())
            }
        })
        .install_exex("proofs-history", {
            let storage = storage.clone();
            let window = args.window;
            let prune_interval = args.prune_interval;
            move |exex_ctx| async move {
                Ok(ProofsExEx::builder(exex_ctx, storage.into())
                    .with_proofs_history_window(window)
                    .with_proofs_history_prune_interval(prune_interval)
                    .build()
                    .run()
                    .boxed())
            }
        })
        .extend_rpc_modules(move |ctx| {
            let state_factory = ProofsStateProviderFactory::new(
                ctx.registry.eth_api().clone(),
                storage.clone(),
            );
            let debug_ext = ProofsDebugApi::new(
                ctx.node().provider().clone(),
                ctx.registry.eth_api().clone(),
                state_factory.clone(),
                ctx.node().task_executor().clone(),
                ctx.node().evm_config().clone(),
            );
            let eth_ext = ProofsEthApi::new(
                ctx.registry.eth_api().clone(),
                state_factory,
            );

            ctx.modules.replace_configured(debug_ext.into_rpc())?;
            ctx.modules.replace_configured(eth_ext.into_rpc())?;
            Ok(())
        }))
}
```

### main.rs changes

```rust
let builder = builder.node(TaikoNode);
let builder = install_proofs_history(builder, &ext_args.proofs_history).await?;
let handle = builder
    .extend_rpc_modules(|ctx| { /* existing taiko_ + taikoAuth_ */ })
    .launch_with_debug_capabilities()
    .await?;
```

### Ordering

Sidecar `extend_rpc_modules` runs before the existing Taiko `extend_rpc_modules`. Both are non-colliding (override vs. merge), but "infrastructure overrides first, feature additions second" is the cleaner reading order.

### Zero cost when disabled

If `--proofs-history` is off, `install_proofs_history` returns the builder unchanged. No MDBX env opened, no ExEx spawned, no RPC override registered. Byte-identical to today.

### Clean shutdown

ExEx + pruner both respond to reth's graceful shutdown signal. Final MDBX commit + `FinishedHeight` ack before process exits. Next start resumes from `earliest/latest` pointers in `ProofWindow`.

---

## 7 / Configuration

### Flags (attached to `TaikoCliExtArgs`)

| Flag | Type | Default | Required | Notes |
|---|---|---|---|---|
| `--proofs-history` | `bool` | `false` | No | Master switch |
| `--proofs-history.storage-path` | `PathBuf` | — | **Yes** if enabled | Recommend `<datadir>/proofs` |
| `--proofs-history.window` | `u64` | `259_200` | No | 72 hours at 1s block time |
| `--proofs-history.prune-interval` | `humantime::Duration` | `15s` | No | Pruner cadence |
| `--proofs-history.prune-batch-size` | `u64` | `10_000` | No | Max blocks per prune tx |
| `--proofs-history.verification-interval` | `u64` | `0` | No | Full-exec integrity check every N; `0` = disabled |

### Environment variable fallbacks

Every flag also readable via `RETH_PROOFS_HISTORY_*` env var, matching reth convention.

### Validation (fail-fast at startup)

1. `storage-path` required when `--proofs-history` is on.
2. `window > 0` required.
3. `window > 10_000_000` → warn but don't reject.
4. If `earliest_stored` is not set, point operator at `alethia-reth proofs init`.

### Interaction with existing flags

When `--proofs-history` is on, `--rpc.eth-proof-window` is ignored for sidecar-covered blocks. Logged once at startup.

### Example k8s command

```yaml
exec ./alethia-reth node \
  --http \
  --http.addr 0.0.0.0 \
  --http.api eth,net,debug,trace,rpc \
  …
  --proofs-history \
  --proofs-history.storage-path /data/alethia-reth/proofs \
  --proofs-history.window 259200 \
  --proofs-history.prune-interval 15s \
  …
```

### Flag module layout

```
crates/cli/src/
├── parser.rs
├── args/
│   └── proofs_history.rs    # NEW: ProofsHistoryArgs with #[clap(...)] attrs
└── lib.rs                   # add `proofs_history: ProofsHistoryArgs` to TaikoCliExtArgs
```

---

## 8 / Operational concerns

### Storage sizing

Op-reth measured ~1 TB for 4 weeks of Base Sepolia (~2s block time, ~1.2M blocks) → ~0.83 MB per block. Taiko extrapolation:

| Window | Blocks (1s block time) | Estimated sidecar size |
|---|---|---|
| 72 hours (default) | 259,200 | ~215 GB |
| 7 days | 604,800 | ~500 GB |
| 14 days | 1,209,600 | ~1 TB |
| 30 days | 2,592,000 | ~2.15 TB |

Current hoodi PVC is **512 Gi**. At the 72h default the sidecar fits alongside the main chain DB with headroom — likely **no PVC change needed** for default-window deployments. Operators choosing a longer window (7d+) should plan a PVC resize.

1. For the 72h default: monitor actual sidecar size in staging; if the Base-derived estimate is low by 2× (~430 GB), PVC growth to `1024Gi` gives safe margin.
2. NVMe-class storage required regardless (pruner needs IOPS).
3. Numbers are Base-derived; Taiko per-block touch rate may differ ±2×. **Measure in staging before mainnet.**

### Metrics

New Prometheus metrics under `alethia_reth_proofs_history_*`:

- `_earliest_block` / `_latest_block` (gauges)
- `_storage_bytes` (gauge)
- `_blocks_processed_total` (counter)
- `_prune_blocks_total` / `_prune_duration_seconds`
- `_witness_request_duration_seconds` (histogram, 1ms..10s buckets)
- `_proof_request_duration_seconds` (histogram)
- `_sync_lag_blocks` — alert if > 100 for > 60s

Clone op-reth's Grafana dashboard (`etc/grafana/dashboards/op-proof-history.json`) and relabel panels.

### Init timing

Foreground blocking command. Estimated duration on current hoodi state: minutes. Mainnet scale could be hours. Resumable anchor ensures crashed init picks up where it left off.

**Rule:** always run `proofs init` against a **stopped** or newly-provisioned node — concurrent writes to the main DB during init cause MVCC retries.

### Forward-only limitation

Sidecar cannot serve blocks before its init block. The RPC handler must return a clear error (not silent fall-through to the slow reth path):

```
{ "code": -32000, "message": "block N is before proofs-history earliest block M;
  historical proofs not available (re-init with an earlier baseline to widen the window)" }
```

### Rollback path

Restart node without `--proofs-history`. MDBX env on disk untouched; main DB untouched; behavior reverts to today. No data migration, no schema surgery.

### Schema evolution

The `VersionedValue<T>` encoding is locked in. Future schema changes (add field, change encoding) require either a full re-init or version tagging. Design choice: **include a `SchemaVersion` metadata table from day one**. On open: if missing, write current version; if mismatched, fail with a clear message instructing re-init. Cheap insurance.

### Performance targets

- `eth_getProof` — p50 < 20ms, p99 < 200ms (vs. current 15s+ for old blocks)
- `debug_executionWitness` — p50 < 500ms, p99 < 2s (dominated by block re-execution)
- Sidecar write overhead per block — < 10ms, does not affect chain sync
- Pruner — never blocks the node, runs outside the ExEx canonical-notification loop

### Staged rollout

1. PR merges with flag OFF by default → zero production impact.
2. One hoodi node opts in → observe 48h, validate sidecar size growth.
3. All hoodi nodes opt in → run for a week, validate pruner keeps window bounded.
4. Mainnet canary → one node opts in → same observation cycle at mainnet scale.
5. Broad mainnet rollout.

Each step reversible by removing the flag.

---

## Testing strategy

### `proofs-trie`

- Unit tests per module (cursor behavior, versioned reads, tombstone handling, schema encoding round-trips).
- Integration test: write N blocks, query proof at block K, verify proof matches what reth's live-state generation would produce against the same block (correctness check against ground truth).
- Property test: randomized write/prune/query sequences, assert invariants (no orphaned change-set entries, `ProofWindow` bounds always match actual data range).

### `proofs-exex`

- ExEx test harness (`reth-exex-test-utils`) drives synthetic chain notifications including commits, reverts, reorgs at varying depths.
- Verify: after reorg, sidecar contains only canonical-chain versioned rows for affected range.
- Verify: pruner removes exactly blocks < `tip − window`.
- Verify: startup safety threshold triggers on oversized prune gap.

### RPC overrides

- End-to-end test launching a real (in-process) TaikoNode with sidecar enabled; issue `debug_executionWitness(N)` + `eth_getProof(addr, slots, N)` for in-window blocks; compare output to responses from a node without the sidecar.
- Out-of-window query returns explicit error (not silent fall-through).

### CLI subcommands

- `proofs init` against a seeded main DB, verify sidecar contains baseline at block 0 covering every account/slot in source.
- `proofs init` resumption: kill mid-run, re-run, verify it picks up from the last `InitialStateAnchor` key.
- `proofs prune` / `proofs unwind` with target boundaries, verify exact row-level outcomes.

### Performance validation (staging)

- Benchmark script issuing concurrent `eth_getProof` calls across the configured window; assert p50/p99 targets.
- Soak test: run for ≥ 48h with continuous RPC load and live chain ingestion; assert no memory growth, sidecar size bounded.

---

## Open risks

1. **Storage estimate is Base-derived.** Taiko's per-block state-touch distribution may differ materially. Must measure in staging before mainnet PVC sizing is finalized.
2. **op-reth is "Under Construction"** per its own README. If upstream changes the schema or cursor contract, we are on our own — no automatic upgrade path. Mitigated by `SchemaVersion` table.
3. **Pruner under high write load.** On a busy chain, prune random writes compete with ExEx writes for MDBX B-tree locks. Mitigated by NVMe requirement + batch-size tuning; validated by soak test.
4. **Init downtime on mainnet-scale state.** Could be hours. If this becomes a blocker, "warm init from a replica" is a future optimization (not in scope here).

---

## Open questions for reviewers

- Confirm storage budget: at the 72h default, the sidecar likely fits in the existing 512Gi PVC, but a 2× variance in per-block size (Base-derived vs Taiko-actual) could push past that. Is a PVC resize to `1024Gi` as a safety margin acceptable, or do we want to risk it at 512Gi and resize reactively if measurement shows we need to?
- Confirm no other RPCs we care about (e.g. `trace_*`, `debug_traceBlock*`) are silently dependent on fast historical state. If they are, either the sidecar needs to serve them too or we document they stay slow.
- Confirm `--rpc.eth-proof-window` override behavior is acceptable (sidecar window supersedes).
