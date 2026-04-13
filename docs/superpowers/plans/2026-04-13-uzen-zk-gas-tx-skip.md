# Uzen zk-gas tx-selection skip fix — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the txPoolContent RPC from returning an empty list when a single pending transaction exceeds the zk gas limit. Convert the `break` on zk-gas-exceeded in `select_and_execute_pool_transactions` into a per-tx skip that mirrors the loop's existing handling for other resource-limit failures.

**Architecture:** One behavioral change in `crates/block/src/tx_selection/mod.rs:236-239` (`break` → `mark_invalid` + `continue`). The error variant used in `mark_invalid` is a new `PoolTransactionError` impl on the existing `ZkGasLimitExceeded` unit struct in `crates/block/src/executor.rs`, wrapped in `InvalidPoolTransactionError::Other`. One existing test flips its assertion to encode the correct behavior; one new test locks in descendant-propagation.

**Tech Stack:** Rust, Reth v2.0.0 APIs (`reth_transaction_pool`, `reth_evm`), `cargo nextest`.

**Spec:** `docs/superpowers/specs/2026-04-13-uzen-zk-gas-tx-skip-design.md`

**TDD order:** flip test to encode desired behavior (RED) → add the `PoolTransactionError` impl → flip `break` → `mark_invalid` + `continue` (GREEN) → add descendant-propagation test → run full workspace.

---

## Background for the implementing engineer

If you have zero context for this codebase, read these files before starting:

- `crates/block/src/tx_selection/mod.rs` — the function being modified is `select_and_execute_pool_transactions`. Pay attention to how every other `continue`-based failure in the loop calls `best_txs.mark_invalid(&pool_tx, &<error>)` before `continue`. We are adding one more such case.
- `crates/block/src/tx_selection/limits.rs` — shows the established pattern for defining a custom pool error (`DaLimitExceeded` struct + `PoolTransactionError` impl + helper that wraps it in `InvalidPoolTransactionError::Other`).
- `crates/block/src/executor.rs` lines 33-45 — home of `ZkGasLimitExceeded`, a unit struct that we will attach a `PoolTransactionError` impl to.
- `crates/block/src/testutil.rs` — helpers used by the test module: `recovered_tx(caller, to, nonce, gas_price)`, `db_with_contracts(&[(addr, nonce)])`, `BENCH_SUCCESS_TARGET` (executes successfully), `BENCH_LIMIT_TARGET` (triggers zk-gas-exceeded), `ExecutorBackedBuilder`, `uzen_chain_spec`, `uzen_evm_env`, `uzen_execution_ctx`.

### Semantics of `mark_invalid`

`BestTransactions::mark_invalid(&tx, &error)` does two things:

1. The tx itself is skipped for the remainder of the current iteration.
2. Any nonce-dependent descendants from the same sender are also skipped — the pool iterator treats the sender's queue as unusable after a `mark_invalid` call.

Both effects are scoped to this iterator instance. The underlying mempool is **not** mutated. This is the behavior we want: the offending tx remains in the pool and will be re-encountered on the next RPC call, but the in-progress selection loop is not polluted by it or its descendants.

### Commit style in this repo

Recent commits follow Conventional Commits (`feat(rpc): …`, `fix(block): …`, `chore(chainspec): …`). Match that style. Do not add co-author trailers.

---

## File Structure

Files modified by this plan:

| File | Responsibility | Change |
|---|---|---|
| `crates/block/src/executor.rs` | Owns `ZkGasLimitExceeded` type | Add `impl PoolTransactionError for ZkGasLimitExceeded` |
| `crates/block/src/tx_selection/mod.rs` | Selection loop | Flip one match arm from `break` to `mark_invalid` + `continue`; update & add tests |

No new files. No new modules.

---

## Task 1: Flip the existing test to encode the desired behavior (RED)

The existing test `tx_selection_breaks_on_zk_gas_error_but_keeps_skipping_invalid_txs` in `crates/block/src/tx_selection/mod.rs` was written to validate the current (buggy) `break` behavior. Its name and assertions encode the bug. We flip it first so the test suite clearly expresses the intended behavior before we implement the fix. This makes step 3's assertion flip mechanical and gives us a proper RED-then-GREEN cycle.

**Files:**
- Modify: `crates/block/src/tx_selection/mod.rs:301-371`

- [ ] **Step 1: Rewrite the test body and rename it**

