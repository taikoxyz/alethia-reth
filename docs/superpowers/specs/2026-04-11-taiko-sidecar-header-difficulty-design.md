# Remove `header_difficulty` from `TaikoExecutionDataSidecar`

## Summary

`TaikoExecutionDataSidecar` should stop exposing `header_difficulty` as part of the client-facing
engine payload contract. Clients should not need to calculate, track, or submit this field.
`alethia-reth` should reconstruct hash-relevant header state internally when possible and otherwise
continue validating payloads with best-effort behavior.

## Current Problem

The sidecar currently contains `header_difficulty`, even though it is an internal implementation
detail needed only to reconstruct a Uzen-era block header before hash validation. That leaks
consensus reconstruction details into the RPC contract and forces external clients to worry about a
field they should not own.

Today:

- Built payloads include `header_difficulty` in the sidecar.
- Incoming `newPayloadV2` requests may rely on sidecar-provided or cache-hydrated
  `header_difficulty`.
- Uzen payload hash validation can fail if the node cannot restore the original difficulty.

## Goals

- Remove `header_difficulty` from the serialized `TaikoExecutionDataSidecar` schema.
- Ensure clients can omit the field entirely with no compatibility burden on their side.
- Preserve successful validation for locally built payloads by restoring difficulty internally.
- Accept incoming payloads unconditionally with respect to this missing field, while keeping the
  existing block-hash validation semantics for cases where reconstruction is impossible.

## Non-Goals

- Changing the external `newPayload` method version or overall payload envelope shape beyond
  removing this field.
- Weakening block-hash validation or accepting hash mismatches.
- Redesigning zk-gas or Uzen difficulty computation beyond what is required to restore the header
  internally.

## Proposed Design

### Wire Contract

Remove `header_difficulty` from `TaikoExecutionDataSidecar`. After this change, the sidecar carries
only client-owned Taiko metadata:

- `tx_hash`
- `withdrawals_hash`
- `taiko_block`

No client request or response should include `headerDifficulty`.

### Internal Restoration Path

`alethia-reth` remains responsible for reconstructing hash-relevant header fields before sealing and
hash comparison.

Recommended behavior:

1. Continue caching built payload header difficulty keyed by built block hash for locally produced
   payloads.
2. When `newPayloadV2` receives a payload, restore the cached difficulty internally if the payload
   hash matches a locally built payload.
3. Pass the restored internal value through a node-local path that is not part of the external
   sidecar schema.
4. If no cached/internal value exists, continue without it rather than rejecting the payload for
   schema reasons alone.
5. Preserve the existing final block-hash comparison. If the node cannot reconstruct the original
   difficulty and the block hash therefore differs, return the existing hash-mismatch error.

This keeps the field out of the wire format without silently accepting malformed blocks.

### Validator Behavior

The validator should consume an internal optional restored difficulty value instead of reading it
from the sidecar. It should:

- set `block.header.difficulty` when an internally restored value exists
- leave the payload-derived value unchanged when no internal value exists
- continue applying the existing Uzen `parent_beacon_block_root` normalization
- continue using the final sealed hash equality check as the authoritative validation gate

### Testing

Update tests to reflect the new contract:

- sidecar serialization/deserialization no longer includes `headerDifficulty`
- locally built Uzen payloads still validate after internal restoration
- payloads without an internally restorable difficulty are accepted by schema conversion but still
  fail later if the sealed block hash does not match

## Risks and Mitigations

- Risk: some internal code path still expects `header_difficulty` on `TaikoExecutionDataSidecar`.
  Mitigation: replace that dependency with a node-local restoration field or separate internal
  wrapper before removing the serialized field.
- Risk: removing the field changes snapshot or serde expectations.
  Mitigation: add or update targeted serde tests around the sidecar schema.
- Risk: built payload restoration is accidentally lost.
  Mitigation: keep the payload-hash-keyed cache and cover it with validator/API tests.

## Implementation Outline

1. Remove `header_difficulty` from `TaikoExecutionDataSidecar`.
2. Introduce a node-local mechanism for carrying restored difficulty outside the RPC schema.
3. Update built-payload conversion and `newPayloadV2` hydration to use the internal mechanism.
4. Update validator reconstruction to consume the internal restored value.
5. Rewrite tests for the new schema and best-effort restoration behavior.
