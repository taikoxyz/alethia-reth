# Uzen ZK Gas Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Uzen zk gas metering and consensus validation so Uzen blocks truncate at the zk gas limit and store finalized block zk gas in `header.difficulty`.

**Architecture:** Add a fork-scoped zk gas module in the EVM crate, wire it into Uzen execution with an inspector-style halt path inspired by `origin/opc-limiter`, thread finalized zk gas through block execution, and validate imported Uzen blocks by recomputing zk gas and comparing it against `header.difficulty`. Keep Uzen tables fixed, but introduce a schedule selector so the next post-Uzen fork can swap tables without rewriting the meter.

**Tech Stack:** Rust, `reth`, `revm`, Alloy EVM/block traits, cargo unit tests, `just fmt`, `just clippy`, `cargo nextest`

---

## File Structure

### New files

- `crates/evm/src/zk_gas/mod.rs`
  Owns module exports and the public entry points for zk gas schedules, meter state, and the Uzen execution adapter.
- `crates/evm/src/zk_gas/schedule.rs`
  Defines `ZkGasSchedule`, spawn-estimate helpers, and `schedule_for(TaikoSpecId)`.
- `crates/evm/src/zk_gas/uzen.rs`
  Contains the fixed Uzen `BLOCK_ZK_GAS_LIMIT`, opcode multipliers, precompile multipliers, and spawn estimates copied from the approved spec.
- `crates/evm/src/zk_gas/meter.rs`
  Implements checked `u64` accounting for per-tx/per-block zk gas and the limit/overflow outcome helpers.
- `crates/evm/src/zk_gas/adapter.rs`
  Implements the inspector-side opcode/spawn charging logic and the dedicated Uzen halt/error path.
- `crates/evm/src/zk_gas/precompiles.rs`
  Wraps the precompile provider so precompile gas usage can be charged with the active schedule.
- `crates/evm/src/zk_gas/tests.rs`
  Centralizes zk gas unit tests that do not belong naturally in one implementation file.

### Existing files to modify

- `crates/evm/src/lib.rs`
  Exports the new `zk_gas` module.
- `crates/evm/src/factory.rs`
  Builds Uzen-aware EVM instances with the zk gas adapter and wrapped precompiles.
- `crates/block/src/config.rs`
  Keeps Uzen env selection centralized and stops hardcoding imported Uzen `difficulty` to zero.
- `crates/block/src/factory.rs`
  Extends execution context with Uzen-specific header expectations and finalized zk gas plumbing.
- `crates/block/src/executor.rs`
  Turns the dedicated Uzen zk gas halt into “discard current tx, stop block, keep earlier txs”.
- `crates/block/src/assembler.rs`
  Writes finalized block zk gas directly into `header.difficulty` for Uzen blocks.
- `crates/block/src/tx_selection/mod.rs`
  Stops local tx selection immediately on the dedicated Uzen zk gas halt instead of treating it like a skippable tx error.
- `crates/payload/src/builder/execution.rs`
  Makes both legacy-mode and new-mode payload building stop cleanly on Uzen zk gas exhaustion.
- `crates/consensus/src/validation/mod.rs`
  Relaxes the standalone `difficulty == 0` rule only for Uzen timestamps and adds post-execution Uzen difficulty/truncation checks.
- `crates/consensus/src/validation/tests.rs`
  Covers the new Uzen header/post-execution validation rules.

## Task 1: Scaffold Fork-Scoped zk Gas Schedules

**Files:**
- Create: `crates/evm/src/zk_gas/mod.rs`
- Create: `crates/evm/src/zk_gas/schedule.rs`
- Create: `crates/evm/src/zk_gas/uzen.rs`
- Create: `crates/evm/src/zk_gas/tests.rs`
- Modify: `crates/evm/src/lib.rs`
- Test: `crates/evm/src/zk_gas/tests.rs`

- [ ] **Step 1: Write the failing schedule-selection tests**

```rust
#[test]
fn uzen_schedule_is_selected_only_for_uzen() {
    assert!(schedule_for(TaikoSpecId::UZEN).is_some());
    assert!(schedule_for(TaikoSpecId::SHASTA).is_none());
}

#[test]
fn uzen_schedule_uses_the_spec_block_limit() {
    let schedule = schedule_for(TaikoSpecId::UZEN).expect("Uzen schedule");
    assert_eq!(schedule.block_limit, 100_000_000);
}
```