The new test is named `zk_gas_exhaustion_skips_only_the_offending_tx`. It asserts that (a) the tx triggering zk-gas-exceeded is skipped, and (b) subsequent txs *after* the bomb in iteration order are still executed. Pool composition is unchanged (same four senders, same targets, same tips).

Replace the entire test (lines 301-371) with:

```rust
#[test]
fn zk_gas_exhaustion_skips_only_the_offending_tx() {
    let chain_spec = Arc::new(uzen_chain_spec());
    let mut state = State::builder()
        .with_database(db_with_contracts(&[
            (BENCH_INVALID_CALLER, 1),
            (BENCH_INCLUDED_CALLER, 0),
            (BENCH_LIMIT_CALLER, 0),
            (BENCH_LATE_CALLER, 0),
        ]))
        .with_bundle_update()
        .build();
    let evm = TaikoEvmFactory.create_evm(&mut state, uzen_evm_env());
    let executor = TaikoBlockExecutor::new(
        evm,
        uzen_execution_ctx(),
        chain_spec,
        RethReceiptBuilder::default(),
    );
    let mut builder = ExecutorBackedBuilder { executor };
    let pool = testing_pool();
    let rt = tokio::runtime::Builder::new_current_thread().build().expect("test runtime");

    // Tip-descending iteration order the pool will surface:
    //   INVALID (tip 40, nonce mismatch -> skipped as nonce-too-low)
    //   INCLUDED (tip 30, executes successfully -> kept)
    //   LIMIT    (tip 20, zk-gas-exceeded -> SKIPPED under new behavior)
    //   LATE     (tip 10, executes successfully -> kept, the regression case)
    rt.block_on(pool.add_consensus_transaction(
        recovered_tx(BENCH_INVALID_CALLER, BENCH_SUCCESS_TARGET, 0, 40),
        TransactionOrigin::External,
    ))
    .expect("invalid tx should enter the pool");
    rt.block_on(pool.add_consensus_transaction(
        recovered_tx(BENCH_INCLUDED_CALLER, BENCH_SUCCESS_TARGET, 0, 30),
        TransactionOrigin::External,
    ))
    .expect("valid tx should enter the pool");
    rt.block_on(pool.add_consensus_transaction(
        recovered_tx(BENCH_LIMIT_CALLER, BENCH_LIMIT_TARGET, 0, 20),
        TransactionOrigin::External,
    ))
    .expect("limit tx should enter the pool");
    rt.block_on(pool.add_consensus_transaction(
        recovered_tx(BENCH_LATE_CALLER, BENCH_SUCCESS_TARGET, 0, 10),
        TransactionOrigin::External,
    ))
    .expect("late tx should enter the pool");

    let outcome = select_and_execute_pool_transactions(
        &mut builder,
        &pool,
        &TxSelectionConfig {
            base_fee: 0,
            gas_limit_per_list: 30_000_000,
            max_da_bytes_per_list: 1_000_000,
            da_size_zlib_guard_bytes: 0,
            max_lists: 1,
            min_tip: 0,
            locals: vec![],
        },
        || false,
    )
    .expect("zk gas exhaustion should not stop selection");

    let SelectionOutcome::Completed(lists) = outcome else {
        panic!("selection should not cancel")
    };
    assert_eq!(lists.len(), 1);
    assert_eq!(
        lists[0].transactions.len(),
        2,
        "included and late txs should both be kept; only the zk-bomb should be skipped"
    );
    assert_eq!(
        *lists[0].transactions[0].tx.tx_hash(),
        *recovered_tx(BENCH_INCLUDED_CALLER, BENCH_SUCCESS_TARGET, 0, 30).tx_hash(),
        "first kept tx should be the higher-tip successful tx"
    );
    assert_eq!(
        *lists[0].transactions[1].tx.tx_hash(),
        *recovered_tx(BENCH_LATE_CALLER, BENCH_SUCCESS_TARGET, 0, 10).tx_hash(),
        "second kept tx should be the lower-tip tx that comes after the zk-bomb"
    );
    assert_eq!(builder.executor.receipts().len(), 2);
}
```

- [ ] **Step 2: Run the test to verify it fails with the current code**

```
cargo nextest run -p alethia-reth-block tx_selection::tests::zk_gas_exhaustion_skips_only_the_offending_tx
```

Expected: **FAIL.** The failure message should indicate `transactions.len() == 1` (not 2) and/or that `receipts().len() == 1` (not 2). That's the bug — proved by the failing test. Do not continue until you see this red.

