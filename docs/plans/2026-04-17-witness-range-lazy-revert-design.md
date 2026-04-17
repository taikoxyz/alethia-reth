# Witness Range Lazy Revert Design

## Goal

Replace the current `taiko_executionWitnessRange` implementation with a single-call design that:

- keeps `taiko_executionWitnessRange(start, start)` close to upstream `debug_executionWitness`
- avoids paying a large up-front preprocessing cost before the first block
- reuses the expensive historical `revert_state()` work across the blocks inside one range request

## Why The First Range Design Failed

The first range implementation tried to amortize history work by precomputing a shared historical base:

- `changeset_cache.get_or_compute_range(start..=tip)` for trie-node reverts
- `from_reverts_auto(start..)` for hashed-state reverts

That design looked attractive on paper, but in practice it moved a large amount of work in front of the first block. The deployment benchmark showed the failure mode clearly:

- `debug_executionWitness(block)` around `head-3000` returned in about `2.9s`
- `taiko_executionWitnessRange(block, block)` timed out at `180s`

So the issue is not that “range RPC is impossible”; it is that the implementation introduced a large fixed tax that upstream single-block witness generation does not pay.

## Constraints

- No two-step “prepare/handle” flow
- No separate prewarm RPC
- The optimization must live behind a single `taiko_executionWitnessRange(start, end)` call
- Archive-only is acceptable
- Correctness is more important than chasing an aggressive theoretical speedup

## Source Of The Real Cost

For historical blocks, upstream `reth` builds witnesses through a historical provider. The expensive part is not ordinary RPC cache misses; it is the historical state reconstruction in `revert_state()`:

- `HistoricalStateProviderRef::witness()` prepends `self.revert_state()?`
- `revert_state()` calls `from_reverts_auto(self.provider, self.block_number..)`
- `from_reverts_auto` scans account/storage changesets from the target block up to tip and reconstructs the historical hashed-state overlay

This means single-block witness generation pays the historical revert cost once per block.

## Chosen Design

### Overview

Keep the range API single-call, but move caching inside the request-local historical provider instead of precomputing a giant shared base.

The new implementation will:

1. Anchor the entire request at `start - 1`
2. Build a request-local `CachedHistoricalStateProvider`
3. Let that provider lazily compute `revert_state()` on first witness/state-root usage
4. Cache that reverted hashed-state overlay for the rest of the range
5. Use `MemoryOverlayStateProvider` to layer already executed blocks on top of that cached historical base

### Why This Is Better

- The first block stays close to the upstream path:
  - execute block on parent state
  - call `witness(Default::default(), hashed_state)`
- The expensive revert is still paid once, but only when the first block actually needs it
- Later blocks reuse the same reverted historical state instead of recalculating `from_reverts_auto(start..tip)` again
- The design removes the large eager preprocessing tax that made `range(start, start)` unusable

## Main Components

### 1. `CachedHistoricalStateProvider`

Add a new request-local provider wrapper in `crates/rpc/src/eth/`.

Responsibilities:

- own the database provider and historical block number for `start - 1`
- mirror the public behavior of upstream historical providers
- lazily compute `from_reverts_auto(block_number..)` once
- store the resulting `HashedPostStateSorted` in a shared cache
- reuse the cached revert for:
  - `witness`
  - `proof`
  - `multiproof`
  - `state_root_from_nodes`
  - `state_root_from_nodes_with_updates`

Non-goals:

- no cross-request cache
- no persistence
- no speculative trie-node precomputation

### 2. Request-Local Shared Ownership

The cached provider will be wrapped in `Arc<_>` and boxed as `StateProviderBox` when needed.

This matters because each block execution still needs a fresh state provider instance:

- first block uses only the cached historical provider
- later blocks use `MemoryOverlayStateProvider::new(base_provider, executed_blocks_so_far)`

Using `Arc<CachedHistoricalStateProvider<_>>` lets all those per-block provider boxes share the same cached revert state.

### 3. Range Execution Flow

For each block in `[start, end]`:

1. Build the execution provider
   - first block: `CachedHistoricalStateProvider`
   - later blocks: `MemoryOverlayStateProvider` on top of the same cached provider plus prior executed blocks
2. Execute the block
3. Record the execution witness access set
4. Ask the active state provider for `witness(Default::default(), hashed_state)`
5. Ask the active state provider for `state_root_with_updates(hashed_state)`
6. Verify the returned state root matches the canonical block header
7. Store the block as an `ExecutedBlock` so the next block can reuse its overlay

This deliberately reuses upstream provider semantics instead of manually assembling trie cursors and overlay state in the RPC method.

## Expected Performance Shape

Expected latency pattern:

- `range(start, start)` should be near single-block `debug_executionWitness`
- `range(start, start + n)` should be roughly:
  - one historical revert
  - plus `n + 1` executions
  - plus `n + 1` witness computations
  - plus `n + 1` state-root-with-updates computations

This does not make witness generation cheap, but it should remove the worst inefficiency:

- old upstream pattern inside a range: `N` historical reverts
- chosen design: `1` historical revert per range request

## Why Split Ranges Still Lose

If the client splits `[100, 250]` into:

- `[100, 150]`
- `[151, 200]`
- `[201, 250]`

then each sub-range creates its own cached provider and pays its own first historical revert.

So even with the new design:

- one larger contiguous range should beat several smaller ranges
- but each sub-range should still be better than calling the single-block RPC for every block

## Correctness Guardrails

- Only operate on contiguous canonical blocks
- Recompute `state_root_with_updates` for every executed block and compare with the canonical header
- Reuse upstream `StateProvider` / `MemoryOverlayStateProvider` semantics instead of inventing a parallel witness algorithm
- Keep the public response format unchanged

## File-Level Changes

- `crates/rpc/src/eth/eth.rs`
  - remove the eager historical base path
  - route range execution through the cached provider
- `crates/rpc/src/eth/cached_historical.rs`
  - add the request-local cached historical provider implementation
- `crates/rpc/src/eth/mod.rs`
  - export the new internal module
- `bin/alethia-reth/src/main.rs`
  - remove the range RPC’s unused `ChangesetCache` wiring

## Validation Plan

Local validation:

- targeted unit tests for lazy cache behavior
- `cargo test -p alethia-reth-rpc --lib`
- `cargo check -p alethia-reth-bin`
- `cargo clippy -p alethia-reth-rpc --all-features --lib -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items`

Deployment validation:

- compare `debug_executionWitness(start)` vs `taiko_executionWitnessRange(start, start)`
- compare a contiguous historical window as:
  - per-block singles
  - one large range
  - several smaller ranges

The success criterion is not “perfect scaling”; it is that the new range RPC stops being dramatically worse than single-block witness generation and begins to win on contiguous windows.
