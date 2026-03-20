# Anchor Zk Gas Payload Error Design

## Context

In new-mode payload building, the payload builder injects the prebuilt anchor
transaction before selecting pool transactions.

Today, if the anchor transaction hits the dedicated zk gas truncation condition,
`execute_anchor_and_pool_transactions` logs the event and returns
`Ok(ExecutionOutcome::Completed(U256::ZERO))`. That treats anchor zk gas
exhaustion as a clean stop and allows payload construction to continue as if an
empty payload had been built successfully.

The requested behavior is narrower and stricter:

- only the new-mode anchor path changes,
- anchor zk gas exhaustion should return a dedicated new error,
- legacy-mode zk gas truncation behavior stays unchanged.

## Goal

Return a dedicated payload-builder error when the new-mode anchor transaction
exceeds the zk gas limit.

## Non-Goals

- No change to legacy-mode zk gas truncation behavior.
- No change to pool-transaction selection behavior after anchor succeeds.
- No change to the dedicated zk gas sentinel string or block-level truncation
  semantics.
- No broad payload-builder error refactor.

## Approaches Considered

### 1. Reuse the existing EVM error path

Change the anchor zk gas branch to `return Err(PayloadBuilderError::evm(err));`.

Pros:

- Smallest code change.

Cons:

- Does not create a new dedicated payload-layer error.
- Makes anchor zk gas exhaustion indistinguishable from other fatal EVM errors
  in the payload builder.

### 2. Add a dedicated local payload-builder error type

Introduce a small typed error in the payload execution helper layer and map only
the new-mode anchor zk gas branch to `PayloadBuilderError::other(...)`.

Pros:

- Matches the requested semantics exactly.
- Keeps the change local to the payload builder.
- Preserves existing error handling for all non-zk-gas anchor failures.

Cons:

- Slightly more code than reusing `PayloadBuilderError::evm(err)`.

### 3. Broaden the change to every payload-builder zk gas stop path

Make both legacy-mode and new-mode payload zk gas exhaustion return errors.

Pros:

- Uniform payload-builder semantics.

Cons:

- Explicitly outside requested scope.
- Changes already-established clean-stop behavior in legacy mode.

## Recommended Approach

Use approach 2.

Add a dedicated local error for new-mode anchor zk gas exhaustion and map only
that branch to `PayloadBuilderError::other(...)`.

## Detailed Design

### Error Type

Define a dedicated payload-layer error in
`crates/payload/src/builder/execution.rs` for the new-mode anchor zk gas case.

Properties:

- documented with rustdoc,
- implements `Display` with a stable, purpose-specific message,
- implements `std::error::Error`,
- used only for the new-mode anchor zk gas branch.

This keeps the payload-builder surface explicit without changing upstream error
contracts.

### Execution Flow Change

In `execute_anchor_and_pool_transactions`, change only this branch:

- current behavior:
  - log anchor zk gas exhaustion,
  - return `Ok(ExecutionOutcome::Completed(U256::ZERO))`.

- new behavior:
  - log anchor zk gas exhaustion,
  - return `Err(PayloadBuilderError::other(AnchorZkGasLimitExceeded))`.

All other branches remain unchanged:

- successful anchor execution still continues to pool selection,
- other anchor failures still return `PayloadBuilderError::evm(err)`,
- legacy-mode provided-transaction execution still stops cleanly on zk gas
  exhaustion instead of returning an error.

### Testing

Add a focused payload-builder test for the new-mode anchor case.

The test should:

- construct a new-mode execution context with a limit-exceeding anchor
  transaction,
- call `execute_anchor_and_pool_transactions`,
- assert that the function now returns an error,
- assert that the returned `PayloadBuilderError` uses the dedicated
  `Other(...)` path,
- assert that the boxed source downcasts to the new
  `AnchorZkGasLimitExceeded` type,
- treat any message assertion as optional secondary validation only,
- avoid changing the existing legacy-mode zk gas regression.

Keep the current legacy-mode clean-stop test intact so the scope boundary stays
visible in tests.

Add a second focused regression for scope preservation.

That test should:

- force a non-zk-gas anchor execution failure in the same new-mode helper,
- assert that the result still maps to `PayloadBuilderError::evm(err)`,
- assert that this failure is not remapped into the new
  `AnchorZkGasLimitExceeded` type.

## Risks

- Low risk if the change stays confined to the anchor branch.
- The main risk is accidentally changing broader payload-builder zk gas handling
  and turning non-anchor truncation into hard failures.

## Verification Plan

- Run the new focused payload test for anchor zk gas exhaustion.
- Run the focused payload test for ordinary non-zk-gas anchor execution
  failure.
- Re-run the existing payload legacy-mode zk gas test.
- Run the relevant payload crate tests.
- Run `just clippy` before completion.
