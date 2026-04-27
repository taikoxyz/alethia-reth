# Network-Aware zk Gas Block Limit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Differentiate the Unzen zk gas block limit by network — Masaya runs at 1B zk gas per block, Devnet/Hoodi/Mainnet stay at 100M — by threading `chain_id` into the zk gas schedule selection.

**Architecture:** Keep one set of opcode/precompile/spawn tables shared across both schedules. Build two `ZkGasSchedule` const values from the same helper (`UNZEN_ZK_GAS_SCHEDULE` for the 100M default, `MASAYA_UNZEN_ZK_GAS_SCHEDULE` for the 1B variant). Add a `chain_id` parameter to `schedule_for` and `shared_meter_for_spec`; the EVM factory pulls `chain_id` from `cfg_env.chain_id` already in scope. Replace the existing `meter_schedule` lookup helper with a `ZkGasMeter::schedule()` accessor so deferred-step charging does not need to re-resolve the schedule from `(spec, chain_id)`.

**Tech Stack:** Rust, Reth/Revm v2.0.0, Taiko zk gas crate (`crates/evm/src/zk_gas`), `cargo nextest`.

**Spec:** [`docs/superpowers/specs/2026-04-27-network-aware-zk-gas-block-limit-design.md`](../specs/2026-04-27-network-aware-zk-gas-block-limit-design.md).

---

### Task 1: Expose the schedule from `ZkGasMeter`

The current `meter_schedule()` helper in `adapter.rs` re-runs `schedule_for(TaikoSpecId::UNZEN)` to recover the static schedule reference. Once `schedule_for` requires a `chain_id` argument, that helper has nowhere to source one. Adding a `schedule()` accessor on `ZkGasMeter` lets the adapter recover the schedule directly from the meter, with no second lookup.

**Files:**
- Modify: `crates/evm/src/zk_gas/meter.rs`
- Test: `crates/evm/src/zk_gas/tests.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/evm/src/zk_gas/tests.rs` after the existing meter tests (right before `#[derive(Default, Debug)] struct StepGasProbeInspector`):

```rust
#[test]
fn meter_exposes_its_schedule() {
    let schedule = schedule_for(TaikoSpecId::UNZEN).expect("Unzen schedule");
    let meter = ZkGasMeter::new(schedule);

    assert!(std::ptr::eq(meter.schedule(), schedule));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p alethia-reth-evm zk_gas::tests::meter_exposes_its_schedule`

Expected: FAIL — compile error `no method named 'schedule' found for struct 'ZkGasMeter'`.

- [ ] **Step 3: Add the accessor**

In `crates/evm/src/zk_gas/meter.rs`, add this method inside `impl<'a> ZkGasMeter<'a>` next to `block_zk_gas_used`:

```rust
    /// Returns the consensus schedule that backs this meter.
    pub const fn schedule(&self) -> &'a ZkGasSchedule {
        self.schedule
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo nextest run -p alethia-reth-evm zk_gas::tests::meter_exposes_its_schedule`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/evm/src/zk_gas/meter.rs crates/evm/src/zk_gas/tests.rs
git commit -m "feat(zk-gas): expose schedule accessor on ZkGasMeter"
```

---

### Task 2: Add `MASAYA_UNZEN_ZK_GAS_SCHEDULE` with shared multiplier tables

Extract the `ZkGasSchedule` body in `unzen.rs` into a private helper that takes a `block_limit` and reuses the existing opcode/precompile multiplier tables and spawn estimates. Use it to build both the existing `UNZEN_ZK_GAS_SCHEDULE` (100M) and the new `MASAYA_UNZEN_ZK_GAS_SCHEDULE` (1B). The schedule still cannot be picked yet — selection lands in Task 3.

**Files:**
- Modify: `crates/evm/src/zk_gas/unzen.rs`
- Test: `crates/evm/src/zk_gas/tests.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/evm/src/zk_gas/tests.rs`. Update the `use super::{...}` block to import the new constant:

```rust
use super::{
    adapter::ZK_GAS_LIMIT_ERR,
    meter::{ZkGasMeter, ZkGasOutcome},
    schedule::schedule_for,
    unzen::{MASAYA_BLOCK_ZK_GAS_LIMIT, MASAYA_UNZEN_ZK_GAS_SCHEDULE, UNZEN_ZK_GAS_SCHEDULE},
};
```

(Adjust to merge with whatever is currently imported — the `unzen::` line is the new bit.)

Add these tests next to the existing `unzen_schedule_*` tests:

```rust
#[test]
fn masaya_unzen_schedule_uses_one_billion_block_limit() {
    assert_eq!(MASAYA_UNZEN_ZK_GAS_SCHEDULE.block_limit, 1_000_000_000);
    assert_eq!(MASAYA_BLOCK_ZK_GAS_LIMIT, 1_000_000_000);
}