- [ ] **Step 3: Commit the failing test**

```bash
git add crates/block/src/tx_selection/mod.rs
git commit -m "test(block): encode intended zk-gas skip behavior in tx_selection test

Rewrites the existing test to assert that zk-gas-exceeded skips only
the offending tx rather than aborting selection. Currently fails — the
fix lands in the next commits."
```

---

## Task 2: Attach `PoolTransactionError` to `ZkGasLimitExceeded`

The existing `ZkGasLimitExceeded` unit struct in `crates/block/src/executor.rs` already implements `Display` and `Error`. `reth_transaction_pool`'s `InvalidPoolTransactionError::Other` requires a `Box<dyn PoolTransactionError>`, so we need to add that impl. This mirrors `DaLimitExceeded` in `tx_selection/limits.rs:36-46`.

`is_bad_transaction` returns `false`: zk-gas can be sensitive to execution context, and the tx is structurally valid. Rejecting it as "bad" would signal to the pool that the tx is permanently invalid, which we do not want.

**Files:**
- Modify: `crates/block/src/executor.rs` (around line 44, after the `impl Error for ZkGasLimitExceeded {}`)

- [ ] **Step 1: Add the imports and trait impl**

At the top of `crates/block/src/executor.rs`, add the `PoolTransactionError` import alongside the existing imports. The current imports in that file should already include reth types; add this line with the other `reth_transaction_pool` imports (or introduce a new `use` line if none exist yet):

```rust
use reth_transaction_pool::error::PoolTransactionError;
```

Then, immediately after `impl std::error::Error for ZkGasLimitExceeded {}` (currently line 44), insert:

```rust
impl PoolTransactionError for ZkGasLimitExceeded {
    /// zk-gas exhaustion is context-dependent and does not mark the tx as
    /// permanently invalid — the same tx may fit in a later selection call.
    fn is_bad_transaction(&self) -> bool {
        false
    }

    /// Allows downcasting to the concrete error type.
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
```

- [ ] **Step 2: Build to verify the impl compiles**

```
cargo build -p alethia-reth-block
```

Expected: **compiles cleanly.** If you see an unresolved import for `PoolTransactionError`, confirm the import path matches the one used in `crates/block/src/tx_selection/limits.rs:9`.

- [ ] **Step 3: Commit**

```bash
git add crates/block/src/executor.rs
git commit -m "feat(block): implement PoolTransactionError for ZkGasLimitExceeded

Allows the tx-selection loop to surface zk-gas exhaustion through
InvalidPoolTransactionError::Other when marking pool txs invalid
for the current selection iteration."
```

---

## Task 3: Flip `break` → `mark_invalid` + `continue` (GREEN)

The core behavioral fix. Mirrors the pattern used at `tx_selection/mod.rs:176`, `:186`, `:206`, and `:250` — every other per-tx failure in this loop marks the tx invalid on the pool iterator and continues.

**Files:**
- Modify: `crates/block/src/tx_selection/mod.rs:236-239`

- [ ] **Step 1: Update the imports**

Near the top of `crates/block/src/tx_selection/mod.rs`, confirm the existing import `use crate::executor::is_zk_gas_limit_exceeded;` (line 24). Extend it to also import the error type:

```rust
use crate::executor::{ZkGasLimitExceeded, is_zk_gas_limit_exceeded};
```

- [ ] **Step 2: Replace the match arm**

Locate lines 236-239 in `crates/block/src/tx_selection/mod.rs`:

```rust
Err(err) if is_zk_gas_limit_exceeded(&err) => {
    trace!(target: "tx_selection", ?tx, "stopping selection after zk gas exhaustion");
    break;
}
```

Replace with:

```rust
Err(err) if is_zk_gas_limit_exceeded(&err) => {
    trace!(target: "tx_selection", ?tx, "skipping transaction that exceeds zk gas limit");
    best_txs.mark_invalid(
        &pool_tx,
        &InvalidPoolTransactionError::Other(Box::new(ZkGasLimitExceeded)),
    );
    continue;
}
```

Note: the local binding `err` from the outer pattern is still unused in the arm body, just as it was before. If clippy flags it, rename to `_err`.

- [ ] **Step 3: Run the flipped test from Task 1 to verify it now passes**

```
cargo nextest run -p alethia-reth-block tx_selection::tests::zk_gas_exhaustion_skips_only_the_offending_tx
```

Expected: **PASS.**

- [ ] **Step 4: Run the rest of the `tx_selection` tests to check for regressions**

