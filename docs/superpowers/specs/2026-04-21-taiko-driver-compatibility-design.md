# Taiko Driver Compatibility Design

Date: 2026-04-21
Repo: `/Users/davidcai/taiko/alethia-reth`
Related consumer: `/Users/davidcai/taiko/taiko-mono/packages/taiko-client`

## Summary

Implement a narrow geth-compatibility shim in `alethia-reth` so the current `taiko-client driver` can run unchanged against `alethia-reth`.

The compatibility surface is limited to:

- Taiko L1 origin RPC responses consumed by `taiko-client`
- `engine_exchangeTransitionConfigurationV1`

The design keeps `alethia-reth` internal storage and preconfirmation semantics intact. Only the outward RPC and engine behavior is aligned where the current driver relies on geth-compatible responses.

## Context

The current `taiko-client driver` assumes geth-compatible behavior in two places:

1. Preconfirmation `L1Origin` responses are returned with zero-valued fields present, especially `l1BlockHeight`.
2. The engine API supports `engine_exchangeTransitionConfigurationV1`.

With `taiko-geth`, the driver works because the wire behavior matches those assumptions. With `alethia-reth`, two compatibility gaps appear:

- preconfirmation `L1Origin` values are returned with `l1_block_height: None` on RPC, which causes the driver to dereference a nil `*big.Int` in its preconfirmation request path
- `engine_exchangeTransitionConfigurationV1` is missing, producing periodic `Method not found` errors

The observed production symptom is repeated driver restarts caused by a nil-pointer panic during preconfirmation block request handling.

## Goal

Make the current `taiko-client driver` run unchanged against `alethia-reth`.

Success means:

- no driver panic caused by preconfirmation `L1Origin` RPC shape
- no periodic transition-configuration method-not-found errors
- no changes required in `taiko-client`

## Non-Goals

- broad Taiko RPC parity beyond what the current driver uses
- refactoring `alethia-reth` storage or payload-building internals
- changing how `alethia-reth` internally represents preconfirmation state
- fixing unrelated driver assumptions outside the current compatibility surface

## Approaches Considered

### 1. Narrow compatibility shim at the RPC boundary

Add a thin outward compatibility layer for the specific Taiko RPC and engine methods that the current driver depends on.

Pros:

- smallest blast radius
- preserves current internal semantics
- directly addresses the live compatibility failures

Cons:

- does not proactively solve future parity gaps outside the current driver path

### 2. Fix only `L1Origin` response parity

Align only the Taiko `L1Origin` response shape and leave engine transition configuration unimplemented.

Pros:

- smallest code change

Cons:

- leaves known driver incompatibility in place
- does not fully satisfy "run unchanged"

### 3. Broad Taiko-facing parity pass

Audit and align a larger set of Taiko RPC and engine behavior for long-term compatibility.

Pros:

- more future-proof

Cons:

- larger scope than required
- slower to ship

## Selected Approach

Approach 1: add a narrow compatibility shim in the Taiko RPC and engine boundary for the exact calls the current `taiko-client driver` depends on.

## Compatibility Scope

The implementation will cover these methods:

- `taiko_l1OriginByID`
- `taiko_headL1Origin`
- `taikoAuth_lastL1OriginByBatchID`
- `taikoAuth_lastCertainL1OriginByBatchID`
- `engine_exchangeTransitionConfigurationV1`

No other APIs are in scope unless required by implementation details.

## Current State

### L1 origin representation

`alethia-reth` currently stores preconfirmation origins internally with zero values in storage, then converts zero-valued optional fields back to `None` in outward RPC responses.

This behavior is correct for its internal model, but it differs from `taiko-geth`'s effective wire compatibility for Go clients. The current `taiko-client driver` assumes the geth-compatible wire shape and is not nil-safe in at least one active path.

### Transition configuration

`taiko-client driver` periodically calls `engine_exchangeTransitionConfigurationV1` with a zeroed transition configuration. `taiko-geth` implements this endpoint. `alethia-reth` currently does not, resulting in recurring error logs.

## Design

### 1. Add geth-compatible outward `L1Origin` translation

Keep `alethia-reth` storage and internal `RpcL1Origin` semantics unchanged. Add a compatibility translation only when returning Taiko RPC/auth responses to clients that currently expect geth-compatible values.

For preconfirmation origins:

- return `l1BlockHeight` as zero rather than omitting it
- return `l1BlockHash` as zero rather than omitting it when applicable
- preserve the existing signature, forced-inclusion flag, block ID, and L2 block hash

This compatibility translation must apply to:

