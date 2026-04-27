# Network-Aware zk Gas Block Limit

## Summary

Differentiate the zk gas block limit by network so that Masaya runs with a 1
billion zk gas budget while Devnet, Hoodi, and Mainnet keep the existing
100 million zk gas budget. Opcode and precompile multiplier tables and spawn
estimates remain identical across networks; only the block limit differs.

## Motivation

The Unzen zk gas schedule currently bakes a single `BLOCK_ZK_GAS_LIMIT = 100_000_000`
constant into `UNZEN_ZK_GAS_SCHEDULE`. We want a 10× higher zk gas budget on the
Masaya network without changing the per-opcode pricing model that the rest of
the protocol relies on.

## Scope

In scope:

- Adding a Masaya-specific Unzen schedule with a 1B block limit.
- Threading `chain_id` into the zk gas schedule lookup so the EVM factory can
  pick the right schedule per network.
- Tests covering the per-network block limit and the shared multiplier tables.

Out of scope:

- Changes to opcode multiplier tables, precompile multiplier tables, or spawn
  estimates.
- Adding zk gas schedules for non-Unzen forks.
- Any chainspec, payload, block, consensus, or RPC crate changes — the chain
  id already flows into `cfg_env` and reaches the factory unchanged.

## Design

### Shared schedule data

`crates/evm/src/zk_gas/unzen.rs` keeps a single source of truth for the opcode
and precompile multiplier tables and the spawn estimates. The current
`unzen_opcode_multipliers()` and `unzen_precompile_multipliers()` `const fn`s
are reused by both schedules. A private helper builds a full `ZkGasSchedule`
given a block limit:

```rust
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

pub const BLOCK_ZK_GAS_LIMIT: u64 = 100_000_000;
pub const MASAYA_BLOCK_ZK_GAS_LIMIT: u64 = 1_000_000_000;

pub const UNZEN_ZK_GAS_SCHEDULE: ZkGasSchedule =
    unzen_schedule_with_block_limit(BLOCK_ZK_GAS_LIMIT);
pub const MASAYA_UNZEN_ZK_GAS_SCHEDULE: ZkGasSchedule =
    unzen_schedule_with_block_limit(MASAYA_BLOCK_ZK_GAS_LIMIT);
```

### Schedule selection

`crates/evm/src/zk_gas/schedule.rs` gains a chain id parameter. The Masaya
chain id (`167011`, sourced from `crates/chainspec/src/genesis/masaya.json`) is
declared as a `pub const` next to the selection function so it lives with the
selection logic and can be referenced by tests.

```rust
pub const TAIKO_MASAYA_CHAIN_ID: u64 = 167_011;

pub const fn schedule_for(
    spec: TaikoSpecId,
    chain_id: u64,
) -> Option<&'static ZkGasSchedule> {
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

### Adapter and factory plumbing

`shared_meter_for_spec` takes the chain id and forwards it to `schedule_for`:

```rust
pub fn shared_meter_for_spec(
    spec: TaikoSpecId,
    chain_id: u64,
) -> Option<SharedZkGasMeter> {
    schedule_for(spec, chain_id)
        .map(|schedule| Arc::new(Mutex::new(ZkGasMeter::new(schedule))))
}
```

`crates/evm/src/factory.rs` reads `input.cfg_env.chain_id` and passes it into
`shared_meter_for_spec` from both `create_evm` and `create_evm_with_inspector`.
No other call sites construct a meter via this helper.

### Tests

In `crates/evm/src/zk_gas/tests.rs`:

- Introduce a small `TEST_DEFAULT_CHAIN_ID` constant (any non-Masaya chain id)
  used by existing tests that don't care about network differentiation.
- Thread the chain id into every existing `schedule_for(...)` call so tests
  compile against the new signature.
- Add new tests:
  - `masaya_unzen_schedule_has_one_billion_block_limit` — asserts
    `schedule_for(UNZEN, TAIKO_MASAYA_CHAIN_ID).block_limit == 1_000_000_000`.
  - `non_masaya_unzen_schedule_keeps_hundred_million_block_limit` — asserts
    `schedule_for(UNZEN, TEST_DEFAULT_CHAIN_ID).block_limit == 100_000_000`.
  - `masaya_and_default_schedules_share_opcode_and_precompile_tables` — asserts
    the two schedules have identical opcode/precompile multiplier arrays and
    spawn estimates so the tables can't drift if someone edits only one.
  - `schedule_for_returns_none_for_pre_unzen_regardless_of_chain_id` — asserts
    `schedule_for(SHASTA, _)` and any other pre-Unzen spec returns `None` for
    both Masaya and non-Masaya chain ids.

The inline `schedule_for(TaikoSpecId::UNZEN)` call inside `adapter.rs` (used by
the adapter's own unit tests) is updated to pass `TEST_DEFAULT_CHAIN_ID`.

## File-by-file changes

- `crates/evm/src/zk_gas/unzen.rs` — extract shared table builders into a
  helper, declare both `UNZEN_ZK_GAS_SCHEDULE` and
  `MASAYA_UNZEN_ZK_GAS_SCHEDULE`, expose `MASAYA_BLOCK_ZK_GAS_LIMIT`.
- `crates/evm/src/zk_gas/schedule.rs` — add `TAIKO_MASAYA_CHAIN_ID`, change
  `schedule_for` to take `chain_id`, branch on it for Unzen.
- `crates/evm/src/zk_gas/adapter.rs` — `shared_meter_for_spec(spec, chain_id)`;
  update inline test helper to pass a chain id.
- `crates/evm/src/zk_gas/tests.rs` — thread chain id through existing tests,
  add the four new tests.
- `crates/evm/src/factory.rs` — pass `input.cfg_env.chain_id` to
  `shared_meter_for_spec` from both `create_evm` and
  `create_evm_with_inspector`.

No changes are required in `crates/chainspec`, `crates/payload`,
`crates/block`, `crates/consensus`, or `crates/rpc`.

## Risks and mitigations

- **Wrong chain id constant.** The Masaya chain id is `167011` per
  `crates/chainspec/src/genesis/masaya.json`. The test
  `masaya_unzen_schedule_has_one_billion_block_limit` will fail loudly if the
  selection logic, the constant, or the genesis JSON drift apart.
- **Silent table drift between the two schedules.** Both schedules are built
  from the same `unzen_schedule_with_block_limit` helper, and the
  `masaya_and_default_schedules_share_opcode_and_precompile_tables` test pins
  this invariant.
- **Pre-Unzen behavior change.** Selection still returns `None` for any spec
  other than Unzen, regardless of chain id, so the meter remains disabled on
  pre-Unzen forks. Covered by
  `schedule_for_returns_none_for_pre_unzen_regardless_of_chain_id`.

## Verification

- `just fmt`
- `just clippy`
- `just test`
