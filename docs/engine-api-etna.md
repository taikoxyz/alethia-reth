# Etna fork and Engine API driver guide

This guide defines alethia-reth's side of the `Etna` fork: the block rules the execution layer
enforces and the Engine API wire contract that drivers use. Every built-in network still
configures Etna as `ForkCondition::Never`, so the Etna rules are a handoff, not an activation
notice. The Engine API method change is not gated on Etna: it applies to Unzen traffic as soon as a
node runs this version (see [Breaking at deploy](#breaking-at-deploy)).

Derivation rules live in taiko-mono's `packages/protocol/docs/Derivation.md`
([taikoxyz/taiko-mono#22189](https://github.com/taikoxyz/taiko-mono/pull/22189), which includes
[#22237](https://github.com/taikoxyz/taiko-mono/pull/22237); checked at `5d35cdd`) and track
[taikoxyz/taiko-mono#22147](https://github.com/taikoxyz/taiko-mono/issues/22147).

## Block rules

Etna keeps Osaka execution, the Unzen zk-gas schedule, fee sharing, EIP-4396 base fees, and strict
post-Shasta timestamp ordering. It removes the anchor transaction:

- No transaction position is reserved, nothing is prepended, and no synthetic transaction or
  receipt is inserted. A block may be empty.
- Golden-touch transactions receive ordinary balance, fee, and refund treatment.
- EIP-2935 and EIP-4788 run in every block, including empty ones, and their writes enter the state
  root and the execution witness.
- Canonical imports execute the complete committed body. Derivation filtering never allows an
  importer to drop transactions silently.

### Header commitments

| Field | Non-genesis Etna rule |
| --- | --- |
| `parentBeaconBlockRoot` | Nonzero. It is the execution state root of the L1 block at the final `anchorBlockNumber`. |
| `extraData` | Exactly 13 bytes: `[basefeeSharingPctg(1) \| proposalId(6) \| anchorBlockNumber(6)]`. |
| `withdrawalsRoot` | The empty withdrawals root; the body carries no withdrawals. |
| `blobGasUsed`, `excessBlobGas` | `0`. |
| `requestsHash` | `sha256("")`. |
| `difficulty` | The block's zk gas (`0` for an empty block). |

For the two anchor fields, the execution layer checks only that the root is nonzero and that
`extraData` has 13 bytes. It does not fetch L1 data, decode the anchor number, or compare anchors
with the parent's. Drivers derive both values, and prover guests authenticate them against L1
headers. The root is neither the L1 anchor block hash nor `l1Origin.l1BlockHash`, which is the L1
block that included the proposal.

Offsets are zero-based and inclusive: `basefeeSharingPctg` is byte 0, `proposalId` bytes 1..6, and
`anchorBlockNumber` bytes 7..12. Both `uint48` fields are big-endian and must fit without
truncation, and `anchorBlockNumber` is the final value after validation and inheritance. Shasta and
Unzen headers keep the 7-byte `[basefeeSharingPctg | proposalId]` layout. The L2 genesis header keeps
a zero root, is exempt from the `extraData` and body rules, and performs no EIP-4788 call.

Shared consensus enforces these rules, so P2P downloads, backfill, staged sync, and
`engine_newPayloadV4` reject the same blocks. Taiko execution ignores the withdrawals and blob-gas
commitments, so other clients must enforce them at import too.

### Anchor inheritance

Drivers recover a built parent's anchor number by the parent's fork:
`Anchor.getBlockState().anchorBlockNumber` before Etna, bytes 7..12 of `extraData` from Etna, and `0`
for genesis. An inherited anchor, as in forced-inclusion and default blocks, repeats the parent's
number/root pair. Take both from an Etna parent's header. A pre-Etna parent's header root is zero,
so authenticate its anchor number and any saved checkpoint through its L2 state instead, and the L1
header when no checkpoint exists. Genesis inheritance uses number `0` and the state root of L1 block
`0`.

### Gas limits and base fee

For Etna targets, `blockMetadata.gasLimit` and `taikoAuth`'s `blockMaxGasLimit` contain no legacy
1,000,000 gas anchor reserve (drivers add none when converting a manifest limit into an Etna target
limit), and the builder uses the complete supplied limit for ordinary transactions. Derivation's
`parent.metadata.gasLimit` subtracts that reserve only for a non-genesis pre-Etna parent. EIP-4396
always uses the raw parent header's `gasLimit` and `gasUsed`, including at the boundary, where the
parent still contains the anchor.

## Engine API methods

alethia-reth serves only the Ethereum Osaka method set, for Unzen and Etna blocks alike:

| Operation | Method |
| --- | --- |
| Start a build, or set the head with `null` attributes | `engine_forkchoiceUpdatedV3` |
| Read a build | `engine_getPayloadV5` |
| Import a payload | `engine_newPayloadV4` |

The V2 methods are not served; calling one returns JSON-RPC `-32601`. Version and fork checks follow
the standard Osaka Engine API rules. Taiko activates Cancun, Prague, and Osaka at Unzen, so a target
before Unzen returns `-38005`; every network has already activated Unzen. `getPayloadV5` checks the
resolved job's own timestamp, so a later head change or reorg does not affect an existing job. An ID
stays retrievable only while the unchanged upstream payload store retains it.

A `null`-attribute FCU performs normal forkchoice validation and returns no payload ID. Non-`VALID`
FCU results are returned as statuses without publishing L1-origin or proposal metadata, and an
unknown payload ID returns `-38001`, so drivers must distinguish payload statuses from JSON-RPC
errors. All three methods use the JWT-authenticated endpoint, and the advertised capabilities are
exactly these three.

Block construction uses the `parentBeaconBlockRoot` from the FCU attributes: zero for Unzen and the
nonzero anchor state root for Etna. The driver must keep that root with the returned payload ID and
pass the same root when importing the payload, never one derived from the current head.

### Breaking at deploy

Dropping V2 takes effect as soon as a node runs this version, not when Etna activates. Every Engine
call a driver makes moves to the three methods above, including L1 derivation, preconfirmation
import, set-head FCUs, and beacon sync. For Unzen blocks, FCUv3 attributes carry `withdrawals: []`
and a zero `parentBeaconBlockRoot`, builds are read with `getPayloadV5` and normalized as described
below, and imports call `newPayloadV4(payload, [], zeroRoot, [])`. Release this version together
with both drivers, and upgrade each node's execution client and driver together. Both drivers' CI
runs against `alethia-reth:main`, so their switches must merge in the same window as this change.

By inspection taiko-geth needs no change for Unzen: its FCUv3, getPayloadV5, and newPayloadV4
handlers accept Unzen timestamps, and its Taiko build and import paths carry the header difficulty.
No driver has run Unzen through these methods against geth yet, so run Unzen round trips against
both execution clients before the release.

Neither execution client builds or imports a pre-Unzen block through these methods. A node whose
head is before Unzen can catch up only through P2P sync or a post-Unzen snapshot; L1 derivation
alone stops with `-38005`. A devnet whose `--devnet-unzen-timestamp` lies in the future cannot build
blocks before that time.

Drivers keep computing the pre-Etna `l1Origin.buildPayloadArgsId` exactly as before: version byte 2
and no `parentBeaconBlockRoot`, although FCUv3 now carries a zero root. Select the Etna fingerprint
by a nonzero root or the Etna timestamp, never by the presence of a root. Both drivers compare the
fingerprint with stored origins to detect blocks they already inserted, so a changed fingerprint
would re-insert blocks preconfirmed before the upgrade. alethia-reth ignores a zero root in pre-Etna
payload IDs, so its FCUv3 IDs for Unzen jobs keep their V2-era values; taiko-geth's FCUv3 IDs carry
version byte 3.

### V5 normalization and V4 import

`getPayloadV5.blockValue` carries the block's finalized zk gas as a hex quantity. The V4 payload
carries it as `headerDifficulty`, a decimal JSON number that is required and may be zero. Drivers
must normalize the response and pass all four positional arguments:

```text
root := the root sent with the FCU that returned this payload ID (zero for Unzen)
zkGas := parse getPayloadV5.blockValue as an unsigned integer that fits u64
payload := getPayloadV5.executionPayload
payload.headerDifficulty := zkGas as a decimal JSON number, including 0
newPayloadV4(payload, [], root, [])
```

The payload object allows exactly the 17 standard V3 properties plus `headerDifficulty`. Construct it
explicitly: legacy Taiko properties such as `txHash`, `withdrawalsHash`, `taikoBlock`, and
`slotNumber` are rejected, even when `null`. The transaction array is required (`[]` for an empty
block). The expected-blob-hash and execution-request arrays must be empty. Neither Unzen nor Etna
accepts blob transactions, withdrawals, execution requests, block access lists, or slot numbers.

The V5 envelope returns empty `blobsBundle` fields, empty `executionRequests`,
`shouldOverrideBuilder: false`, `withdrawals: []`, and zero blob-gas fields. It does not include the
root, so receivers of externally built blocks must take the root from the original header or
transport. Generic Ethereum clients must not read builder revenue from `blockValue` on this endpoint;
internal payload ranking still uses actual fees.

### Error contract

| Condition | Response |
| --- | --- |
| A V2 method | JSON-RPC `-32601` |
| A target before Unzen | JSON-RPC `-38005` |
| Unknown or evicted payload ID | JSON-RPC `-38001` |
| FCUv3 attributes without `withdrawals` or `parentBeaconBlockRoot`, or with `slotNumber` | JSON-RPC `-38003` |
| Malformed V4 arguments or unsupported payload properties | JSON-RPC `-32602` |
| A nonzero root before Etna | JSON-RPC `-32602` |
| Invalid Etna FCUv3 attributes, such as a zero root or a non-13-byte `extraData` | JSON-RPC `-32602` (current) |
| An Etna payload with a zero root, withdrawals, blob gas, or nonempty side arrays | JSON-RPC `-32602` (current) |
| An Unzen payload with withdrawals, blob gas, or nonempty side arrays | Payload status `INVALID` |
| Wrong Etna `extraData` length on import | Payload status `INVALID` |
| Block-hash mismatch, zk-gas/difficulty mismatch, zk-gas exhaustion | Payload status `INVALID` |

The two "current" rows are not the intended final contract. Before activation, block-content
failures should become `INVALID` (a blob-hash mismatch with `latestValidHash: null`) and invalid FCU
attributes `-38003`.

Direct `reth_newPayload` submissions run the same payload conversion, so an inconsistent legacy
Osaka sidecar fails conversion before it can reach the invalid-header cache.

### FCU attributes

Every FCUv3 request with attributes sends `withdrawals: []` and a `parentBeaconBlockRoot`: zero for
Unzen and the nonzero anchor state root for Etna. Omitting either returns `-38003`. For an Etna
target, also send no `anchorTransaction`, set `blockMetadata.timestamp` equal to
`payloadAttributes.timestamp`, and send exactly 13 bytes of `blockMetadata.extraData`. That field
keeps its Base64 encoding; do not send hex. An explicit transaction list, including an empty one, is
derived input; an absent list selects from the transaction pool.

## `taikoAuth` preselection

`taikoAuth_txPoolContent` and `taikoAuth_txPoolContentWithMinTip` keep their existing parameters.
Preselection simulates the next block under the parent's fork rules and never applies pre-execution
system calls. On an Etna parent it uses the parent's root and `extraData` (13 zero bytes for an Etna
genesis) and drops the legacy 2,000,000 zk-gas anchor reserve. At the boundary the parent is still
pre-Etna, so the first Etna block is selected with that reserve. Results are estimates; the builder
enforces the actual gas and zk-gas limits. A driver that still sends the removed trailing
`blockContext` argument gets no error, because extra positional parameters are ignored; it then
simulates under the parent's rules.

## Devnet activation override

`--devnet-etna-timestamp <TIMESTAMP>` or `ALETHIA_RETH_DEVNET_ETNA_TIMESTAMP=<TIMESTAMP>` applies only
to the canonical Taiko devnet chain spec. Omitted keeps `ForkCondition::Never`, `0` activates Etna at
genesis, and a nonzero value activates it at that timestamp. Startup rejects a missing Unzen
activation, a non-timestamp condition, or an Etna timestamp earlier than Unzen; equal timestamps are
valid. Do not rewrite an existing chain's genesis to activate the fork. Offline commands such as
`stage run` and `re-execute` cannot reproduce an overridden schedule, and the ordering check does not
yet require Shasta.

## Other local activation work

- Batch lookup's cache-miss fallback stops at an empty block or a non-anchor first transaction. Make
  it fork-aware before `lastBlockIDByBatchID` or `lastL1OriginByBatchID` serve anchorless history.
- Agree the Etna payload-ID preimage with both drivers. It includes `l1Origin.buildPayloadArgsId`,
  so a driver that stamps the returned ID into that field cannot reproduce it. Pre-Etna IDs stay
  unchanged, and the hash domain remains `taiko-tbd-payload-v1`.
- `eth_simulateV1` needs an explicit nonzero `blockOverrides.beaconRoot` for an Etna target, and a
  locally built pending Etna block is unavailable. Decide any simulation-only root policy before
  activation without relaxing the consensus rule.
- Downstream Rust code that builds `TaikoExecutionDataSidecar` literals needs the new `osaka` field.
- Require Shasta in custom Etna schedules; the startup ordering check covers only Unzen.
- Let offline commands such as `stage run` and `re-execute` load an overridden Etna schedule.

## Geth port checklist

These sites were inspected at taiko-geth
[`4e001283f48841068c28fd5335dd784d4ed96bca`](https://github.com/taikoxyz/taiko-geth/tree/4e001283f48841068c28fd5335dd784d4ed96bca).
They identify port work, not completed work.

| Path at that revision | Rule to fork-gate and compare |
| --- | --- |
| `core/state_processor.go:115`, `miner/taiko_worker.go:273`, `eth/state_accessor.go:261`, five `eth/tracers/api.go` sites | Index-zero `MarkAsAnchor` and its fee exemptions stay legacy-only, including replay and tracing. |
| `miner/taiko_worker.go:225–227` and its transaction loop | Empty lists are valid; an index-zero failure is filtered like any other. |
| `miner/taiko_worker.go:300`, `core/state_processor.go:144` | Remove the `i > 0` zk-gas exception; index zero can exhaust the budget. |
| `consensus/taiko/consensus.go:357–362` | `FinalizeAndAssemble` stops requiring an anchor at index zero. |
| `consensus/taiko/consensus.go:265`, `miner/worker.go:319–322` | The zero-root rule becomes the Etna nonzero-root rule, with matching EIP-4788 execution. |
| `consensus/taiko/consensus.go:206–212` | The 7-byte Shasta `extraData` rule becomes 13 bytes for non-genesis Etna headers. |
| Pool/build, preselection, fee distribution, body validation | Full gas budget, no anchor zk reserve once the parent is Etna, fee shares, empty bodies, blob rejection, ordinary golden-touch transactions. |

An unfunded first transaction is a useful discriminator: Etna skips it as an ordinary failure, while
a port that still applies `MarkAsAnchor` could include it fee-exempt.

## Evidence

`crates/node/tests/etna_engine.rs` runs live two-node build/import round trips, a cross-fork reorg,
malicious commitments, legacy sidecar rejection, debug tracing, and the genesis case. Unit tests pin
each block rule at its authoritative layer. Cross-client vectors are not checked in; produce them
once the geth and prover ports start, from the then-current spec.

Real-history differential replay is still required for selected Hoodi and mainnet Shasta and Unzen
ranges, including planted golden-touch transactions. It needs retained archive pre-state, the exact
chain configuration, block ranges, and transaction hashes, and should compare v1.4.1 with this
branch for headers, receipts, balances, nonces, state roots, and zk gas.

The pinned upstream Reth validator wraps signature-recovery errors as internal errors
(`crates/engine/tree/src/tree/payload_validator.rs:1236`), so a malformed signature can make
`engine_newPayloadV4` return a JSON-RPC internal error instead of `INVALID`.

## Activation checklist

Keep every network at `ForkCondition::Never` until each item is complete.

- [ ] Roll out the taiko-geth EIP-4788 sealing fix (taikoxyz/taiko-geth#601) to every geth node
      before deploying the beacon-roots contract on an existing network.
- [ ] Deploy and verify the canonical EIP-2935 and EIP-4788 contracts.
- [ ] Disable privileged Anchor writes strictly before the first Etna block: deploy the `anchorV4`
      timestamp gate (taikoxyz/taiko-mono#22222) and verify on devnet or Hoodi that a funded forged
      `anchorV4` call in the first anchorless block reverts.
- [ ] Both drivers implement the Etna parts of the contract: the nonzero per-job root, the 13-byte
      `extraData`, and the parent anchor-number rule.
- [ ] Prover guests verify the root as the state root of the L1 block named by the `extraData`
      anchor number, authenticated against L1 headers, plus the inheritance and genesis rules.
- [ ] Bridge proofs after Etna verify against the anchor state root that EIP-4788 records for an L2
      timestamp (taikoxyz/taiko-mono#22222). Relayers and UIs must build proofs with that timestamp,
      refresh expired ones, and work without Anchor privileges.
- [ ] Complete the geth port and run shared cross-client and prover vectors.
- [ ] Resolve the local activation items above and the Engine error classes.
- [ ] Record real-history differential results with reproducible inputs and both source revisions.
- [ ] Resolve or accept the malformed-signature response limitation for driver retry paths.
- [ ] Schedule activation only through a coordinated network upgrade.