#[test]
fn masaya_and_default_unzen_schedules_share_opcode_and_precompile_tables() {
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.opcode_multipliers
    );
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.precompile_multipliers,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.precompile_multipliers
    );
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.call,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.call
    );
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.callcode,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.callcode
    );
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.delegatecall,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.delegatecall
    );
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.staticcall,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.staticcall
    );
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.create,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.create
    );
    assert_eq!(
        UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.create2,
        MASAYA_UNZEN_ZK_GAS_SCHEDULE.spawn_estimates.create2
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p alethia-reth-evm zk_gas::tests::masaya`

Expected: FAIL — compile error `unresolved imports MASAYA_BLOCK_ZK_GAS_LIMIT, MASAYA_UNZEN_ZK_GAS_SCHEDULE`.

- [ ] **Step 3: Refactor `unzen.rs` to share the table builder**

Replace the head of `crates/evm/src/zk_gas/unzen.rs` (the doc comment, constants, and `UNZEN_ZK_GAS_SCHEDULE` definition — i.e. everything before `const fn unzen_opcode_multipliers()`) with:

```rust
//! Fixed Unzen zk gas schedule constants copied from the approved protocol spec.
//!
//! Source:
//! <https://github.com/taikoxyz/taiko-mono/blob/main/packages/protocol/docs/zk_gas_spec.md>

use super::schedule::{SpawnEstimates, ZkGasSchedule};

/// Maximum zk gas permitted within a single Unzen block on Devnet, Hoodi, and Mainnet.
pub const BLOCK_ZK_GAS_LIMIT: u64 = 100_000_000;

/// Maximum zk gas permitted within a single Unzen block on the Taiko Masaya network.
pub const MASAYA_BLOCK_ZK_GAS_LIMIT: u64 = 1_000_000_000;

/// Fail-safe multiplier used for any opcode or precompile missing from the Unzen table.
const FAILSAFE_MULTIPLIER: u16 = u16::MAX;

/// Builds an Unzen-shaped zk gas schedule with the requested block limit. Opcode multipliers,
/// precompile multipliers, and spawn estimates are identical across all networks; only the block
/// budget differs.
const fn unzen_schedule_with_block_limit(block_limit: u64) -> ZkGasSchedule {
    ZkGasSchedule {
        block_limit,
        opcode_multipliers: unzen_opcode_multipliers(),
        precompile_multipliers: unzen_precompile_multipliers(),
        spawn_estimates: SpawnEstimates {
            call: 12_500,
            callcode: 12_500,
            delegatecall: 3_500,
            staticcall: 3_500,
            create: 37_000,
            create2: 44_500,
        },
    }
}

/// Default Unzen zk gas schedule used by Devnet, Hoodi, and Mainnet.
pub const UNZEN_ZK_GAS_SCHEDULE: ZkGasSchedule =
    unzen_schedule_with_block_limit(BLOCK_ZK_GAS_LIMIT);

/// Unzen zk gas schedule used by the Taiko Masaya network with a 10× higher block budget.
pub const MASAYA_UNZEN_ZK_GAS_SCHEDULE: ZkGasSchedule =
    unzen_schedule_with_block_limit(MASAYA_BLOCK_ZK_GAS_LIMIT);
```

Leave `unzen_opcode_multipliers()` and `unzen_precompile_multipliers()` exactly as they are — they stay verbatim below this header.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p alethia-reth-evm zk_gas::tests::masaya`

Expected: PASS for both new tests. The existing `unzen_schedule_uses_the_spec_block_limit`, `unzen_schedule_uses_spec_opcode_and_precompile_multipliers`, and `unzen_schedule_uses_spec_spawn_estimates` tests must also still pass — run them too:

Run: `cargo nextest run -p alethia-reth-evm zk_gas::tests::unzen_schedule`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/evm/src/zk_gas/unzen.rs crates/evm/src/zk_gas/tests.rs
git commit -m "feat(zk-gas): add MASAYA_UNZEN_ZK_GAS_SCHEDULE with shared tables"
```

---

### Task 3: Thread `chain_id` through schedule selection

Add `TAIKO_MASAYA_CHAIN_ID` and a `chain_id` argument to `schedule_for` and `shared_meter_for_spec`, branch on it for Unzen, and update every call site (the EVM factory, the inline `meter_schedule` helper in `adapter.rs`, and all existing tests).

**Files:**
- Modify: `crates/evm/src/zk_gas/schedule.rs`
- Modify: `crates/evm/src/zk_gas/adapter.rs:35-38, 384-387`
- Modify: `crates/evm/src/factory.rs:51, 72`
- Modify: `crates/evm/src/zk_gas/tests.rs`

- [ ] **Step 1: Write the failing selection tests**

Append to `crates/evm/src/zk_gas/tests.rs`:

```rust
#[test]
fn schedule_for_returns_masaya_schedule_on_masaya_chain_id() {
    let schedule =
        schedule_for(TaikoSpecId::UNZEN, TAIKO_MASAYA_CHAIN_ID).expect("Unzen schedule");
    assert_eq!(schedule.block_limit, 1_000_000_000);
    assert!(std::ptr::eq(schedule, &MASAYA_UNZEN_ZK_GAS_SCHEDULE));
}

#[test]
fn schedule_for_returns_default_schedule_on_non_masaya_chain_ids() {
    for chain_id in [1u64, 167u64, 167_010u64, 167_009u64] {
        let schedule = schedule_for(TaikoSpecId::UNZEN, chain_id).expect("Unzen schedule");
        assert_eq!(schedule.block_limit, 100_000_000);
        assert!(std::ptr::eq(schedule, &UNZEN_ZK_GAS_SCHEDULE));
    }
}

#[test]
fn schedule_for_returns_none_for_pre_unzen_regardless_of_chain_id() {
    assert!(schedule_for(TaikoSpecId::SHASTA, TAIKO_MASAYA_CHAIN_ID).is_none());
    assert!(schedule_for(TaikoSpecId::SHASTA, 167).is_none());
}
```

Update the `use super::{...}` import block to also bring in `TAIKO_MASAYA_CHAIN_ID`:

```rust
use super::{
    adapter::ZK_GAS_LIMIT_ERR,
    meter::{ZkGasMeter, ZkGasOutcome},
    schedule::{TAIKO_MASAYA_CHAIN_ID, schedule_for},
    unzen::{MASAYA_BLOCK_ZK_GAS_LIMIT, MASAYA_UNZEN_ZK_GAS_SCHEDULE, UNZEN_ZK_GAS_SCHEDULE},
};
```

- [ ] **Step 2: Run the tests to confirm they fail**

Run: `cargo nextest run -p alethia-reth-evm zk_gas::tests`

Expected: FAIL — compile errors `cannot find value TAIKO_MASAYA_CHAIN_ID`, plus `this function takes 1 argument but 2 arguments were supplied` on every new `schedule_for(..., chain_id)` call. The pre-existing tests will still compile because they pass one arg; they will be updated in Step 4.

- [ ] **Step 3: Update `schedule.rs`**

Replace the entire body of `crates/evm/src/zk_gas/schedule.rs` with:

```rust
//! Shared zk gas schedule types and fork selection helpers.

use crate::spec::TaikoSpecId;

use super::unzen::{MASAYA_UNZEN_ZK_GAS_SCHEDULE, UNZEN_ZK_GAS_SCHEDULE};

/// EVM chain id for the Taiko Masaya network. Sourced from
/// `crates/chainspec/src/genesis/masaya.json`. Tests in `crates/chainspec` already pin the
/// genesis hash for this chain id, which guards against drift.
pub const TAIKO_MASAYA_CHAIN_ID: u64 = 167_011;

/// Fixed raw-gas estimates for spawn opcodes.
#[derive(Clone, Copy)]
pub struct SpawnEstimates {
    /// Fixed raw-gas estimate for `CALL`.
    pub call: u64,
    /// Fixed raw-gas estimate for `CALLCODE`.
    pub callcode: u64,
    /// Fixed raw-gas estimate for `DELEGATECALL`.
    pub delegatecall: u64,
    /// Fixed raw-gas estimate for `STATICCALL`.
    pub staticcall: u64,
    /// Fixed raw-gas estimate for `CREATE`.
    pub create: u64,
    /// Fixed raw-gas estimate for `CREATE2`.
    pub create2: u64,
}

/// Consensus-owned zk gas schedule for a Taiko fork.
#[derive(Clone, Copy)]
pub struct ZkGasSchedule {
    /// Maximum zk gas permitted across a single block.
    pub block_limit: u64,
    /// Per-opcode proving-cost multipliers indexed by opcode byte.
    pub opcode_multipliers: [u16; 256],
    /// Per-precompile proving-cost multipliers indexed by low-byte address.
    pub precompile_multipliers: [u16; 256],
    /// Fixed raw-gas estimates for spawn opcodes.
    pub spawn_estimates: SpawnEstimates,
}

/// Returns the consensus zk gas schedule for the active Taiko fork on the given chain, when
/// defined. Masaya runs Unzen with a 10× higher block budget than Devnet/Hoodi/Mainnet; all other
/// chains share the default Unzen schedule.
pub const fn schedule_for(spec: TaikoSpecId, chain_id: u64) -> Option<&'static ZkGasSchedule> {
    match spec {
        TaikoSpecId::UNZEN => Some(if chain_id == TAIKO_MASAYA_CHAIN_ID {
            &MASAYA_UNZEN_ZK_GAS_SCHEDULE
        } else {
            &UNZEN_ZK_GAS_SCHEDULE
        }),
        _ => None,
    }
}
```

- [ ] **Step 4: Update existing test call sites in `tests.rs`**

The pre-existing tests use `schedule_for(TaikoSpecId::UNZEN)` with one argument. Rewrite each one to pass an explicit non-Masaya chain id so they keep selecting the default 100M schedule. Replace each occurrence individually with a sed-equivalent edit:

In `crates/evm/src/zk_gas/tests.rs`, replace every occurrence of:

```rust
schedule_for(TaikoSpecId::UNZEN)
```

with:

```rust
schedule_for(TaikoSpecId::UNZEN, 167)
```

And replace the single occurrence of:

```rust
assert!(schedule_for(TaikoSpecId::SHASTA).is_none());
```

with:

```rust
assert!(schedule_for(TaikoSpecId::SHASTA, 167).is_none());
```

- [ ] **Step 5: Update `shared_meter_for_spec` and `meter_schedule` in `adapter.rs`**

In `crates/evm/src/zk_gas/adapter.rs`, replace the `shared_meter_for_spec` function (the one currently at lines ~33-38) with:

```rust
/// Returns a freshly initialized shared meter handle for the requested spec and chain when
/// metering is active.
pub fn shared_meter_for_spec(spec: TaikoSpecId, chain_id: u64) -> Option<SharedZkGasMeter> {
    // Returning `None` keeps the inspector on its zero-overhead pass-through path.
    schedule_for(spec, chain_id).map(|schedule| Arc::new(Mutex::new(ZkGasMeter::new(schedule))))
}
```

Replace the `meter_schedule` helper near the bottom of the file with:

```rust
/// Returns the schedule backing a shared meter handle.
fn meter_schedule(meter: &SharedZkGasMeter) -> &'static ZkGasSchedule {
    lock_meter(meter).schedule()
}
```

This drops the previous `schedule_for(TaikoSpecId::UNZEN)` lookup entirely; the meter already owns the schedule.

- [ ] **Step 6: Update `factory.rs` to forward `chain_id`**

In `crates/evm/src/factory.rs`, update both `create_evm` and `create_evm_with_inspector` so the `shared_meter_for_spec` call passes the chain id from `cfg_env`.

Replace the line in `create_evm`:

```rust
        let meter = shared_meter_for_spec(spec_id);
