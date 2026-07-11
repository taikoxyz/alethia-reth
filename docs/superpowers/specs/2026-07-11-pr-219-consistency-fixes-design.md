# PR 219 Consistency Fixes Design

## Goal

Close the remaining consistency gaps in PR 219 so every applied canonical promotion persists its
pending L1 origin and startup reconciliation never mutates or serves state after an incomplete
repair.

## Scope

The patch changes only the Taiko engine API and its builder wiring, plus focused tests. It does not
attempt to solve the PR's documented residual around `BatchToLastBlock` entries at or below the
persisted head whose proposal content changes during a same-height reorg.

## Applied Forkchoice Errors

Reth follows the Engine API requirement that invalid payload attributes do not roll back a valid
forkchoice update. It applies the supplied state without starting a build and then returns
`UpdatedInvalidPayloadAttributes`. The Taiko wrapper must treat that specific error as an applied
forkchoice for pending-origin bookkeeping.

The wrapper will centralize pending-origin persistence in one helper. A normal `VALID` response and
`UpdatedInvalidPayloadAttributes` will both invoke that helper for the requested head hash. Other
errors will not persist anything. A persistence failure will re-buffer the entry and return the
persistence error so a retry can complete the durable write.

## Reconciliation Error Handling

The predicate used to decide whether a stored origin row can back `head_l1_origin` will return a
`Result`. Database reads and canonical-hash lookups will propagate their errors instead of being
interpreted as a missing or noncanonical row. Any such error aborts the write transaction, leaving
the existing pointer unchanged.

Startup reconciliation becomes a construction invariant. `TaikoEngineApi::new` will return a
`Result`, and the engine API builder will propagate reconciliation failures instead of starting an
authenticated engine endpoint over known-unreconciled custom tables.

## Testing

Tests will be written before production changes and observed failing for the intended reason. They
will cover:

- recognizing `UpdatedInvalidPayloadAttributes` as an applied forkchoice while rejecting unrelated
  errors;
- propagating origin-table and canonical-hash lookup failures through head reconciliation;
- preserving existing keep, clamp, and clear behavior after the resolver becomes fallible;
- constructor/builder compilation after startup reconciliation becomes mandatory.

Final verification will run formatting, the repository-required clippy gate, targeted RPC tests,
and the full workspace test command before the branch is committed and pushed.

## Publishing

The implementation will be committed with a Conventional Commit message and pushed as a
fast-forward update to `origin/david/defer-l1-origin-persistence`, updating PR 219 in place.
