# Hoodi Unzen Fork Time Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Activate the embedded Hoodi Unzen hardfork at `2026-06-18 13:00:00 UTC` (`1781787600`).

**Architecture:** Hoodi hardfork scheduling is centralized in `TAIKO_HOODI_HARDFORKS`. Updating Hoodi's Unzen `ForkCondition` is enough because `extend_with_shared_hardforks` derives Cancun, Prague, and Osaka activation from the Unzen condition.

**Tech Stack:** Rust, Cargo workspace, `alloy_hardforks::ForkCondition`, `reth_ethereum_forks::ChainHardforks`.

---

## File Structure

- Modify: `crates/chainspec/src/hardfork.rs`
  - Change the Hoodi Unzen activation condition.
  - Add a focused unit test beside the existing Hoodi Shasta timestamp test.
- No new production files are needed.

### Task 1: Pin Hoodi Unzen Timestamp

**Files:**
- Modify: `crates/chainspec/src/hardfork.rs:79-87`
- Test: `crates/chainspec/src/hardfork.rs:286-308`

- [ ] **Step 1: Write the failing test**

Add this test after `test_hoodi_shasta_timestamp`:

```rust
#[test]
fn test_hoodi_unzen_timestamp() {
    let unzen = TAIKO_HOODI_HARDFORKS.fork(TaikoHardfork::Unzen);
    assert!(unzen.is_timestamp(), "unzen activation should be timestamp-based");
    assert_eq!(unzen, ForkCondition::Timestamp(1_781_787_600));
}
```

- [ ] **Step 2: Run the focused test to verify it fails**

Run:

```sh
cargo test -p alethia-reth-chainspec test_hoodi_unzen_timestamp
```

Expected: FAIL because Hoodi Unzen is still `ForkCondition::Never`.

- [ ] **Step 3: Write the minimal implementation**

Change the Hoodi hardfork table from:

```rust
(TaikoHardfork::Unzen.boxed(), ForkCondition::Never),
```

to:

```rust
(TaikoHardfork::Unzen.boxed(), ForkCondition::Timestamp(1_781_787_600)),
```

- [ ] **Step 4: Run focused chainspec tests**

Run:

```sh
cargo test -p alethia-reth-chainspec hoodi
```

Expected: PASS for both Hoodi timestamp tests.

- [ ] **Step 5: Check patch hygiene**

Run:

```sh
git diff --check
```

Expected: no whitespace errors.

- [ ] **Step 6: Commit implementation**

Stage only the plan and chainspec files:

```sh
git add docs/superpowers/plans/2026-06-11-hoodi-unzen-fork-time.md crates/chainspec/src/hardfork.rs
git commit -m "feat(chainspec): set Hoodi Unzen fork time"
```