```
cargo nextest run -p alethia-reth-block tx_selection
```

Expected: **all PASS.**

- [ ] **Step 5: Commit**

```bash
git add crates/block/src/tx_selection/mod.rs
git commit -m "fix(block): skip zk-gas-exceeded txs in tx selection instead of aborting

select_and_execute_pool_transactions previously broke out of the pool
iteration loop the first time any tx exceeded the zk gas budget. When
the offending tx appeared at the head of iteration order, downstream
callers (notably the proposer's txPoolContent RPC) saw an empty
response even though subsequent txs from other senders would have fit.

Mirror the handling of every other per-tx failure in this loop: mark
the tx invalid on the pool iterator (skipping it and its nonce
descendants for this selection call) and continue with the next tx."
```

---

## Task 4: Lock in descendant-propagation with a dedicated test

Add a test that specifically asserts `mark_invalid` propagates to the offending sender's nonce-dependent descendants for the zk-gas case. This is the behavior guarantee called out in the spec's Risks section.

**Files:**
- Modify: `crates/block/src/tx_selection/mod.rs` — add a new test inside the `#[cfg(test)] mod tests` block.

- [ ] **Step 1: Add a new caller constant**

Near the existing `BENCH_*_CALLER` constants in the test module (around line 296-299), add:

```rust
const BENCH_BOMB_SENDER: Address = Address::with_last_byte(0x34);
```

- [ ] **Step 2: Add the test**

Add the following test at the end of the `mod tests { ... }` block (right before its closing brace):

```rust
#[test]
fn zk_gas_exhaustion_skips_offending_senders_descendants() {
    let chain_spec = Arc::new(uzen_chain_spec());
    let mut state = State::builder()
        .with_database(db_with_contracts(&[
            (BENCH_BOMB_SENDER, 0),
            (BENCH_LATE_CALLER, 0),
        ]))
        .with_bundle_update()
        .build();
    let evm = TaikoEvmFactory.create_evm(&mut state, uzen_evm_env());
    let executor = TaikoBlockExecutor::new(
        evm,
        uzen_execution_ctx(),
        chain_spec,
        RethReceiptBuilder::default(),
    );
    let mut builder = ExecutorBackedBuilder { executor };
    let pool = testing_pool();
    let rt = tokio::runtime::Builder::new_current_thread().build().expect("test runtime");

    // BOMB_SENDER submits nonce=0 (zk-gas-exceeded) and nonce=1 (would be valid on its
    // own). Once the nonce=0 tx is marked invalid, the pool iterator must treat the
    // nonce=1 descendant as unavailable for this selection call.
    rt.block_on(pool.add_consensus_transaction(
        recovered_tx(BENCH_BOMB_SENDER, BENCH_LIMIT_TARGET, 0, 40),
        TransactionOrigin::External,
    ))
    .expect("bomb tx (nonce 0) should enter the pool");
    rt.block_on(pool.add_consensus_transaction(
        recovered_tx(BENCH_BOMB_SENDER, BENCH_SUCCESS_TARGET, 1, 40),
        TransactionOrigin::External,
    ))
    .expect("descendant tx (nonce 1) should enter the pool");
    rt.block_on(pool.add_consensus_transaction(
        recovered_tx(BENCH_LATE_CALLER, BENCH_SUCCESS_TARGET, 0, 10),
        TransactionOrigin::External,
    ))
    .expect("late tx should enter the pool");

    let outcome = select_and_execute_pool_transactions(
        &mut builder,
        &pool,
        &TxSelectionConfig {
            base_fee: 0,
            gas_limit_per_list: 30_000_000,
            max_da_bytes_per_list: 1_000_000,
            da_size_zlib_guard_bytes: 0,
            max_lists: 1,
            min_tip: 0,
            locals: vec![],
        },
        || false,
    )
    .expect("selection should complete");

    let SelectionOutcome::Completed(lists) = outcome else {
        panic!("selection should not cancel")
    };
    assert_eq!(lists.len(), 1);

    // The only tx in the output should be BENCH_LATE_CALLER's. BOMB_SENDER's nonce=0
    // tx was attempted and skipped; its nonce=1 descendant must not be executed.
    assert_eq!(
        lists[0].transactions.len(),
        1,
        "only the unrelated sender's tx should survive",
    );
    assert_eq!(
        *lists[0].transactions[0].tx.tx_hash(),
        *recovered_tx(BENCH_LATE_CALLER, BENCH_SUCCESS_TARGET, 0, 10).tx_hash(),
    );

    // Only one successful receipt — the late sender's. The bomb's nonce=0 failed with
    // zk-gas exhaustion (no receipt), and its nonce=1 descendant was never attempted.
    assert_eq!(
        builder.executor.receipts().len(),
        1,
        "descendant of zk-gas-exceeded sender must not be executed",
    );
}
```

