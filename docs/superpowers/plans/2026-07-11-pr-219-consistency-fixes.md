# PR 219 Consistency Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ensure every applied canonical forkchoice persists its buffered L1 origin and make startup reconciliation abort safely on any incomplete read or repair.

**Architecture:** Keep the existing buffered-origin design and add one decision helper that classifies both normal `VALID` results and reth's applied-forkchoice invalid-attributes error as promotion outcomes. Make the head resolver's predicate fallible, propagate reconciliation errors through `TaikoEngineApi::new`, and centralize take/persist/re-buffer behavior in one method.

**Tech Stack:** Rust, Tokio, reth Engine API, MDBX-backed reth providers, Cargo tests/clippy/rustfmt.

## Global Constraints

- Every non-test production Rust symbol and associated item must have purpose/contract documentation.
- Preserve the accepted residual for at-or-below-head `BatchToLastBlock` mappings after same-height reorgs.
- Write each regression test first and observe the intended failure before production edits.
- Run `just fmt`, `just clippy`, and `just test` before publishing.
- Remove `docs/superpowers` from the final tree before pushing.

---

### Task 1: Persist Pending Origins After Applied Forkchoice Errors

**Files:**
- Modify: `crates/rpc/src/engine/api.rs:13-35, 175-306, 593-732`
- Test: `crates/rpc/src/engine/api.rs` inline test module

**Interfaces:**
- Consumes: `EngineApiError::ForkChoiceUpdate`, `BeaconForkChoiceUpdateError`, and `ForkchoiceUpdateError::UpdatedInvalidPayloadAttributes`.
- Produces: `forkchoice_applied(&Result<ForkchoiceUpdated, EngineApiError>) -> bool` and `persist_pending_l1_origin(&self, B256) -> Result<(), EngineApiError>`.

- [ ] **Step 1: Write the failing applied-error classification tests**

Add tests that construct the exact pinned-reth error and an unrelated internal error:

```rust
#[test]
fn invalid_payload_attributes_error_reports_an_applied_forkchoice() {
    let result = Err(EngineApiError::ForkChoiceUpdate(
        BeaconForkChoiceUpdateError::ForkchoiceUpdateError(
            ForkchoiceUpdateError::UpdatedInvalidPayloadAttributes,
        ),
    ));
    assert!(forkchoice_applied(&result));
}

#[test]
fn unrelated_forkchoice_error_does_not_report_an_applied_forkchoice() {
    let result = Err(EngineApiError::Internal(Box::new(io::Error::other("boom"))));
    assert!(!forkchoice_applied(&result));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test -p alethia-reth-rpc --lib engine::api::tests::invalid_payload_attributes_error_reports_an_applied_forkchoice
```

Expected: compilation fails because `forkchoice_applied` and its required imports do not exist.

- [ ] **Step 3: Implement the applied-forkchoice classifier**

Import `ForkchoiceUpdateError` from `alloy_rpc_types_engine` and `BeaconForkChoiceUpdateError` from `reth_engine_primitives`. Add:

```rust
/// Return whether reth applied the requested forkchoice state.
fn forkchoice_applied(result: &Result<ForkchoiceUpdated, EngineApiError>) -> bool {
    match result {
        Ok(status) => status.payload_status.is_valid(),
        Err(EngineApiError::ForkChoiceUpdate(
            BeaconForkChoiceUpdateError::ForkchoiceUpdateError(
                ForkchoiceUpdateError::UpdatedInvalidPayloadAttributes,
            ),
        )) => true,
        _ => false,
    }
}
```

- [ ] **Step 4: Centralize pending-origin persistence**

Add the following documented method next to `persist_l1_origin`:

```rust
/// Persist and remove the pending origin for `head_block_hash`, re-buffering it on failure.
fn persist_pending_l1_origin(&self, head_block_hash: B256) -> Result<(), EngineApiError> {
    let Some(pending) = self.lock_pending_l1_origins().take(head_block_hash) else {
        return Ok(())
    };
    if let Err(err) = self.persist_l1_origin(
        pending.stored_l1_origin.clone(),
        pending.is_preconf_block,
        pending.batch_id,
    ) {
        self.lock_pending_l1_origins().stash(head_block_hash, pending);
        return Err(err)
    }
    Ok(())
}
```

Store the inner FCU result instead of immediately applying `?`. Stash newly built payload data only for `Ok` responses, call `persist_pending_l1_origin` whenever `forkchoice_applied(&result)` is true, then return the original result. This ensures the applied-invalid-attributes error still performs promotion bookkeeping.

- [ ] **Step 5: Run both focused tests and the engine API unit tests**

Run:

```bash
cargo test -p alethia-reth-rpc --lib engine::api::tests::invalid_payload_attributes_error_reports_an_applied_forkchoice
cargo test -p alethia-reth-rpc --lib engine::api::tests::unrelated_forkchoice_error_does_not_report_an_applied_forkchoice
cargo test -p alethia-reth-rpc --lib engine::api::tests
```

Expected: all focused and engine API tests pass.

### Task 2: Make Reconciliation Fallible and Mandatory

