# Anchor Zk Gas Payload Error Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make new-mode anchor zk gas exhaustion return a dedicated payload-builder error while preserving every other payload zk gas path.

**Architecture:** Keep the change local to `crates/payload/src/builder/execution.rs`. Introduce a dedicated typed error for the new-mode anchor zk gas case, map only that branch to `PayloadBuilderError::other(...)`, and add focused tests proving both the changed path and the unchanged non-zk-gas/legacy-mode behavior.

**Tech Stack:** Rust, reth payload builder APIs, Cargo tests, `just clippy`

---

## File Map

- Modify: `crates/payload/src/builder/execution.rs`
  Purpose: define the dedicated payload-layer error, change the new-mode anchor zk gas branch, and add focused tests beside the helper logic.
- Verify: `cargo test -q -p alethia-reth-payload --lib`
  Purpose: run the payload crate tests after the focused cases pass.
- Verify: `just clippy`
  Purpose: satisfy the repo docs and lint gate.

### Task 1: Add the dedicated anchor zk gas payload error

**Files:**
- Modify: `crates/payload/src/builder/execution.rs`

- [ ] **Step 1: Write the failing focused test for the changed path**

Add a new test in `crates/payload/src/builder/execution.rs` for the new-mode anchor path. It should:

- build a `PoolExecutionContext` whose `anchor_tx` hits the zk gas limit,
- instantiate the real helper inputs required by
  `execute_anchor_and_pool_transactions(...)`, specifically:
  - a `MockEthProvider` seeded with the Taiko chain spec and parent header data
    needed by anchor validation,
  - a `NoopTransactionPool`,
  - a local test helper that constructs a genuinely validation-passing anchor
    transaction instead of a generic `recovered_tx(...)` fixture,
- call `execute_anchor_and_pool_transactions(...)`,
- assert that the function returns `Err(...)` instead of `Ok(ExecutionOutcome::Completed(U256::ZERO))`,
- assert that the returned `PayloadBuilderError` uses the dedicated `Other(...)` path,
- assert that the boxed source downcasts to the new `AnchorZkGasLimitExceeded` type.

Use the existing test fixtures already imported in the file where possible, and add only the minimal extra scaffolding needed for a pool value if the helper requires one.

- [ ] **Step 2: Run the focused test to verify it fails**

Run:

```bash
cargo test -q -p alethia-reth-payload anchor_zk_gas -- --nocapture
```

Expected: FAIL because the code still returns `Completed(U256::ZERO)` and the dedicated error type does not exist yet.

- [ ] **Step 3: Implement the dedicated error type**

In `crates/payload/src/builder/execution.rs`, add a documented error type similar in style to other small local typed errors:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnchorZkGasLimitExceeded;

impl std::fmt::Display for AnchorZkGasLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("new-mode anchor transaction exceeded zk gas limit")
    }
}

impl std::error::Error for AnchorZkGasLimitExceeded {}
```

Keep it local to this file unless compilation forces broader visibility for the tests.

- [ ] **Step 4: Change only the new-mode anchor zk gas branch**

In `execute_anchor_and_pool_transactions(...)`, replace:

```rust
return Ok(ExecutionOutcome::Completed(U256::ZERO));
```

with:

```rust
return Err(PayloadBuilderError::other(AnchorZkGasLimitExceeded));
```

Do not change:

- the legacy-mode zk gas clean-stop logic,
- the `PayloadBuilderError::evm(err)` path for ordinary anchor failures,
- pool transaction selection behavior after successful anchor execution.

- [ ] **Step 5: Run the focused changed-path test to verify it passes**

Run the targeted test command from Step 2 again.

Expected: PASS, with the returned error downcasting to `AnchorZkGasLimitExceeded`.

- [ ] **Step 6: Commit Task 1**

Run:

```bash
git add crates/payload/src/builder/execution.rs
git commit -m "fix(payload): error on anchor zk gas exhaustion"
```

### Task 2: Add scope-preservation regressions and verify the crate

**Files:**
- Modify: `crates/payload/src/builder/execution.rs`

- [ ] **Step 1: Add the regression for ordinary anchor execution failures**

Add a second focused test in `crates/payload/src/builder/execution.rs` that forces a non-zk-gas anchor execution failure in the same new-mode helper and asserts:

- the failure occurs after anchor validation has already succeeded,
- the setup uses the same `MockEthProvider`, `NoopTransactionPool`, and
  validation-passing anchor helper as Task 1,
- the failure mode is prescribed, not ad hoc: configure the builder block gas
  limit below the anchor transaction gas limit so execution fails with
  `TransactionGasLimitMoreThanAvailableBlockGas`,
- the result still maps to `PayloadBuilderError::evm(err)`,
- the error is not the new `AnchorZkGasLimitExceeded` type.

Do not use an anchor validation failure for this test, because validation errors
already map through `PayloadBuilderError::other(...)` and would not exercise the
preserved execution-error branch.

- [ ] **Step 2: Re-run the existing legacy-mode zk gas regression**

Run:

```bash
cargo test -q -p alethia-reth-payload execute_provided_transactions_stops_on_zk_gas_error -- --exact
```

Expected: PASS, confirming legacy-mode behavior stays unchanged.

- [ ] **Step 3: Run the ordinary-anchor-failure regression**

Run a targeted command for the new test added in Step 1.

Expected: PASS, confirming non-zk-gas anchor failures still use `PayloadBuilderError::evm(err)`.

- [ ] **Step 4: Run the payload crate tests**

Run:

```bash
cargo test -q -p alethia-reth-payload --lib
```

Expected: PASS.

- [ ] **Step 5: Run the lint gate**

Run:

```bash
just clippy
```

Expected: PASS with no docs or lint regressions.

- [ ] **Step 6: Commit Task 2**

Run:

```bash
git add crates/payload/src/builder/execution.rs
git commit -m "test(payload): cover anchor zk gas error paths"
```

## Notes For Execution

- Keep the implementation local to `crates/payload/src/builder/execution.rs`.
- Do not broaden the change into legacy-mode zk gas handling.
- Do not remap ordinary anchor execution failures into the new dedicated error.
- Prefer typed assertions over message-only assertions for the new error.
- Follow the repository documentation policy for any new production symbol.

## Manual Review

Because this session is not automatically dispatching plan-review agents unless needed, do a local review before execution handoff:

- Re-read the anchor zk gas branch to confirm it is the only behavior-changing branch.
- Re-read the tests to confirm one covers the changed path and one covers preserved behavior.
- Confirm the plan still maps directly back to `docs/superpowers/specs/2026-03-20-anchor-zk-gas-payload-error-design.md`.