- [ ] **Step 2: Run the targeted test to verify it fails**

Run: `cargo test -p alethia-reth-evm uzen_schedule_is_selected_only_for_uzen --lib`

Expected: FAIL with missing `zk_gas` module or unresolved schedule symbols.

- [ ] **Step 3: Add the documented schedule skeleton**

```rust
/// Consensus-owned zk gas schedule for a Taiko fork.
pub struct ZkGasSchedule {
    pub block_limit: u64,
    pub opcode_multipliers: [u16; 256],
    pub precompile_multipliers: [u16; 256],
    pub spawn_estimates: SpawnEstimates,
}

pub fn schedule_for(spec: TaikoSpecId) -> Option<&'static ZkGasSchedule> {
    match spec {
        TaikoSpecId::UZEN => Some(&UZEN_ZK_GAS_SCHEDULE),
        _ => None,
    }
}
```

- [ ] **Step 4: Fill the fixed Uzen tables from the approved spec**

Copy the exact Uzen constants from [2026-03-19-uzen-zk-gas-design.md](/Users/davidcai/taiko/alethia-reth/docs/superpowers/specs/2026-03-19-uzen-zk-gas-design.md). Keep them compile-time and documented. Do not add runtime config hooks.

- [ ] **Step 5: Run the schedule tests until they pass**

Run: `cargo test -p alethia-reth-evm uzen_schedule_ --lib`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/evm/src/lib.rs crates/evm/src/zk_gas/mod.rs crates/evm/src/zk_gas/schedule.rs crates/evm/src/zk_gas/uzen.rs crates/evm/src/zk_gas/tests.rs
git commit -m "feat(evm): add Uzen zk gas schedules"
```

## Task 2: Implement Checked zk Gas Meter Arithmetic

**Files:**
- Create: `crates/evm/src/zk_gas/meter.rs`
- Modify: `crates/evm/src/zk_gas/mod.rs`
- Modify: `crates/evm/src/zk_gas/tests.rs`
- Test: `crates/evm/src/zk_gas/tests.rs`

- [ ] **Step 1: Write the failing meter tests**

```rust
#[test]
fn meter_promotes_committed_tx_usage_into_block_usage() {
    let schedule = schedule_for(TaikoSpecId::UZEN).unwrap();
    let mut meter = UzenZkGasMeter::new(schedule);
    meter.charge_opcode(0x01, 3).expect("charge");
    meter.commit_transaction();
    assert_eq!(meter.block_zk_gas_used(), 3 * u64::from(schedule.opcode_multipliers[0x01]));
}

#[test]
fn meter_treats_checked_overflow_as_limit_exceeded() {
    let mut meter = UzenZkGasMeter::new(schedule_for(TaikoSpecId::UZEN).unwrap());
    assert!(matches!(meter.try_add(u64::MAX, 2), Err(ZkGasOutcome::LimitExceeded)));
}
```

- [ ] **Step 2: Run the targeted meter test to verify it fails**

Run: `cargo test -p alethia-reth-evm meter_promotes_committed_tx_usage_into_block_usage --lib`

Expected: FAIL with missing `UzenZkGasMeter` or missing methods.

- [ ] **Step 3: Implement the documented meter**

```rust
/// Checked zk gas accounting for a single Uzen block execution.
pub struct UzenZkGasMeter<'a> {
    schedule: &'a ZkGasSchedule,
    block_zk_gas_used: u64,
    tx_zk_gas_used: u64,
}

impl<'a> UzenZkGasMeter<'a> {
    pub fn reset_transaction(&mut self) { /* ... */ }
    pub fn commit_transaction(&mut self) { /* ... */ }
    pub fn charge_opcode(&mut self, opcode: u8, raw_gas: u64) -> Result<(), ZkGasOutcome> { /* ... */ }
    pub fn charge_precompile(&mut self, address_low_byte: u8, gas_used: u64) -> Result<(), ZkGasOutcome> { /* ... */ }
}
```

- [ ] **Step 4: Add tests for reset, commit, limit-exceeded, and overflow paths**

Run: `cargo test -p alethia-reth-evm meter_ --lib`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/evm/src/zk_gas/mod.rs crates/evm/src/zk_gas/meter.rs crates/evm/src/zk_gas/tests.rs
git commit -m "feat(evm): add Uzen zk gas meter"
```

## Task 3: Wire Uzen Opcode And Precompile Metering Into EVM Execution

