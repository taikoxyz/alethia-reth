# Beacon Root Override Integration Design

## Context

PR #227 adds revmc JIT execution, while PR #228 independently fixes propagation of
`eth_simulateV1`'s `blockOverrides.beaconRoot` into the Unzen EIP-4788 system call. Merging #227
without the beacon-root portion of #228 would retain the simulation regression. The JIT-rejection
portion of #228 must not be included because #227 intentionally supports `re-execute --jit`.

## Scope

Carry an optional parent beacon block root through `TaikoNextBlockEnvAttributes` and every
constructor of that type. Pending RPC execution reads the value from `BlockOverrides`; derived
blocks preserve the header value; payload building forwards the payload attribute; call sites
without an override pass `None`.

No JIT configuration, CLI validation, fork activation, or unrelated RPC behavior changes.

## Execution Semantics

`ConfigureEvm::context_for_next_block` passes the optional value to
`normalize_parent_beacon_block_root`:

- before Unzen, any supplied value is discarded and the execution context contains `None`;
- at Unzen, a supplied value is preserved;
- at Unzen without an override, the existing `B256::ZERO` fallback remains.

This keeps the fork boundary and fallback behavior centralized in the existing normalization
helper.

## Affected Call Sites

- `BuildPendingEnv` extracts `BlockOverrides::beacon_root`;
- derived-block conversion copies `Header::parent_beacon_block_root`;
- payload building forwards the payload attribute;
- executor tests and authenticated RPC construction use `None` when no override exists.

## Testing

Use test-driven development:

1. Add a focused regression test that builds a pending Unzen environment with a nonzero beacon-root
   override and asserts the resulting execution context preserves it.
2. Run the focused test and confirm it fails because the current code substitutes the zero root.
3. Add the propagation field and update all constructors.
4. Re-run the focused test, the affected crate tests, formatting, clippy, and the workspace test
   suite.

The implementation will be reviewed independently before publication.

## Publication

Commit the behavior change separately as `fix(evm): honor beacon root overrides` and push it to
PR #227's existing `feat/evm-revmc-jit` source branch. PR #228 remains unmerged because its
`re-execute --jit` rejection conflicts with #227.