```

with:

```rust
        let meter = shared_meter_for_spec(spec_id, input.cfg_env.chain_id);
```

Replace the matching line in `create_evm_with_inspector`:

```rust
        let meter = shared_meter_for_spec(spec_id);
```

with:

```rust
        let meter = shared_meter_for_spec(spec_id, input.cfg_env.chain_id);
```

- [ ] **Step 7: Build and run the full zk_gas test suite**

Run: `cargo build -p alethia-reth-evm`

Expected: clean build, no warnings or errors. If a compile error remains, it almost certainly comes from a `schedule_for(TaikoSpecId::UNZEN)` call site that still passes one argument — search for any remaining one-arg calls with `grep -rn 'schedule_for(' crates/` and update them to pass `167` (test contexts) or `input.cfg_env.chain_id` (production contexts).

Then:

Run: `cargo nextest run -p alethia-reth-evm zk_gas::tests`

Expected: every test in `zk_gas::tests` passes, including the three new Task 3 tests, the two new Task 2 tests, and the Task 1 accessor test.

- [ ] **Step 8: Commit**

```bash
git add crates/evm/src/zk_gas/schedule.rs \
        crates/evm/src/zk_gas/adapter.rs \
        crates/evm/src/zk_gas/tests.rs \
        crates/evm/src/factory.rs
