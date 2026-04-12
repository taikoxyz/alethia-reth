# Header Difficulty In `engine_getPayloadV2`

## Summary

Extend Taiko's `engine_getPayloadV2` response with an optional top-level `headerDifficulty`
field. The field carries the finalized Uzen `header.difficulty` value, which is the finalized
block zk gas committed into the built block header.

The goal is to make Taiko clients always pass the original header difficulty back through
`engine_newPayloadV2`, instead of relying on per-node cache hydration.

This extension must remain backward compatible with clients that only understand the standard
`ExecutionPayloadEnvelopeV2` response shape.

## Problem

After Uzen, Taiko writes finalized zk gas into `block.header.difficulty`. That field is
hash-relevant for imported payload validation and round-tripping built payloads.

Today:

- The built block header contains the correct difficulty.
- `engine_getPayloadV2` returns the standard `ExecutionPayloadEnvelopeV2` shape, which does not
  expose header difficulty.
- `engine_newPayloadV2` accepts Taiko-specific top-level fields including `headerDifficulty`.
- Local same-node round trips work only because `alethia-reth` caches built header difficulty by
  block hash and hydrates it during `engine_newPayloadV2` when the client omits it.

This means the system does not provide a robust protocol-level guarantee that clients can always
pass the original header difficulty across nodes, restarts, or offline transport.

## Requirements

1. A Taiko client must be able to obtain the original header difficulty from
   `engine_getPayloadV2`.
2. A Taiko client must always pass that value back in `engine_newPayloadV2`.
3. Existing clients that deserialize `engine_getPayloadV2` as the standard
   `ExecutionPayloadEnvelopeV2` must continue to work.
4. Existing servers that do not yet return `headerDifficulty` must remain readable by updated
   Taiko clients during rollout.
5. `blockValue` semantics must remain unchanged.

## Non-Goals

- Do not overload `blockValue` with zk gas or header difficulty.
- Do not introduce a second RPC round trip for payload metadata.
- Do not require upstream Engine API type changes.

## Chosen Approach

Add an optional Taiko-specific top-level `headerDifficulty` field to the JSON object returned by
`engine_getPayloadV2`.

Example response shape:

```json
{
  "executionPayload": { "...": "..." },
  "blockValue": "0x0",
  "headerDifficulty": "0x2a"
}
```

The existing `executionPayload` and `blockValue` fields remain unchanged. The new field is
optional and only carries Taiko metadata.

## Compatibility Model

### Old client -> new server

Safe. Old clients that deserialize into `ExecutionPayloadEnvelopeV2` should ignore the unknown
`headerDifficulty` field.

### New client -> old server

Safe during rollout. Updated clients deserialize `headerDifficulty` as optional. If the field is
absent, the client receives `None`.

Operationally, "always pass" becomes guaranteed only once the client talks to a server version
that implements this extension.

### New client -> new server

Desired steady state. The client reads `headerDifficulty` from `engine_getPayloadV2` and always
echoes it into `engine_newPayloadV2`.

## Server Design (`alethia-reth`)

### New response wrapper

Introduce a Taiko response wrapper type for `engine_getPayloadV2`:

- `envelope: ExecutionPayloadEnvelopeV2` via `#[serde(flatten)]`
- `header_difficulty: Option<U256>` serialized as `headerDifficulty`

This wrapper remains wire-compatible with the standard response because it flattens the standard
envelope and only adds one optional field.

### Conversion

Replace the current direct `From<EthBuiltPayload>` path used by Taiko `getPayloadV2` with a
Taiko-specific conversion that:

- builds the normal `ExecutionPayloadEnvelopeV2`
- extracts `built_payload.block().header().difficulty`
- includes it as `headerDifficulty`

### API surface

`engine_getPayloadV2` in Taiko's RPC trait should return the new wrapper type instead of the bare
standard envelope.

This only changes Taiko's concrete RPC surface. The JSON fields for standard consumers remain the
same plus one extra optional field.

### Cache behavior

Retain the existing built-payload difficulty cache and `newPayloadV2` hydration for compatibility
with clients that still omit `headerDifficulty`.

That cache becomes a fallback rather than the primary transport.

## Client Design (`taiko-client-rs`)

### New response wrapper

Introduce a client-side `engine_getPayloadV2` response type with:

- `envelope: ExecutionPayloadEnvelopeV2` via `#[serde(flatten)]`
- `header_difficulty: Option<U256>` from `headerDifficulty`

### RPC client method

Change `engine_get_payload_v2` to deserialize into the new wrapper type instead of directly into
`ExecutionPayloadEnvelopeV2`.

### Submission path

When converting the fetched payload into `TaikoExecutionDataSidecar`, propagate
`header_difficulty` from the wrapper into `TaikoExecutionDataSidecar.header_difficulty`.

This replaces the current behavior that intentionally sets `header_difficulty: None` for
`engine_getPayloadV2` round trips.

### Rollout behavior

If `headerDifficulty` is absent because the server is old, the client keeps `header_difficulty:
None`.

That keeps the client backward compatible during rollout, while the same-node server fallback can
still hydrate the value when available.

## Testing

### `alethia-reth`

Add tests covering:

1. `engine_getPayloadV2` response serialization includes `headerDifficulty`.
2. The value equals the built block header difficulty.
3. The field is omitted or preserved correctly under serde expectations when optional.

### `taiko-client-rs`

Add tests covering:

1. `engine_get_payload_v2` response deserializes successfully when `headerDifficulty` is present.
2. Deserialization still succeeds when `headerDifficulty` is absent.
3. The driver submission path propagates `headerDifficulty` into the Taiko sidecar.

## Risks

- If any consumer uses a strict JSON schema that rejects unknown fields in `engine_getPayloadV2`,
  the extra field could break that consumer. This is unlikely for normal serde-based Rust clients,
  but should be called out.
- During mixed-version rollout, "always pass" is not enforceable until the server side is upgraded.

## Rejected Alternatives

### Reuse `blockValue`

Rejected because `blockValue` has standard Engine API semantics unrelated to header difficulty or
zk gas. Reusing it would create protocol ambiguity and future breakage risk.

### Separate metadata RPC

Rejected because it adds a second round trip and unnecessary coordination between calls that
already identify a single payload.

## Implementation Notes

- Preserve documentation requirements for all new production symbols and methods.
- Keep the current fallback hydration path until all clients are upgraded.
- Prefer narrowly scoped Taiko wrapper types rather than modifying upstream alloy types.