**Files:**
- Create: `crates/evm/src/zk_gas/adapter.rs`
- Create: `crates/evm/src/zk_gas/precompiles.rs`
- Modify: `crates/evm/src/factory.rs`
- Modify: `crates/evm/src/zk_gas/mod.rs`
- Modify: `crates/evm/src/zk_gas/tests.rs`
- Test: `crates/evm/src/zk_gas/tests.rs`

- [ ] **Step 1: Write failing execution-hook tests**

```rust
#[test]
fn uzen_adapter_uses_spawn_estimate_for_precompile_dispatch() {
    // Build a Uzen EVM that calls a precompile and assert the opcode charge
    // uses the schedule spawn estimate instead of step_gas.
}

#[test]
fn uzen_adapter_raises_dedicated_error_when_limit_is_exceeded() {
    // Execute bytecode that forces the limit path and assert the returned error
    // contains the dedicated Uzen zk gas marker.
}
```

- [ ] **Step 2: Run the targeted EVM test to verify it fails**

Run: `cargo test -p alethia-reth-evm uzen_adapter_raises_dedicated_error_when_limit_is_exceeded --lib`

Expected: FAIL because no adapter or wrapped precompile provider exists.

- [ ] **Step 3: Implement the inspector-style halt path inspired by `origin/opc-limiter`**

```rust
if let Err(ZkGasOutcome::LimitExceeded) = meter.charge_opcode(opcode, raw_gas) {
    let err_slot = ctx.error();
    if err_slot.is_ok() {
        *err_slot = Err(ContextError::Custom(UZEN_ZK_GAS_LIMIT_ERR.to_string()));
    }
    interp.halt_fatal();
}
```

Also wrap the precompile provider so precompile gas gets charged after execution using the active schedule.

- [ ] **Step 4: Teach the EVM factory to enable the adapter only for `TaikoSpecId::UZEN`**

Keep non-Uzen execution on the existing path. Do not change `mix_hash`/`prevrandao` behavior.

- [ ] **Step 5: Run the EVM zk gas test suite**

Run: `cargo test -p alethia-reth-evm uzen_ --lib`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/evm/src/factory.rs crates/evm/src/zk_gas/mod.rs crates/evm/src/zk_gas/adapter.rs crates/evm/src/zk_gas/precompiles.rs crates/evm/src/zk_gas/tests.rs
git commit -m "feat(evm): meter Uzen execution for zk gas"
```

## Task 4: Thread Finalized zk Gas Through Block Execution And Header Assembly

**Files:**
- Modify: `crates/block/src/config.rs`
- Modify: `crates/block/src/factory.rs`
- Modify: `crates/block/src/executor.rs`
- Modify: `crates/block/src/assembler.rs`
- Test: `crates/block/src/executor.rs`
- Test: `crates/block/src/assembler.rs`

- [ ] **Step 1: Write the failing block-execution tests**

```rust
#[test]
fn assembled_uzen_block_uses_final_zk_gas_as_difficulty() {
    // Build a Uzen block with one successful transaction and assert
    // header.difficulty == finalized block zk gas.
}

#[test]
fn executor_discards_limit_exceeded_tx_and_stops_after_it() {
    // Build a block where tx2 exceeds the zk gas limit and tx3 would otherwise fit.
    // Assert tx1 is included, tx2 is absent, tx3 is absent.
}
```

- [ ] **Step 2: Run the targeted block test to verify it fails**

Run: `cargo test -p alethia-reth-block assembled_uzen_block_uses_final_zk_gas_as_difficulty --lib`

Expected: FAIL because block assembly still writes zero difficulty and execution does not truncate on zk gas.

- [ ] **Step 3: Extend execution context with the Uzen header expectations**

Add an optional `expected_difficulty` field for imported-block validation and the finalized zk gas plumbing needed by the executor/assembler. Keep non-Uzen callers on the old default path.

- [ ] **Step 4: Implement executor stop semantics**

Make the executor interpret the dedicated Uzen zk gas error as:

- discard current tx,
- do not append receipt,
- stop block execution,
- keep earlier committed transactions intact.

- [ ] **Step 5: Write finalized Uzen zk gas into `header.difficulty`**

Use the finalized block zk gas directly. Do not add it onto a prior `difficulty` value.

- [ ] **Step 6: Run the block crate tests**

Run: `cargo test -p alethia-reth-block uzen_ --lib`

Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add crates/block/src/config.rs crates/block/src/factory.rs crates/block/src/executor.rs crates/block/src/assembler.rs
git commit -m "feat(block): truncate Uzen blocks on zk gas exhaustion"
```

