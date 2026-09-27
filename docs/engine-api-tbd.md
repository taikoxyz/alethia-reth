# TBD fork and Engine API driver guide

This guide defines the alethia-reth wire contract for the temporary `TBD` fork. `TBD` selects
Osaka execution rules after Unzen. It is a driver and release-coordination handoff, not a network
activation notice: every built-in network still configures TBD as `ForkCondition::Never`.

## Engine API method matrix

Drivers must choose the method family from the target payload timestamp, compared with the TBD
activation timestamp. The current head and the time when a request is sent do not select the
family.

| Operation | Target before TBD | Target at or after TBD |
| --- | --- | --- |
| Start a build | `engine_forkchoiceUpdatedV2` | `engine_forkchoiceUpdatedV3` |
| Read a build | `engine_getPayloadV2` | `engine_getPayloadV5` |
| Import a payload | `engine_newPayloadV2` | `engine_newPayloadV4` |

For FCU requests with payload attributes, route on the attributes' target timestamp. For
`newPayload`, route on the payload timestamp. For `getPayload`, alethia-reth first resolves the
payload ID and routes on that job's own block timestamp. A head change or reorg does not change an
existing job's method family. This is not an additional retention guarantee: an ID is routable
only while the unchanged upstream payload store retains it, and a later resolved job can evict an
older ID.

Both FCU versions accept `null` payload attributes. A null-attributes FCU performs the normal
forkchoice validation and head update without starting a build; it therefore has no target
timestamp to route and returns no payload ID. Non-`VALID` FCU results are returned without
publishing L1-origin or proposal metadata.

TBD block construction uses the nonzero `parentBeaconBlockRoot` from the FCU payload attributes.
The driver must associate that original root with the returned payload ID and use the same root
when importing the resolved payload. Do not derive it from the current head or replace it after a
reorg. Non-genesis TBD blocks require a present, nonzero root. Genesis processing separately uses
the canonical zero beacon root.

### V5 normalization and V4 import

Taiko uses `getPayloadV5.blockValue` for finalized block zk-gas. On the JSON wire,
`blockValue` is a hex Ethereum quantity such as `"0x0"`, while the Taiko
`executionPayload.headerDifficulty` extension is a decimal JSON number such as `0`. Missing or
`null` difficulty is invalid; explicit zero is valid. The payload must also contain its complete
transaction array, including `[]` for an empty block.

Drivers must perform a checked normalization and pass all four positional V4 arguments:

```text
root := the original root associated with this FCU payload ID
zkGas := parse getPayloadV5.blockValue as an unsigned integer
require zkGas fits the existing zk-gas limit and decimal u64 representation
payload := getPayloadV5.executionPayload
payload.headerDifficulty := zkGas as a decimal JSON number, including 0
newPayloadV4(payload, [], root, [])
```

The second argument is the empty expected-blob-versioned-hashes array. The fourth is the empty
execution-requests array. TBD does not accept blob transactions, withdrawals, nonempty execution
requests, Amsterdam block-access-list data, or a slot-number extension.

Generic Ethereum clients expect `blockValue` to represent builder revenue. They cannot assume
this Taiko endpoint has Ethereum `blockValue` semantics: both the Unzen V2 response and TBD V5
response carry hash-relevant zk-gas there. Alethia-reth changes only the response envelope;
internal payload ranking remains based on actual transaction fees.

### FCU metadata encoding

The existing FCU `blockMetadata.extraData` field keeps the
`TaikoBlockMetadata` `serde_with::As<Base64>` representation. Do not send hex in that field. The
new `taikoAuth` target context described below uses ordinary Alloy hex bytes instead. Both fields
carry the same seven-byte Shasta layout, but they intentionally use different JSON encodings.

## Devnet activation override

The optional `--devnet-tbd-timestamp <TIMESTAMP>` flag and
`ALETHIA_RETH_DEVNET_TBD_TIMESTAMP=<TIMESTAMP>` environment variable apply only to the canonical
Taiko devnet chain spec.

- Omitted means no TBD override; the embedded `ForkCondition::Never` remains in effect.
- Explicit `0` is distinct from omission and activates TBD at genesis for a fresh devnet.
- A nonzero value activates TBD at that Unix timestamp.

An enabled TBD fork must use timestamp activation and must be ordered at or after Unzen. Equal
Unzen and TBD timestamps are valid. Startup rejects a missing Unzen activation, a non-timestamp
activation condition, or a TBD timestamp earlier than Unzen. Existing-chain genesis must not be
rewritten to activate this fork.

## `taikoAuth` target context

Both transaction-pool simulation methods append `blockContext` as their final positional
argument. Existing arguments retain their order:

```text
taikoAuth_txPoolContent(
  beneficiary,
  baseFee,
  blockMaxGasLimit,
  maxBytesPerTxList,
  locals,
  maxTransactionsLists,
  blockContext
)

taikoAuth_txPoolContentWithMinTip(
  beneficiary,
  baseFee,
  blockMaxGasLimit,
  maxBytesPerTxList,
  locals,
  maxTransactionsLists,
  minTip,
  blockContext
)
```

The context object is:

```json
{
  "timestamp": "0x64",
  "parentBeaconBlockRoot": "0x0000000000000000000000000000000000000000000000000000000000000001",
  "extraData": "0x00000000000000"
}
```

`timestamp` is an Ethereum hex quantity. The root and `extraData` are ordinary Alloy hex bytes;
the example extra data is exactly seven zero bytes. An explicit context selects rules from its
target timestamp and supplies the root and extra data used by standard pre-execution. Omission
retains the legacy parent-context simulation only while the parent is before TBD. Once the parent
is at or after TBD, `blockContext` is required. Drivers should supply it when simulating a target
that crosses the activation boundary.

## Known malformed-signature response limitation

Invalid transaction signatures are rejected, but the pinned upstream Reth validator currently
wraps signature recovery errors as `BlockExecutionError::other` in
`crates/engine/tree/src/tree/payload_validator.rs:1236`. Consequently, a malformed signature can make
`engine_newPayloadV4` return a JSON-RPC internal error instead of a payload status of `INVALID`.
Drivers must not treat the current response as proof that all malformed transaction bodies use
the `INVALID` status. Local TBD difficulty mismatches and zk-gas exhaustion are classified as
payload validation failures and do return `INVALID`.

## Activation checklist

These release prerequisites remain intentionally unchecked. Do not change any network activation
condition from `ForkCondition::Never` until every item is complete and the activation release is
coordinated.

- [ ] Confirm geth has matching EIP-4788 handling and state-transition results.
- [ ] Deploy and verify the canonical EIP-2935 history-storage and EIP-4788 beacon-roots contracts.
- [ ] Disable privileged Anchor writes before or with the first TBD block.
- [ ] Confirm both drivers implement the six-method routing matrix, checked V5 normalization,
      retained per-job root, and trailing `taikoAuth` target contexts.
- [ ] Confirm the prover guest verifies the L1 execution block hash and preserves legacy-parent
      inheritance across the activation boundary.
- [ ] Confirm checkpoint/reveal consumers operate permissionlessly without Anchor privileges.
- [ ] Successfully consume the Task 8 cross-client vectors in the other client and prover; the
      checked-in vectors prove alethia-reth behavior only.
- [ ] Resolve or explicitly accept the upstream malformed-signature response limitation for all
      driver retry and invalid-chain paths.
- [ ] Keep existing-chain genesis unchanged and schedule activation only through a coordinated
      network upgrade.
