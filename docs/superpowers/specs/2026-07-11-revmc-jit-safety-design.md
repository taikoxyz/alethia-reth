# Safe revmc JIT Integration

## Context

Pull request #218 adds optional revmc JIT execution to Alethia Reth. The latest
revision addresses the original platform and channel-capacity failures, but two
correctness boundaries remain unsafe:

- the pinned revmc revision can produce a different halt result and journaled
  state from the interpreter when dynamic-gas failure ordering matters; and
- Reth's canonical Engine and RPC paths share `ConfigureEvm` methods, so changing
  the global factory to protect ordinary RPC calls also disables JIT execution
  for live payload validation while still leaving pending-block RPC construction
  on the JIT-capable path.

The runtime control channel also needs a failure-safe contract: controls must not
block when the worker has not started, has failed to start, or is racing with a
state transition.

## Goals

- Make JIT execution observationally equivalent to interpreter execution for the
  known dynamic-gas failure regression.
- Keep the canonical Engine and payload-building paths JIT-capable when the
  operator explicitly enables JIT.
- Keep `eth_call`, tracing, simulation, estimation, and pending-block RPC work on
  an interpreter-only EVM configuration.
- Make pause, resume, and cache-clear controls non-blocking and return explicit
  errors when no running worker can accept them.
- Preserve the PR's existing non-zero channel validation and Linux/LLVM build
  fixes.
- Avoid upgrading the broader Reth/revm dependency graph in this PR.

## Non-goals

- Enabling JIT by default.
- Changing the RPC surface or its authorization model.
- General revmc feature development beyond the correctness and lifecycle fixes
  needed by this integration.
- Replacing the pinned Reth revision.

## Dependency Strategy

The repository will vendor the compatible revmc source corresponding to the
currently pinned `4042c2e` revision and point the workspace dependency at that
local source. The vendored copy will retain upstream licensing and provenance.
Only the compatible portions of these upstream changes will be backported:

- revmc #395: end stack sections around operations that require `gasleft`, and
  preserve the runtime's exact-error configuration so gas failures occur in the
  same order as the interpreter;
- revmc #391: track whether the compilation worker actually started and use
  non-blocking control delivery.

Cache clearing will receive the same started-worker and non-blocking treatment as
pause and resume. This avoids introducing an external fork or a repository-owned
remote branch while keeping the Cargo build reproducible.

## EVM Configuration Boundaries

`TaikoEvmConfig` remains the canonical node configuration. Its direct and block
executor factories stay JIT-capable so Engine payload validation, canonical block
execution, and payload building all honor the operator's JIT setting.

An explicit interpreter-only clone of that configuration will be constructed for
the Ethereum RPC API. `TaikoEthApiBuilder` will build `EthApi` with this scoped
configuration while preserving the cache, gas cap, simulation limits, fee
settings, permits, pending-block mode, raw transaction forwarder, and other
settings supplied by Reth's RPC context.

This split follows the actual ownership boundary instead of trying to infer the
caller inside shared `ConfigureEvm` methods:

```text
canonical node config (JIT-capable)
  -> Engine payload validation
  -> canonical block execution
  -> payload construction

RPC-scoped config (interpreter-only)
  -> call / estimate / trace / simulate
  -> pending-block RPC construction
```

Both configurations may share the backend's administrative handle, but RPC
execution factories must have JIT support disabled structurally rather than by a
temporary global toggle.

## Runtime Control Contract

The backend lifecycle distinguishes "enabled by configuration" from "worker has
started and can receive controls." Enabling must not expose a control-ready state
until worker startup succeeds. Pause, resume, and cache clear use `try_send` (or an
equivalent bounded non-blocking operation) and report a typed error for these
conditions:

- JIT is disabled;
- the worker has not started or failed to start;
- the control queue is full;
- the worker has disconnected.

No RPC control call may wait indefinitely for a receiver.

## Tests

Implementation follows test-first development:

1. Replace the documented divergence test with an equality regression asserting
   identical result, gas, halt reason, and state for interpreter and JIT paths.
2. Add routing tests proving canonical Engine/block factories remain JIT-capable
   while the RPC-scoped configuration is interpreter-only, including its
   next-block builder.
3. Add backend tests for controls before startup, after failed/unavailable
   startup, and with a full bounded queue, asserting prompt errors instead of
   blocking.
4. Retain and run the existing zero-capacity parsing and execution tests.

## Verification

Before pushing, run targeted EVM, RPC, block, and CLI tests; `just fmt`;
`just clippy`; the workspace test suite; and the relevant default-feature,
no-default-feature, and release/build checks. A focused code review will then
check the final diff for routing regressions, blocking behavior, missing
documentation, and accidental changes outside the approved scope.

## Rollback

The feature remains opt-in. If an unexpected JIT issue is discovered after this
change, operators can leave JIT disabled without changing canonical interpreter
behavior. The vendored dependency and scoped RPC configuration are isolated so
they can be reverted without changing chain data formats or RPC schemas.
