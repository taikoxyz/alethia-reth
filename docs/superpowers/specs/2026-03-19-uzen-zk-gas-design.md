# Uzen ZK Gas Design

## Status

Approved design draft for implementation planning.

## Summary

Implement zk gas as a Uzen consensus rule in `alethia-reth`.

For Uzen blocks:

- `header.difficulty` is repurposed to store the finalized block zk gas value.
- `mix_hash` / `prevrandao` semantics remain unchanged.
- zk gas is recomputed from block execution using the protocol multiplier tables and spawn estimates from the zk gas spec.
- If execution exceeds the zk gas limit mid-transaction, that transaction is discarded and all later transactions are skipped.
- Imported blocks are rejected unless their body ends at the last fully included transaction and `header.difficulty` matches the recomputed block zk gas.

The Uzen multiplier tables are fixed. The implementation should still introduce a schedule abstraction so the next post-Uzen fork can replace the tables without rewriting the meter.

## Goals

- Enforce the zk gas rules from `taiko-mono/packages/protocol/docs/zk_gas_spec.md` for Uzen blocks.
- Make `header.difficulty` the canonical block zk gas value for Uzen.
- Keep `mix_hash` / `prevrandao` behavior unchanged.
- Share the same metering and truncation logic between local block building and imported block validation.
- Keep the multiplier-table mechanism extensible for the next fork after Uzen.

## Non-Goals

- Changing pre-Uzen consensus behavior.
- Making zk gas schedules runtime-configurable.
- Moving multiplier tables into chain-spec JSON.
- Changing Uzen tables after activation.
- Redefining `mix_hash` semantics.

## Confirmed Constraints

- Uzen activation remains timestamp-gated through the existing `TaikoHardfork::Uzen` path.
- `header.difficulty` becomes consensus data for Uzen and must match the locally recomputed block zk gas.
- When zk gas is exceeded mid-transaction, the offending transaction is not part of the block.
- Any transactions after the offending transaction are also not part of the block.
- `header.difficulty` must equal the zk gas from fully included transactions only.
- Uzen uses a fixed table set. Future forks may replace the tables, but Uzen itself will not.

## Current Codebase Touchpoints

- `crates/block/src/config.rs`
  Builds `BlockEnv` and selects `TaikoSpecId`.
- `crates/block/src/assembler.rs`
  Writes `block_env.difficulty()` into the final header.
- `crates/block/src/executor.rs`
  Owns transaction execution and block-level commit flow.
- `crates/block/src/factory.rs`
  Threads execution context into the executor.
- `crates/evm/src/factory.rs`
  Creates the Taiko EVM and inspector wiring.
- `crates/evm/src/execution.rs`
  Routes transaction execution through `TaikoEvmHandler`.
- `crates/consensus/src/validation/mod.rs`
  Performs header and block consensus validation.
- `crates/payload/src/builder/mod.rs`
  Builds local payloads and currently continues through some recoverable transaction failures.

## Reused Idea From `origin/opc-limiter`

Do not reuse the old `JUMPDEST` policy or limits.

Do reuse the pattern where execution logic:

- keeps a shared block-level counter in execution context,
- raises a custom execution error through the EVM context,
- halts immediately so `revm` discards the current transaction journal,
- lets outer block-building logic distinguish that fatal consensus stop from ordinary invalid transactions.

This pattern is the right base for Uzen zk gas truncation.

## Proposed Architecture

### 1. Fork-Scoped zk gas schedules

Add a new `zk_gas` area under `crates/evm/src/` with:

- `ZkGasSchedule`
- `UzenZkGasSchedule`
- `schedule_for(spec: TaikoSpecId) -> Option<&'static ZkGasSchedule>`

`ZkGasSchedule` contains:

- `block_limit: u64`
- `opcode_multipliers: [u16; 256]`
- `precompile_multipliers: [u16; 256]`
- `spawn_estimates` keyed by spawn opcode

For now:

- `schedule_for(TaikoSpecId::UZEN)` returns the Uzen schedule.
- earlier forks return `None`.

This keeps Uzen fixed while making the next fork a table swap instead of a meter rewrite.

### 2. Dedicated Uzen zk gas meter

Add a `UzenZkGasMeter` that tracks:

- `block_zk_gas_used: u64`
- `tx_zk_gas_used: u64`
- active schedule

It exposes methods to:

- reset per-transaction state,
- charge opcode zk gas,
- charge precompile zk gas,
- promote committed tx zk gas into block zk gas,
- read finalized block zk gas.

All arithmetic uses checked `u64`.

Any multiplication or addition overflow is treated as limit exceeded.

### 3. Execution adapter

Add a Uzen-only execution adapter, inspector-based or equivalent, that translates `revm` execution events into meter charges:

- per-opcode `step` / `step_end` accounting,
- child-frame / precompile dispatch detection for spawn opcodes,
- post-precompile gas charging,
- immediate halt with custom context error on limit exceed.

This adapter should only activate when the current spec is `TaikoSpecId::UZEN`.

### 4. Block executor integration

Extend `TaikoBlockExecutionCtx` so the executor and assembler can access shared zk gas state for the block being executed.

The executor remains the single place that decides whether a transaction:

- commits,
- is skipped as invalid in existing non-consensus cases,
- or aborts block construction due to zk gas exhaustion.

### 5. Header assembly

