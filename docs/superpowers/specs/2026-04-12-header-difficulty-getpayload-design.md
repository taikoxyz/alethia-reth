# Header Difficulty Via `blockValue` In `engine_getPayloadV2`

## Summary

Reuse Taiko's `engine_getPayloadV2` `blockValue` field for Uzen-and-later payloads. For those
payloads, `blockValue` carries the finalized `header.difficulty` value, which is the finalized
block zk gas committed into the built block header.

The goal is to make Taiko clients always pass the original header difficulty back through
`engine_newPayloadV2`, instead of relying on per-node cache hydration.

This keeps the standard `ExecutionPayloadEnvelopeV2` wire shape unchanged while redefining
`blockValue` semantics for Taiko Uzen payloads.

## Problem

After Uzen, Taiko writes finalized zk gas into `block.header.difficulty`. That field is
hash-relevant for imported payload validation and round-tripping built payloads.

Today:

- The built block header contains the correct difficulty.
- `engine_getPayloadV2` returns the standard `ExecutionPayloadEnvelopeV2` shape, and Taiko
  currently does not use `blockValue`.
- `engine_newPayloadV2` accepts Taiko-specific top-level fields including `headerDifficulty`.
- Local same-node round trips work only because `alethia-reth` caches built header difficulty by
  block hash and hydrates it during `engine_newPayloadV2` when the client omits it.

This means the system does not provide a robust protocol-level guarantee that clients can always
pass the original header difficulty across nodes, restarts, or offline transport.

## Requirements

1. A Taiko client must be able to obtain the original header difficulty from
   `engine_getPayloadV2`.
2. A Taiko client must always pass that value back in `engine_newPayloadV2`.
3. The `engine_getPayloadV2` JSON field layout must remain the standard
   `ExecutionPayloadEnvelopeV2` shape.
4. Existing servers that still return legacy Taiko `blockValue` contents must remain readable by
   updated Taiko clients during rollout.
5. Pre-Uzen behavior must remain unchanged.

## Non-Goals

- Do not introduce a second RPC round trip for payload metadata.
- Do not require upstream Engine API type changes.
- Do not add a new top-level `headerDifficulty` field to `engine_getPayloadV2`.

## Chosen Approach

For Uzen-and-later Taiko payloads, set `engine_getPayloadV2.blockValue` equal to the built block's
`header.difficulty`.

Example response shape:

```json
{
  "executionPayload": { "...": "..." },
  "blockValue": "0x2a"
}
```

No new fields are added. The existing `blockValue` field is repurposed for Taiko Uzen payloads.
For pre-Uzen payloads, keep the existing behavior.

## Compatibility Model

### Old client -> new server

Wire-compatible. However, any client that interprets `blockValue` using standard Ethereum
fee-recipient-value semantics will see Taiko-specific meaning instead for Uzen payloads.

### New client -> old server

Safe during rollout only if the client treats legacy servers as not carrying header difficulty in
`blockValue` and continues to rely on the same-node hydration fallback.

Operationally, "always pass" becomes guaranteed only once the client talks to a server version
that writes Uzen header difficulty into `blockValue`.

### New client -> new server

Desired steady state. The client reads Uzen header difficulty from `blockValue` and always echoes
it into `engine_newPayloadV2.headerDifficulty`.

## Server Design (`alethia-reth`)

### Conversion

Replace the current direct `From<EthBuiltPayload>` path used by Taiko `getPayloadV2` with a
Taiko-specific conversion that:

- builds the normal `ExecutionPayloadEnvelopeV2`
- for Uzen payloads, overwrites `block_value` with `built_payload.block().header().difficulty`
- for pre-Uzen payloads, preserves the current `block_value` behavior

### API surface

`engine_getPayloadV2` keeps returning the standard `ExecutionPayloadEnvelopeV2` type.

This avoids any wire-shape change on the RPC surface. The change is entirely semantic for Taiko
Uzen payloads.

### Cache behavior

Retain the existing built-payload difficulty cache and `newPayloadV2` hydration for compatibility
with clients that still omit `headerDifficulty`.

That cache becomes a fallback rather than the primary transport.

## Client Design (`taiko-client-rs`)

### RPC client method

Keep `engine_get_payload_v2` deserializing into `ExecutionPayloadEnvelopeV2`.

### Submission path

When converting the fetched payload into `TaikoExecutionDataSidecar`, set
`header_difficulty = Some(envelope.block_value)` for Uzen-and-later payloads.

This replaces the current behavior that intentionally sets `header_difficulty: None` for
`engine_getPayloadV2` round trips.

### Rollout behavior

If the server is old and still returns legacy `blockValue` contents, the client must treat that
server as not carrying Uzen header difficulty and keep `header_difficulty: None`.

That keeps the client backward compatible during rollout, while the same-node server fallback can
still hydrate the value when available.

### Uzen detection

The client must only interpret `blockValue` as `headerDifficulty` for Uzen-and-later payloads.
Use the existing Taiko hardfork activation logic keyed by payload timestamp or block context rather
than treating `blockValue` that way unconditionally.

## Testing

### `alethia-reth`

Add tests covering:

1. `engine_getPayloadV2` response serialization keeps the standard envelope shape.
2. For Uzen payloads, `blockValue` equals the built block header difficulty.
3. For pre-Uzen payloads, `blockValue` preserves existing behavior.

### `taiko-client-rs`

Add tests covering:

1. The Uzen submission path maps `blockValue` into `header_difficulty`.
2. The pre-Uzen submission path does not reinterpret `blockValue` as header difficulty.
3. Legacy-server rollout behavior still leaves `header_difficulty` unset when appropriate.

## Risks

- Any consumer that assumes standard Engine API `blockValue` semantics will misinterpret Taiko
  Uzen payloads.
- During mixed-version rollout, "always pass" is not enforceable until the server side is upgraded.

## Rejected Alternatives

### Add `headerDifficulty`

Rejected because it changes the `engine_getPayloadV2` wire shape and introduces a Taiko-specific
top-level field when the existing envelope already has an unused slot in Taiko deployments.

### Separate metadata RPC

Rejected because it adds a second round trip and unnecessary coordination between calls that
already identify a single payload.

## Implementation Notes

- Preserve documentation requirements for all new production symbols and methods.
- Keep the current fallback hydration path until all clients are upgraded.
- Prefer narrowly scoped Taiko wrapper types rather than modifying upstream alloy types.
