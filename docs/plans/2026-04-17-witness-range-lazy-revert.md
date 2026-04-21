# Witness Range Lazy Revert Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Rewrite `taiko_executionWitnessRange` so a single range request reuses one lazily computed historical revert overlay instead of eagerly precomputing a huge shared base.

**Architecture:** Add a request-local cached historical provider that mirrors upstream historical state semantics but memoizes `revert_state()` for the lifetime of one range call. Rebuild the range RPC on top of that provider plus `MemoryOverlayStateProvider`, so the first block stays close to the upstream single-block path and later blocks only add incremental work.

**Tech Stack:** Rust, `reth-provider`, `reth-chain-state`, `reth-revm`, `reth-trie`, `jsonrpsee`

---

### Task 1: Add The Cached Historical Provider Module

**Files:**
- Create: `crates/rpc/src/eth/cached_historical.rs`
- Modify: `crates/rpc/src/eth/mod.rs`
- Test: `crates/rpc/src/eth/cached_historical.rs`

**Step 1: Write the failing tests**

Add unit tests for a small cache helper in `cached_historical.rs` that verify:

- the loader closure runs only once even when called repeatedly
- the cached value is shared across cloned `Arc` handles

Use an atomic counter in the tests so the failure is unambiguous.

**Step 2: Run test to verify it fails**

Run: `cargo test -p alethia-reth-rpc --lib cached_historical`

Expected: FAIL because the module and tests do not exist yet.

**Step 3: Write minimal implementation**

Implement:

- a request-local cache helper that stores `Arc<HashedPostStateSorted>`
- a `CachedHistoricalStateProvider<P>` wrapper that:
  - owns the DB provider and historical block number
  - delegates account/storage/blockhash/bytecode lookups to an inner `HistoricalStateProvider<P>`
  - lazily computes `from_reverts_auto(block_number..)` and reuses it
  - implements the `StateRootProvider` and `StateProofProvider` methods needed by witness generation using the cached revert state

**Step 4: Run test to verify it passes**

Run: `cargo test -p alethia-reth-rpc --lib cached_historical`

Expected: PASS for the new cache helper tests.

**Step 5: Commit**

```bash
git add crates/rpc/src/eth/cached_historical.rs crates/rpc/src/eth/mod.rs
git commit -m "feat(rpc): add cached historical witness provider"
```

### Task 2: Replace The Eager Shared-Base Range Path

**Files:**
- Modify: `crates/rpc/src/eth/eth.rs`
- Test: `crates/rpc/src/eth/eth.rs`

**Step 1: Write the failing test**

Add a focused unit test for the new range execution setup that validates the RPC still rejects:

- `start_block == 0`
- reversed bounds

and that the helper building the initial range anchor chooses `start - 1`.

Use a small pure helper for the anchor calculation so the test is deterministic.

**Step 2: Run test to verify it fails**

Run: `cargo test -p alethia-reth-rpc --lib anchor_block_number`

Expected: FAIL because the helper and test do not exist yet.

**Step 3: Write minimal implementation**

In `eth.rs`:

- remove:
  - `HistoricalWitnessBase`
  - `WitnessRangeOverlay`
  - `historical_witness_base`
  - manual trie cursor / witness assembly helpers
- add a helper that builds one `Arc<CachedHistoricalStateProvider<_>>` for the request
- change `execution_state_provider(...)` to:
  - return the cached historical provider for the first block
  - return `MemoryOverlayStateProvider` over the same cached provider plus prior `ExecutedBlock`s for later blocks
- change block witness generation to:
  - execute the block
  - call `db.database.0.witness(Default::default(), hashed_state.clone())`
  - call `db.database.0.state_root_with_updates(hashed_state.clone())`
  - verify the computed state root matches the block header
  - store the resulting `ExecutedBlock` for the next block

**Step 4: Run test to verify it passes**

Run: `cargo test -p alethia-reth-rpc --lib anchor_block_number`

Expected: PASS for the new helper test.

**Step 5: Commit**

```bash
git add crates/rpc/src/eth/eth.rs
git commit -m "refactor(rpc): reuse lazy historical revert in witness range"
```

### Task 3: Remove Dead Wiring And Tighten Docs

**Files:**
- Modify: `bin/alethia-reth/src/main.rs`
- Modify: `crates/rpc/src/eth/eth.rs`
- Test: `cargo check -p alethia-reth-bin`

**Step 1: Write the failing test**

Run the binary type-check before removing dead dependencies.

Run: `cargo check -p alethia-reth-bin`

Expected: FAIL or warn once the RPC constructor changes and the old `ChangesetCache` wiring no longer matches.

**Step 2: Write minimal implementation**

- remove `ChangesetCache` from the Taiko RPC constructor and stored fields
- update constructor call sites in `bin/alethia-reth/src/main.rs`
- refresh doc comments so the new code explains the lazy request-local cache rather than the eager shared-base approach

**Step 3: Run test to verify it passes**

Run: `cargo check -p alethia-reth-bin`

Expected: PASS.

**Step 4: Commit**

```bash
git add bin/alethia-reth/src/main.rs crates/rpc/src/eth/eth.rs
git commit -m "chore(rpc): remove eager witness range wiring"
```

### Task 4: Verify The RPC Crate End To End

**Files:**
- Test: `crates/rpc/src/eth/cached_historical.rs`
- Test: `crates/rpc/src/eth/eth.rs`

**Step 1: Run the RPC unit test suite**

Run: `cargo test -p alethia-reth-rpc --lib`

Expected: PASS.

**Step 2: Run linting**

Run: `cargo clippy -p alethia-reth-rpc --all-features --lib -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items`

Expected: PASS.

**Step 3: Record the deployment checks to run after release**

Document these smoke tests in the final handoff:

- `debug_executionWitness(start)`
- `taiko_executionWitnessRange(start, start)`
- `taiko_executionWitnessRange(start, end)`
- split-range comparison on the same historical window

**Step 4: Commit**

```bash
git add docs/plans/2026-04-17-witness-range-lazy-revert-design.md docs/plans/2026-04-17-witness-range-lazy-revert.md
git commit -m "docs: capture lazy revert witness range plan"
```
