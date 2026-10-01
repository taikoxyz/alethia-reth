# Etna fork and Engine API driver guide

This guide defines the alethia-reth wire contract for the `Etna` fork. `Etna` selects
Osaka execution rules after Unzen. It is a driver and release-coordination handoff, not a network
activation notice: every built-in network still configures Etna as `ForkCondition::Never`.

This implements alethia-reth's portion of
[taiko-mono issue 22147](https://github.com/taikoxyz/taiko-mono/issues/22147). The required protocol
meaning of `parentBeaconBlockRoot` is the derived L1 execution block hash. Drivers must derive it
from the anchor block number, and prover guests must authenticate it against L1 headers; these
external integrations remain unconfirmed activation prerequisites. An ordinary EL checks the
header invariant and executes EIP-4788 without independently fetching L1 data. The L1 inclusion
hash in `l1Origin.l1BlockHash` is a different value and must not be substituted for it.

Etna retains Osaka execution, the Unzen zk-gas schedule, fee sharing, EIP-4396 base fees, and strict
post-Shasta timestamp ordering. EIP-2935 and EIP-4788 run even in empty blocks, and their writes
must enter the state root and witness. No transaction position is reserved for an anchor, no
synthetic transaction or receipt is inserted, and golden-touch transactions receive ordinary
balance, fee, and refund treatment. Canonical imports must execute the complete committed body;
derivation filtering is not permission to silently discard transactions from an imported block.

## Engine API method matrix

Drivers must choose the method family from the target payload timestamp, compared with the Etna
activation timestamp. The current head and the time when a request is sent do not select the
family.

| Operation | Target before Etna | Target at or after Etna |
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

Both method families use the existing JWT-authenticated endpoint. The advertised Engine
capabilities are the six methods above. Historical consensus rules and V2 payload IDs are
preserved, but two operational V2 responses have changed: a non-`VALID` FCU result with attributes
now returns that status, and an unknown payload ID returns `-38001`; both previously surfaced as
`-32603`. Drivers must distinguish payload status from JSON-RPC errors.

Etna block construction uses the nonzero `parentBeaconBlockRoot` from the FCU payload attributes.
The driver must associate that original root with the returned payload ID and use the same root
when importing the resolved payload. Do not derive it from the current head or replace it after a
reorg. Non-genesis Etna blocks require a present, nonzero root. Genesis processing separately uses
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

The first argument allows exactly these 18 properties, with standard V3 encodings except for the
decimal `headerDifficulty` extension:

```text
parentHash, feeRecipient, stateRoot, receiptsRoot, logsBloom, prevRandao,
blockNumber, gasLimit, gasUsed, timestamp, extraData, baseFeePerGas,
blockHash, transactions, withdrawals, blobGasUsed, excessBlobGas,
headerDifficulty
```

Construct this object explicitly. A serialized legacy Taiko payload is not a V4 request:
`txHash`, `withdrawalsHash`, `taikoBlock`, `TaikoBlock`, `slotNumber`, and every other additional
property are rejected, including properties set to `null` or zero. Transaction and withdrawals
roots are derived from the actual body; legacy root overrides are not supported on this route.

The second argument is the empty expected-blob-versioned-hashes array. The fourth is the empty
execution-requests array. Etna does not accept blob transactions, withdrawals, nonempty execution
requests, Amsterdam block-access-list data, or a slot-number extension.

Generic Ethereum clients expect `blockValue` to represent builder revenue. They cannot assume
this Taiko endpoint has Ethereum `blockValue` semantics: both the Unzen V2 response and Etna V5
response carry hash-relevant zk-gas there. Alethia-reth changes only the response envelope;
internal payload ranking remains based on actual transaction fees.

The V5 envelope also returns empty `blobsBundle` commitments/proofs/blobs, empty
`executionRequests`, and `shouldOverrideBuilder: false`. Its payload has `withdrawals: []` and
zero `blobGasUsed` and `excessBlobGas`. The original root is absent from this standard envelope;
receivers of externally built blocks must obtain it from the original header or transport.

### Current error contract

| Condition | Response |
| --- | --- |
| Known method used for the wrong target fork | JSON-RPC `-38005` |
| Unknown or evicted payload ID | JSON-RPC `-38001` |
| Malformed/missing V4 arguments, unsupported object properties | JSON-RPC `-32602` |
| Etna body checks: zero root, nonempty withdrawals/hash/request arrays, nonzero blob gas | Currently JSON-RPC `-32602` on `engine_newPayloadV4` |
| Invalid Etna FCUv3 attributes | Currently JSON-RPC `-32602` |
| Block-hash mismatch, executed zk-gas/difficulty mismatch, zk-gas exhaustion | Payload status `INVALID` |

The body and FCU rows describe the current implementation, not the intended final interoperability
contract. Before activation, align block-content failures with `INVALID` (including a blob-hash
mismatch with `latestValidHash: null`) and invalid FCU attributes with `-38003`, while preserving
upstream forkchoice/status precedence and parameter errors for malformed requests. Direct
`reth_newPayload` submissions run conversion checks too; an inconsistent legacy Osaka sidecar
must fail conversion before it can poison the invalid-header cache.

### FCU attributes, gas limits, and metadata

For an Etna target, send no `anchorTransaction`, send `withdrawals: []`, and require
`blockMetadata.timestamp == payloadAttributes.timestamp`. Supply a nonzero root and exactly seven
bytes of `blockMetadata.extraData`. An explicit transaction list, including an empty list, is
derived input; an absent list requests selection from the transaction pool. Null/omitted
withdrawals currently pass normalization but are outside this driver contract; strict rejection
is an activation compatibility item.

For Etna targets, both `blockMetadata.gasLimit` and `taikoAuth`'s `blockMaxGasLimit` exclude the
legacy **1,000,000 gas anchor reserve**. The builder uses the complete supplied limit for ordinary
transactions. Drivers must fork-gate any code that adds the reserve when converting a manifest
limit into a target block limit. Explicit Etna preselection also removes the legacy 2,000,000
zk-gas anchor reserve; the final builder still enforces actual gas and zk-gas limits.

EIP-4396 always uses the real parent header's `gasLimit` and `gasUsed`. At the boundary that parent
can be legacy and include the anchor and its reserve. Do not subtract a reserve from the parent
or reinterpret its gas fields using the child's rules.

The existing FCU `blockMetadata.extraData` field keeps the
`TaikoBlockMetadata` `serde_with::As<Base64>` representation. Do not send hex in that field. The
new `taikoAuth` target context described below uses ordinary Alloy hex bytes instead. Both fields
carry the same seven-byte Shasta layout: one byte of `basefeeSharingPctg`, followed by a six-byte
big-endian proposal ID. They intentionally use different JSON encodings.

## Devnet activation override

The optional `--devnet-etna-timestamp <TIMESTAMP>` flag and
`ALETHIA_RETH_DEVNET_ETNA_TIMESTAMP=<TIMESTAMP>` environment variable apply only to the canonical
Taiko devnet chain spec.

- Omitted means no Etna override; the embedded `ForkCondition::Never` remains in effect.
- Explicit `0` is distinct from omission and activates Etna at genesis for a fresh devnet.
- A nonzero value activates Etna at that Unix timestamp.

An enabled Etna fork must use timestamp activation and must be ordered at or after Unzen. Equal
Unzen and Etna timestamps are valid. Startup rejects a missing Unzen activation, a non-timestamp
activation condition, or an Etna timestamp earlier than Unzen. Existing-chain genesis must not be
rewritten to activate this fork.

The override is a node-start option. Offline commands such as `stage run` and `re-execute` do not
parse the Taiko extension and cannot reproduce an overridden Etna devnet schedule merely from
`--chain devnet`. Offline Etna replay is unsupported until those commands can receive and verify
the actual Taiko fork schedule; a generic genesis JSON is not a verified workaround. The override
is ignored for non-devnet chain specs, matching the existing devnet-only override convention.
Custom Etna schedules must also have Shasta active;
the current startup ordering validator checks Unzen but does not yet enforce Shasta.

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
retains the legacy parent-context simulation only while the parent is before Etna. Once the parent
is at or after Etna, `blockContext` is required. Drivers should supply it when simulating a target
that crosses the activation boundary.

Preselection is an estimate, not a block-validity decision. Explicit context must have a timestamp
later than the selected parent from Shasta onward; earlier forks also allow equality. Etna context
requires a nonzero root and exactly seven extra-data bytes. A pre-Shasta explicit context must fit
its legacy 32-byte extra-data decoder. Invalid
context is a parameter error, with the field-specific reason retained.

## Other local activation work

- Batch lookup's cache-miss fallback still stops on an empty block or a non-anchor transaction at
  index zero. Make that stop condition fork-aware before using `lastBlockIDByBatchID` or
  `lastL1OriginByBatchID` on anchorless history; cache hits are unaffected.
- Agree the Etna payload-ID preimage with both drivers. It currently includes
  `l1Origin.buildPayloadArgsId`, so a driver that computes an ID and then stamps that field cannot
  reproduce the EL ID. The inspected Rust driver logs a mismatch and uses the returned EL ID;
  this is not evidence of an immediate build rejection. V2 IDs must remain unchanged. The internal
  hash domain remains `taiko-tbd-payload-v1`, preserving job IDs across the rename to Etna.
- `eth_simulateV1` requires an explicit nonzero `blockOverrides.beaconRoot` for an Etna target.
  Locally constructed full pending blocks are unavailable without a root. `eth_call` and
  `eth_estimateGas` have their own pending simulation path and are covered separately. Decide and
  test any simulation-only root inheritance policy before activation, including a legacy/genesis
  parent with zero root; never relax the consensus root invariant to supply a wallet default.
- Public `TaikoExecutionDataSidecar` literals need the new `osaka` field when downstream Rust
  dependencies are upgraded. Driver-local preselection request types also need to serialize the
  new optional context. Compile and exercise both drivers against the chosen release.

## Geth port checklist

The following sites were inspected at taiko-geth
[`4e001283f48841068c28fd5335dd784d4ed96bca`](https://github.com/taikoxyz/taiko-geth/tree/4e001283f48841068c28fd5335dd784d4ed96bca).
They identify port work; they do not establish that a later geth release has completed it.

| Path at that revision | Rule to fork-gate and compare |
| --- | --- |
| `core/state_processor.go:115`, `miner/taiko_worker.go:273`, `eth/state_accessor.go:261`, and five `eth/tracers/api.go` sites | Index-zero `MarkAsAnchor` and its balance/fee exemptions must remain legacy-only, including replay and tracing. |
| `miner/taiko_worker.go:225–227` and its transaction loop | Empty lists are valid; index-zero ordinary failures use normal filtering rather than anchor-fatal handling. |
| `miner/taiko_worker.go:300`, `core/state_processor.go:144` | Remove the `i > 0` zk-gas exception for Etna; index zero can exhaust the budget. |
| `consensus/taiko/consensus.go:357–362` | `FinalizeAndAssemble` must stop requiring an anchor at index zero. |
| `consensus/taiko/consensus.go:265`, `miner/worker.go:319–322` | Legacy zero-root checks must become the Etna nonzero-root rule, with matching EIP-4788 execution. |
| Pool/build, preselection, fee distribution, and body validation | Match the full target gas budget, zero anchor zk reserve, nonzero fee shares, empty bodies, blob rejection, and ordinary golden-touch transactions. |

A first unfunded transaction is a useful discriminator: Etna derivation skips it as an ordinary
failure; a port retaining `MarkAsAnchor` could include it fee-exempt. Cross-client tests must check
committed transactions, receipts, state roots, balances, and zk-gas, not merely API acceptance.

## Evidence and remaining replay work

The checked-in corpus is [`etna-cases.json`](../crates/node/tests/fixtures/etna-cases.json), exercised
by [`etna_engine.rs`](../crates/node/tests/etna_engine.rs) and
[`etna_history.rs`](../crates/node/tests/etna_history.rs). The legacy reference capture helper is
[`generate-legacy-reference.py`](../crates/node/tests/fixtures/generate-legacy-reference.py).
Synthetic differential fixtures establish only the cases they execute. A checksum of their
entries is an integrity check, not independent evidence that a reference client was run.

Real-history differential replay remains required for selected Hoodi/mainnet Shasta and Unzen
ranges, including known planted golden-touch transactions. It needs retained pre-state in an
archive database (or authenticated equivalent witness data), exact chain/fork configuration,
block ranges, and transaction hashes. An archive RPC URL alone is not input to `re-execute`.
Compare the stable v1.4.1 baseline with this branch for headers/hashes, receipts, fee balances,
nonces, state roots, and zk-gas. No real-chain replay or other-client agreement is claimed here.

Before cross-client sign-off, export portable fork/genesis profiles and ordered FCU steps for reorg
vectors, and preserve decimal V2 `headerDifficulty`. Include index-zero nonce/signature/
type/gas filtering, blob rejection, zk truncation at index zero and after a prefix, nonzero fee
sharing, and funded/unfunded golden-touch calls to the treasury. Existing Rust tests are not a
substitute for another client and the prover consuming those cases.

## Known malformed-signature response limitation

Invalid transaction signatures are rejected, but the pinned upstream Reth validator currently
wraps signature recovery errors as `BlockExecutionError::other` in
`crates/engine/tree/src/tree/payload_validator.rs:1236`. Consequently, a malformed signature can make
`engine_newPayloadV4` return a JSON-RPC internal error instead of a payload status of `INVALID`.
Drivers must not treat the current response as proof that all malformed transaction bodies use
the `INVALID` status. Local Etna difficulty mismatches and zk-gas exhaustion are classified as
payload validation failures and do return `INVALID`.

## Activation checklist

These release prerequisites remain intentionally unchecked. Do not change any network activation
condition from `ForkCondition::Never` until every item is complete and the activation release is
coordinated.

- [ ] Roll out matching geth build/import EIP-4788 handling and verify state-transition parity
      before deploying the beacon-roots contract on existing networks.
- [ ] Deploy and verify the canonical EIP-2935 history-storage and EIP-4788 beacon-roots contracts.
- [ ] Disable privileged Anchor writes **strictly before the first Etna block**: deploy the
      `anchorV4` timestamp gate and disable the L2 `_authorizedSyncer` path, as required by the
      [accepted issue corrections](https://github.com/taikoxyz/taiko-mono/issues/22147#issuecomment-5756503026).
      Preserve proxy storage and historical state. On devnet/Hoodi, construct a funded forged
      `anchorV4` call in the first anchorless block and verify it reverts. A pool-only golden-touch
      filter can be a separately agreed defense, but permissionless proposers can bypass it and
      it cannot replace the contract cutover.
- [ ] Confirm both drivers implement the six-method routing matrix, checked V5 normalization,
      retained per-job root, and trailing `taikoAuth` target contexts.
- [ ] Confirm the prover guest verifies the L1 execution block hash and preserves legacy-parent
      inheritance across the activation boundary. Forced/default blocks must not depend on a
      permissionless reveal having already occurred.
- [ ] Confirm checkpoint/reveal consumers operate permissionlessly without Anchor privileges.
      Reveals are ordinary transactions, not an EL block-validity condition or a synthetic
      SignalService call. Keep the existing treasury available for fee distribution.
- [ ] Complete the geth port and consume the expanded portable fixture corpus in the other client
      and prover; the checked-in vectors prove alethia-reth behavior only.
- [ ] Resolve the local activation items above, including Engine error classes, strict FCUv3
      withdrawals, payload IDs, batch lookup, simulation policy, and custom fork ordering.
- [ ] Record real-history differential results with reproducible inputs and both source revisions.
- [ ] Resolve or explicitly accept the upstream malformed-signature response limitation for all
      driver retry and invalid-chain paths.
- [ ] Keep existing-chain genesis unchanged and schedule activation only through a coordinated
      network upgrade.