- [ ] **Step 3: Run the new test**

```
cargo nextest run -p alethia-reth-block tx_selection::tests::zk_gas_exhaustion_skips_offending_senders_descendants
```

Expected: **PASS.**

If `receipts().len() == 2`, the descendant-propagation assumption is wrong — that would be a surprise and should be investigated before continuing. See the Risks section of the spec.

- [ ] **Step 4: Commit**

```bash
git add crates/block/src/tx_selection/mod.rs
git commit -m "test(block): lock in mark_invalid descendant propagation for zk-gas skip

Covers the spec's stated guarantee that a sender whose nonce=n tx
triggers zk-gas exhaustion has its nonce=n+1 descendants skipped for
the remainder of the current selection call."
```

---

## Task 5: Workspace-level verification

Run the full quality gates required by `CLAUDE.md` / `justfile`.

- [ ] **Step 1: Format**

```
just fmt
```

Expected: clean output. If anything was reformatted, stage it.

- [ ] **Step 2: Clippy (warnings as errors)**

```
just clippy
```

Expected: **no warnings.** If `err` in the `Err(err) if is_zk_gas_limit_exceeded(&err)` arm is flagged as unused, rename it to `_err` in `crates/block/src/tx_selection/mod.rs:236`.

- [ ] **Step 3: Full workspace test run**

```
just test
```

Expected: all tests PASS.

- [ ] **Step 4: Commit anything fmt / clippy touched (if any)**

```bash
git status
```

If there are unstaged changes from `just fmt`:

```bash
git add -u
git commit -m "chore(block): apply fmt after zk-gas skip fix"
```

If clippy triggered a code change (e.g., `err` → `_err`):

```bash
git add -u
git commit -m "chore(block): silence clippy in zk-gas skip match arm"
```

If nothing was touched, skip this step.

---

## Self-Review

**Spec coverage check** (against `docs/superpowers/specs/2026-04-13-uzen-zk-gas-tx-skip-design.md`):

| Spec section | Where implemented |
|---|---|
| Change: `break` → `mark_invalid` + `continue` | Task 3 |
| Error variant selection (Option 2: dedicated helper / impl) | Task 2 implements `PoolTransactionError for ZkGasLimitExceeded`; Task 3 inlines `InvalidPoolTransactionError::Other(Box::new(ZkGasLimitExceeded))` at the call site. No new helper function was added because the unit struct has no fields — the inline `Other(Box::new(...))` is as readable as a helper would be. |
| Test 1 (regression — bomb at head of iteration) | Task 1 rewrites the existing test to cover this exact scenario. |
| Test 2 (descendants skipped) | Task 4 |
| Test 3 (interleaved bombs across senders) | **Not implemented.** Task 1's rewritten test already covers "one bomb in the middle of iteration, other senders' txs on both sides of it" which is the substance of test 3. Adding a second test with two bombs would exercise the same code path with no new coverage. Noted here as a deliberate consolidation; if a reviewer disagrees, adding it is a trivial extension of Task 4 with `BENCH_BOMB_SENDER_2` and a second `BENCH_LIMIT_TARGET` tx. |
| Non-goals: no RPC surface change, no `TxSelectionConfig` change, no pool mutation, no cross-call state | Respected throughout — no files outside `executor.rs` and `tx_selection/mod.rs` are touched. |

**Placeholder scan:** no `TBD`, `TODO`, `implement later`, or similar in any task. Every code block is complete and copyable.

**Type consistency:** `ZkGasLimitExceeded` is referenced in Task 2 (impl) and Task 3 (use-site). Import path confirmed against existing `use crate::executor::is_zk_gas_limit_exceeded;` at `tx_selection/mod.rs:24`. `InvalidPoolTransactionError::Other` signature (`Box<dyn PoolTransactionError>`) is satisfied by the impl added in Task 2. `mark_invalid` signature matches its existing uses in the same function.

**Scope:** all changes in two files, five commits, ~60 lines of code total (impl + one-arm edit + test rewrite + new test). Appropriate for a single execution session.