- `taiko_l1OriginByID`
- `taiko_headL1Origin`
- `taikoAuth_lastL1OriginByBatchID`
- `taikoAuth_lastCertainL1OriginByBatchID`

The conversion should happen at the outward RPC boundary rather than changing storage or preconfirmation classification.

### 2. Keep internal preconfirmation semantics unchanged

`alethia-reth` should continue to treat preconfirmation origins internally as optional-or-zero semantics. The compatibility layer should not alter:

- how preconfirmation blocks are detected
- how origins are stored
- how head origin pointers are persisted
- how batch-to-last-block mappings are derived

This keeps the compatibility change narrow and avoids coupling execution logic to geth wire quirks.

### 3. Implement `engine_exchangeTransitionConfigurationV1`

Add support for `engine_exchangeTransitionConfigurationV1` in `alethia-reth`'s engine RPC surface.

Required behavior for current driver compatibility:

- the method exists
- it accepts a non-nil `TerminalTotalDifficulty`
- the current driver call pattern with zero TTD, zero block hash, and zero block number succeeds
- it returns a response compatible with what the driver expects from geth

The implementation does not need a broader merge-era design exercise. It only needs to satisfy the current driver's compatibility contract.

## Implementation Shape

### L1 origin response compatibility

Implementation shape:

- keep `StoredL1Origin` and `RpcL1Origin` unchanged for internal use
- add a dedicated geth-compatibility conversion for Taiko-facing outward responses in the RPC layer
- route the four covered methods through that compatibility conversion before returning values

This avoids silently changing every use of `into_rpc()` if other internal callers benefit from the current optional representation.

### Transition configuration compatibility

Implementation shape:

- extend `crates/rpc/src/engine/api.rs` so the Taiko engine RPC trait exposes `exchangeTransitionConfigurationV1`
- add the method to the supported engine capabilities list
- implement the handler in the existing Taiko engine API type rather than introducing a separate compatibility module
- validate the input to the level required by the current driver request pattern and return a deterministic geth-compatible response

## Error Handling

For `L1Origin` compatibility:

- preserve existing not-found behavior
- do not fabricate a found origin when none exists
- only translate the representation of found preconfirmation values

For transition configuration:

- preserve normal JSON-RPC error handling for malformed requests
- avoid returning `Method not found`
- prefer deterministic validation failures over permissive undefined behavior

## Testing

Add focused compatibility tests around the new boundary behavior.

### L1 origin tests

For each covered Taiko method:

- insert a preconfirmation origin in storage
- confirm the outward RPC response returns geth-compatible zero-valued fields instead of omitted optionals

Covered methods:

- `taiko_l1OriginByID`
- `taiko_headL1Origin`
- `taikoAuth_lastL1OriginByBatchID`
- `taikoAuth_lastCertainL1OriginByBatchID`

Also include:

- at least one confirmed-origin control case
- not-found behavior checks where applicable

### Transition configuration tests

Add an engine RPC test that:

- calls `engine_exchangeTransitionConfigurationV1`
- uses the same zeroed config shape as the current driver
- verifies the method succeeds and returns a compatible response

## Verification Plan

After implementation, verify against the live deployment shape used today:

1. Redeploy `alethia-reth` with the compatibility patch.
2. Observe `l2-node-reth` driver logs.
3. Confirm:
   - no nil-pointer panic in preconfirmation request handling
   - no `Failed to exchange Transition Configuration error="Method not found"` logs
   - driver restart count stops increasing under normal load

## Risks

### Over-broad response translation

If the compatibility conversion replaces the default `RpcL1Origin` shape globally, it may affect future consumers that prefer explicit optionals.

Mitigation:

- keep the translation localized to the covered Taiko outward methods

### Incomplete geth parity in transition configuration

If the new engine method exists but behaves too differently from geth, the current driver may stop logging errors but still encounter subtle issues later.

Mitigation:

- match geth's current behavior closely for the zero-TTD probe shape used by the driver
- test directly against the expected driver request

## Acceptance Criteria

- preconfirmation `L1Origin` responses from covered methods are geth-compatible for the current Go driver
- `engine_exchangeTransitionConfigurationV1` is implemented and compatible with the driver's existing call pattern
- the current `taiko-client driver` runs unchanged against `alethia-reth`
- no changes are required in `/Users/davidcai/taiko/taiko-mono/packages/taiko-client`

## Out-of-Scope Follow-Up

If additional geth-vs-reth mismatches are discovered later, handle them as separate compatibility tasks. Do not expand this work unless a newly discovered issue blocks the current driver from running unchanged.