**Files:**
- Modify: `crates/rpc/src/engine/api.rs:92-124, 416-590, 673-710`
- Modify: `crates/rpc/src/engine/builder.rs:61-90`
- Test: `crates/rpc/src/engine/api.rs` inline test module

**Interfaces:**
- Changes: `TaikoEngineApi::new(...) -> Result<Self, EngineApiError>`.
- Changes: `resolve_reconciled_head_l1_origin(...) -> Result<HeadL1OriginReconciliation, String>` with `is_promotable_row: impl Fn(u64) -> Result<bool, String>`.

- [ ] **Step 1: Write failing resolver error-propagation tests**

Add:

```rust
#[test]
fn reconcile_propagates_promotable_row_read_errors() {
    let resolved = resolve_reconciled_head_l1_origin(Some(5), 10, 1024, |_| {
        Err("origin read failed".to_string())
    });
    assert_eq!(resolved, Err("origin read failed".to_string()));
}
```

Update existing resolver test closures to return `Ok(bool)` and unwrap successful results. The new test must initially fail to compile against the infallible resolver.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test -p alethia-reth-rpc --lib engine::api::tests::reconcile_propagates_promotable_row_read_errors
```

Expected: type mismatch because the resolver predicate currently returns `bool`.

- [ ] **Step 3: Make the resolver and database predicate fallible**

Change the resolver signature and calls:

```rust
fn resolve_reconciled_head_l1_origin(
    stored_head: Option<u64>,
    chain_head: u64,
    lookback: u64,
    is_promotable_row: impl Fn(u64) -> Result<bool, String>,
) -> Result<HeadL1OriginReconciliation, String>
```

Use `is_promotable_row(number)?` at both decision sites and wrap `Keep`, `ClampTo`, and `Clear` in `Ok`. In `try_reconcile_l1_origin_tables`, propagate both reads:

```rust
let is_promotable_row = |number: u64| -> Result<bool, String> {
    let Some(row) = tx.get::<StoredL1OriginTable>(number).map_err(|err| err.to_string())? else {
        return Ok(false)
    };
    if row.l1_block_height.is_zero() {
        return Ok(false)
    }
    let canonical = provider.block_hash(number).map_err(|err| err.to_string())?;
    Ok(canonical == Some(row.l2_block_hash))
};
```

Apply `?` to the resolver call so any failed read aborts the transaction.

- [ ] **Step 4: Fail engine construction when reconciliation fails**

Remove the logging-only reconciliation wrapper. Change `TaikoEngineApi::new` to return `Result<Self, EngineApiError>`, map the reconciliation `String` through `io::Error::other` into `EngineApiError::Internal`, and return `Ok(Self { ... })`. Update `TaikoEngineApiBuilder::build_engine_api` to propagate the constructor result with `?`.

- [ ] **Step 5: Run reconciliation tests and compile the RPC crate**

Run:

```bash
cargo test -p alethia-reth-rpc --lib engine::api::tests::reconcile_propagates_promotable_row_read_errors
cargo test -p alethia-reth-rpc --lib engine::api::tests
cargo check -p alethia-reth-rpc --all-features
```

Expected: all tests pass and the builder compiles with the fallible constructor.

### Task 3: Verify, Remove Temporary Docs, Commit, and Push

**Files:**
- Delete: `docs/superpowers/specs/2026-07-11-pr-219-consistency-fixes-design.md`
- Delete: `docs/superpowers/plans/2026-07-11-pr-219-consistency-fixes.md`
- Verify: `crates/rpc/src/engine/api.rs`, `crates/rpc/src/engine/builder.rs`

**Interfaces:**
- Produces: a fast-forward update to `origin/david/defer-l1-origin-persistence`.

- [ ] **Step 1: Run repository verification**

Run:

```bash
just fmt
just clippy
just test
git diff --check 5c22ded731ee099255dd7de5eb689913e469714a..HEAD
```

Expected: every command exits zero.

- [ ] **Step 2: Remove temporary Superpowers documentation**

Delete both files under `docs/superpowers`, remove the directories if empty, and verify:

```bash
test ! -e docs/superpowers
```

Expected: exit zero.

- [ ] **Step 3: Review and commit the exact scope**

Run `git status -sb`, `git diff --stat 5c22ded..HEAD`, and `git diff 5c22ded..HEAD`. Stage only `crates/rpc/src/engine/api.rs`, `crates/rpc/src/engine/builder.rs`, and the requested documentation deletions. Commit with:

```bash
git commit -m "fix(rpc): harden deferred L1 origin persistence"
```

- [ ] **Step 4: Re-run final verification against committed HEAD**

Run:

```bash
just clippy
just test
git status --short
```

Expected: clippy and tests exit zero; the worktree is clean.

- [ ] **Step 5: Push the existing PR branch**

Confirm the remote head is still `5c22ded731ee099255dd7de5eb689913e469714a`, then push without force:

```bash
git push origin HEAD:david/defer-l1-origin-persistence
```

Expected: a fast-forward update that refreshes PR 219.