git commit -m "feat(zk-gas): select zk gas schedule per network via chain id"
```

---

### Task 4: Workspace verification

Run the full workspace lint and test suites to confirm the change doesn't regress consumers outside the `evm` crate. The block, payload, consensus, and rpc crates depend on `zk_gas::adapter::ZK_GAS_LIMIT_ERR` but not on `schedule_for` or `shared_meter_for_spec`, so they should compile without modification — this task verifies that.

**Files:**
- (none)

- [ ] **Step 1: Run `just fmt`**

Run: `just fmt`

Expected: exit 0, no diff. If `cargo sort` rewrites a manifest, re-stage it.

- [ ] **Step 2: Run `just clippy`**

Run: `just clippy`

Expected: exit 0, no warnings. Treat any warning as an error and fix it before continuing.

- [ ] **Step 3: Run `just test`**

Run: `just test`

Expected: every workspace test passes, including:
- `crates/evm/src/zk_gas/tests.rs` (the six new tests plus the existing suite)
- the existing block/payload/consensus/rpc tests, untouched by this change

- [ ] **Step 4: Final commit (only if `just fmt` rewrote anything)**

If `just fmt` produced a diff, commit it:

```bash
git add -u
git commit -m "chore(zk-gas): apply formatter after network-aware schedule change"
```

Otherwise skip this step — there is nothing to commit.
