# Taiko `newPayload` Uzen Hash Mismatch Design

## Summary

Fix the narrow `engine_newPayloadV2` hash-mismatch bug in `alethia-reth` for Taiko devnet Uzen
blocks.

Today the payload builder and payload validator reconstruct different block headers for the same
payload:

- the block builder seals Uzen blocks with `header.difficulty = finalized_block_zk_gas`
- the payload validator reconstructs a block from `ExecutionPayloadV1::try_into_block()`
- upstream `ExecutionPayloadV1` conversion defaults header `difficulty` to zero
- the validator then seals a different header and rejects the payload with `block hash mismatch`

The fix is to make Taiko payload validation reconstruct the same header shape the Taiko builder
uses, without changing the engine RPC payload shape and without bundling the separate Cancun blob
environment issue.

## Scope

In scope:

- `engine_newPayloadV2` validation for Taiko execution payloads
- Taiko-local payload-to-block reconstruction used by the engine validator
- regression tests proving builder and validator agree for Uzen payloads

Out of scope:

- the earlier `excess blob gas missing in the EVM's environment after Cancun` error
- changes to driver payload generation
- broader engine API compatibility or payload version migrations

## Context

Relevant current behavior:

- `crates/block/src/assembler.rs` writes Uzen header `difficulty` from finalized zk gas
- `crates/rpc/src/engine/validator.rs` reconstructs blocks using
  `ExecutionPayloadV1::try_into_block()`
- upstream `ExecutionPayloadV1` conversion fills `difficulty` with zero

This causes `engine_newPayloadV2` to reject payloads that the local builder itself produced.

## Approaches Considered

### 1. Taiko-specific reconstruction in the validator

Recommended.

Implement a Taiko-local helper in the engine validator that reconstructs a block from
`TaikoExecutionData`, then applies Taiko header normalization before sealing.

Pros:

- fixes the bug at the exact failing boundary
- preserves strict hash validation
- keeps the wire format stable
- does not require driver changes

Cons:

- adds Taiko-specific reconstruction logic in the validator

### 2. Add explicit `difficulty` to Taiko execution payload types

Rejected for this change.

Pros:

- explicit data model

Cons:

- changes the engine payload shape
- requires coordinated producer and consumer changes
- larger compatibility surface than needed for this bug

### 3. Relax hash validation for Uzen payloads

Rejected.

Pros:

- smallest patch

Cons:

- hides the mismatch instead of fixing deterministic reconstruction
- weakens validation guarantees

## Chosen Design

Add a Taiko-specific conversion helper near the engine validator and use it from
`TaikoEngineValidator::convert_payload_to_block`.

### Reconstruction Rules

The helper will:

1. start from the execution payload fields already sent over the engine API
2. decode transactions into a block body
3. overwrite `transactions_root` from `taiko_sidecar.tx_hash` when present
4. overwrite `withdrawals_root` from `taiko_sidecar.withdrawals_hash` and attach an empty
   withdrawals body when present
5. restore Taiko/Uzen header `difficulty` instead of leaving the upstream default zero
6. seal the reconstructed block and compare it to the submitted `block_hash`

### Uzen Difficulty Handling

For this narrow fix, the validator must reconstruct the same Uzen header shape the local builder
sealed. The chosen rule is:

- pre-Uzen behavior remains unchanged
- for Uzen-active payloads, the validator uses Taiko-specific payload data to restore the header
  `difficulty` before sealing

This preserves strict validation while aligning the validator with builder-side block assembly.

## Error Handling

`newPayload` remains strict.

- If reconstruction matches, validation continues unchanged.
- If reconstruction still diverges, the node continues to return `Invalid payload`.

The fix must not bypass the block-hash comparison.

## Testing

Add focused regression coverage for the engine validator path:

1. A positive test that builds a Uzen block through the existing builder path, converts it to
   `TaikoExecutionData`, and validates it successfully through `TaikoEngineValidator`.
2. A negative test that mutates the Taiko-specific header-relevant input and confirms validation
   still fails with a block-hash mismatch.

These tests prove:

- builder and validator now agree for valid Uzen payloads
- hash validation still catches malformed payloads

## Risks

- If the reconstruction helper misses another Taiko-only header rule, the hash mismatch may
  persist for later payloads even if block 1 succeeds.
- Keeping Taiko reconstruction logic in the validator means future Taiko header changes must update
  both builder and validator paths.

## Follow-up

After this narrow fix, the separate Cancun/blob environment bug should be handled in a dedicated
change.