For Uzen blocks, write finalized `block_zk_gas_used` directly into `header.difficulty`.

Do not:

- add it to an existing difficulty value,
- derive it from `mix_hash`,
- or populate it before execution is finalized.

`mix_hash` remains `block_env.prevrandao()` as it does today.

## Execution Rules

### Per-opcode charging

For every executed opcode:

1. Compute `step_gas` as the EVM gas consumed by that opcode step.
2. Determine whether the opcode is a spawn opcode:
   - `CALL`
   - `CALLCODE`
   - `DELEGATECALL`
   - `STATICCALL`
   - `CREATE`
   - `CREATE2`
3. If the spawn opcode actually created a child frame or invoked a precompile, use the fixed `SPAWN_ESTIMATE[opcode]` as raw gas.
4. Otherwise use `step_gas` as raw gas.
5. Charge `raw_gas * opcode_multiplier[opcode]`.
6. If `block_zk_gas_used + tx_zk_gas_used > block_limit`, halt immediately.

### Precompile charging

When a CALL-family opcode resolves to an active precompile:

- the CALL-family opcode itself still uses the spawn estimate path above,
- the precompile execution is charged separately as
  `precompile_gas_used * precompile_multiplier[address_low_byte]`,
- the limit check runs again immediately after charging the precompile.

### Overflow behavior

Any checked arithmetic failure is treated exactly like zk gas limit exceed:

- current transaction aborts,
- current transaction is discarded,
- block execution stops.

## Block Execution Semantics

### Local payload building

For Uzen blocks:

1. Initialize `block_zk_gas_used = 0`.
2. Before each transaction, reset `tx_zk_gas_used = 0`.
3. Execute the transaction under the Uzen meter.
4. If execution succeeds, commit the transaction normally and promote `tx_zk_gas_used` into `block_zk_gas_used`.
5. If execution halts due to zk gas exceed, do not commit the transaction and stop processing the block.
6. Do not attempt to execute any later transactions.

### Imported block execution

Imported Uzen blocks must be re-executed under the same meter.

The canonical body is valid only if execution behavior matches the body boundary:

- if all listed transactions fully execute, the body is valid only if `difficulty == recomputed_block_zk_gas`,
- if the meter would halt inside transaction `N`, the body is valid only if transaction `N` and every later transaction are absent.

This means a body that contains the offending transaction, or any transaction after it, is invalid even if the header difficulty is otherwise plausible.

## Consensus Validation

### Standalone header validation

In `crates/consensus/src/validation/mod.rs`:

- keep enforcing `difficulty == 0` before Uzen,
- stop enforcing that rule at Uzen activation timestamps,
- keep existing nonce / ommer / gas / base-fee checks unchanged unless another Uzen rule explicitly changes them.

### Post-execution validation

Add a Uzen-specific consensus validation step that:

- reuses the same metering-enabled execution path,
- recomputes finalized block zk gas,
- checks that `header.difficulty == recomputed_block_zk_gas`,
- rejects any block body that extends past the truncation point,
- rejects any block body that includes the offending transaction.

The implementation should prefer shared executor logic over a separate handwritten replay validator, to avoid drift.

## Error Model

Introduce a distinct execution error for Uzen zk gas exhaustion.

Requirements:

- it must be distinguishable from normal transaction validation failures,
- it must cause `revm` to discard the current transaction state,
- it must let the block executor stop block execution cleanly,
- it must not be treated as a generic “skip this tx and continue” error in payload building.

The payload builder and tx-selection paths should interpret this error as:

- do not commit the offending transaction,
- stop building the block immediately.

## Testing Strategy

### Unit tests

- opcode charge uses `step_gas * multiplier` for ordinary opcodes,
- spawn opcodes use fixed estimates only when a child frame or precompile dispatch actually occurs,
- precompile charges use the precompile multiplier table,
- checked arithmetic overflow triggers the limit-exceeded path,
- schedule selection returns Uzen only for `TaikoSpecId::UZEN`.

### Execution tests

- block with all txs under the limit yields `header.difficulty == total_block_zk_gas`,
- transaction exceeding the limit is discarded,
- later transactions are skipped,
- earlier committed transactions remain in the block,
- offending transaction receipt is absent,
- imported Uzen block with extra transactions after truncation is rejected,
- imported Uzen block with mismatched `difficulty` is rejected.

### Regression tests

- pre-Uzen blocks still require `difficulty == 0`,
- `mix_hash` behavior remains unchanged before and after Uzen,
- existing non-Uzen payload-building behavior is preserved.

## Compatibility Notes

- Downstream Taiko components that currently assume post-merge Taiko headers always have `difficulty == 0` will need Uzen updates.
- Within this repository, Uzen consensus should be explicit and deterministic at the validation boundary, regardless of downstream client readiness.

## Implementation Outline

1. Add zk gas schedule and meter types under `crates/evm/src/zk_gas`.
2. Add Uzen execution adapter that charges opcodes and precompiles and raises the dedicated halt error.
3. Thread shared zk gas state through block execution context.
4. Update the block executor to stop on zk gas exceed and keep only fully committed transactions.
5. Populate Uzen `header.difficulty` from finalized block zk gas in the assembler path.
6. Add Uzen consensus validation for recomputed difficulty and truncation correctness.
7. Add unit and integration coverage across execution, payload building, and import validation.

## Open Questions

None at this stage. The remaining work is implementation planning and execution.