## Task 5: Enforce Uzen Consensus Validation

**Files:**
- Modify: `crates/consensus/src/validation/mod.rs`
- Modify: `crates/consensus/src/validation/tests.rs`
- Test: `crates/consensus/src/validation/tests.rs`

- [ ] **Step 1: Write the failing consensus tests**

```rust
#[test]
fn pre_uzen_header_still_rejects_nonzero_difficulty() { /* ... */ }

#[test]
fn uzen_post_execution_rejects_difficulty_mismatch() { /* ... */ }

#[test]
fn uzen_post_execution_rejects_body_past_truncation_point() { /* ... */ }
```

- [ ] **Step 2: Run the targeted consensus test to verify it fails**

Run: `cargo test -p alethia-reth-consensus uzen_post_execution_rejects_difficulty_mismatch --lib`

Expected: FAIL because validation still assumes `difficulty == 0` semantics or lacks Uzen post-execution checks.

- [ ] **Step 3: Update standalone header validation**

Keep `difficulty == 0` enforced before Uzen timestamps and relax only that rule for Uzen headers.

- [ ] **Step 4: Add Uzen post-execution validation**

Check that:

- recomputed Uzen block zk gas equals `header.difficulty`,
- imported bodies do not extend past the truncation point,
- offending transactions are not present.

- [ ] **Step 5: Run the consensus test suite**

Run: `cargo test -p alethia-reth-consensus validation --lib`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/consensus/src/validation/mod.rs crates/consensus/src/validation/tests.rs
git commit -m "feat(consensus): validate Uzen zk gas difficulty"
```

## Task 6: Stop Payload Building And Tx Selection On Uzen zk Gas Exhaustion

**Files:**
- Modify: `crates/payload/src/builder/execution.rs`
- Modify: `crates/block/src/tx_selection/mod.rs`
- Test: `crates/payload/src/builder/execution.rs`
- Test: `crates/block/src/tx_selection/mod.rs`

- [ ] **Step 1: Write the failing payload/selection tests**

```rust
#[test]
fn execute_provided_transactions_stops_on_uzen_zk_gas_error() { /* ... */ }

#[test]
fn tx_selection_breaks_on_uzen_zk_gas_error_but_keeps_skipping_invalid_txs() { /* ... */ }
```

- [ ] **Step 2: Run the targeted test to verify it fails**

Run: `cargo test -p alethia-reth-payload execute_provided_transactions_stops_on_uzen_zk_gas_error --lib`

Expected: FAIL because payload building still treats only validation errors specially.

- [ ] **Step 3: Teach payload building to stop on the dedicated Uzen zk gas error**

Handle the new error separately from:

- invalid tx skip paths,
- gas-limit-over-block validation,
- unexpected fatal execution failures.

- [ ] **Step 4: Teach tx selection to stop on the same dedicated error**

Do not mark later transactions invalid and continue. Break the list/block immediately.

- [ ] **Step 5: Run payload and tx-selection tests**

Run: `cargo test -p alethia-reth-payload --lib`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/payload/src/builder/execution.rs crates/block/src/tx_selection/mod.rs
git commit -m "feat(payload): stop selection on Uzen zk gas exhaustion"
```

## Task 7: Final Verification And Cleanup

**Files:**
- Modify as needed based on test/clippy fallout
- Verify touched files all have required Rust doc comments

- [ ] **Step 1: Run focused crate tests**

Run:

```bash
cargo test -p alethia-reth-evm --lib
cargo test -p alethia-reth-block --lib
cargo test -p alethia-reth-consensus --lib
cargo test -p alethia-reth-payload --lib
```

Expected: PASS

- [ ] **Step 2: Run formatting**

Run: `just fmt`

Expected: PASS with no remaining formatting diff.

- [ ] **Step 3: Run clippy docs gate**

Run: `just clippy`

Expected: PASS

- [ ] **Step 4: Run the workspace test suite**

Run: `just test`

Expected: PASS

- [ ] **Step 5: Commit any fallout fixes**

```bash
git add crates/evm/src crates/block/src crates/consensus/src crates/payload/src
git commit -m "test: verify Uzen zk gas integration"
```

Only make this commit if verification required code changes. If verification is clean, skip the commit and note that in the execution handoff.
