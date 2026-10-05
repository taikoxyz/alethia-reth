# Etna fork and Engine API driver guide

This guide defines alethia-reth's side of the `Etna` fork: the block rules the execution layer
enforces and the Engine API wire contract that drivers use. Every built-in network still
configures Etna as `ForkCondition::Never`, so the Etna rules are a handoff, not an activation
notice. The Engine API method change is not gated on Etna: it applies to Unzen traffic as soon as a
node runs this version (see [Breaking at deploy](#breaking-at-deploy)).

Derivation rules live in taiko-mono's `packages/protocol/docs/Derivation.md`
([taikoxyz/taiko-mono#22189](https://github.com/taikoxyz/taiko-mono/pull/22189), which includes
[#22237](https://github.com/taikoxyz/taiko-mono/pull/22237); checked at `5d35cdd`) and track
[taikoxyz/taiko-mono#22147](https://github.com/taikoxyz/taiko-mono/issues/22147). Activation
prerequisites and open work are listed in the description of
[taikoxyz/alethia-reth#248](https://github.com/taikoxyz/alethia-reth/pull/248).

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
with the parent's. Drivers derive both values, including inherited anchors and the genesis case, as
Derivation.md specifies, and prover guests authenticate them against L1 headers. The root is neither
the L1 anchor block hash nor `l1Origin.l1BlockHash`, which is the L1 block that included the
proposal.

Offsets are zero-based and inclusive: `basefeeSharingPctg` is byte 0, `proposalId` bytes 1..6, and
`anchorBlockNumber` bytes 7..12. Both `uint48` fields are big-endian and must fit without
truncation, and `anchorBlockNumber` is the final value after validation and inheritance. Shasta and
Unzen headers keep the 7-byte `[basefeeSharingPctg | proposalId]` layout. The L2 genesis header keeps
a zero root, is exempt from the `extraData` and body rules, and performs no EIP-4788 call.

Shared consensus enforces these rules, so P2P downloads, backfill, staged sync, and
`engine_newPayloadV4` reject the same blocks. Taiko execution ignores the withdrawals and blob-gas
commitments, so other clients must enforce them at import too.

### Gas limits and base fee

For Etna targets, `blockMetadata.gasLimit` and `taikoAuth`'s `blockMaxGasLimit` contain no legacy
1,000,000 gas anchor reserve, and the builder uses the complete supplied limit for ordinary
transactions. EIP-4396 always uses the raw parent header's `gasLimit` and `gasUsed`, including at
the boundary, where the parent still contains the anchor. Derivation.md defines how drivers derive
target limits from the manifest.

## Engine API methods

alethia-reth serves only the Ethereum Osaka method set, for Unzen and Etna blocks alike:

| Operation | Method |
| --- | --- |
| Start a build, or set the head with `null` attributes | `engine_forkchoiceUpdatedV3` |
| Read a build | `engine_getPayloadV5` |
| Import a payload | `engine_newPayloadV4` |

The V2 methods are not served. Version and fork checks follow the standard Osaka Engine API rules.
Taiko activates Cancun, Prague, and Osaka at Unzen, so the three methods reject a target before
Unzen; every network has already activated Unzen. `getPayloadV5` checks the resolved job's own
timestamp, so a later head change or reorg does not affect an existing job. An ID stays retrievable
only while the unchanged upstream payload store retains it.

A `null`-attribute FCU performs normal forkchoice validation and returns no payload ID. Non-`VALID`
FCU results are returned as statuses without publishing L1-origin or proposal metadata, so drivers
must distinguish payload statuses from JSON-RPC errors; the [error contract](#error-contract) lists
each code. All three methods use the JWT-authenticated endpoint, and the advertised capabilities are
exactly these three.

### Breaking at deploy

Dropping V2 takes effect as soon as a node runs this version, not when Etna activates. Every Engine
call a driver makes moves to the three methods above, including L1 derivation, preconfirmation
import, set-head FCUs, and beacon sync, and Unzen traffic follows the same
[FCU attributes](#fcu-attributes) and [V5 normalization](#v5-normalization-and-v4-import) rules.
Release this version together with both drivers, and upgrade each node's execution client and
driver together. Both drivers' CI runs against `alethia-reth:main`, so their switches must merge in
the same window as this change. taiko-geth's FCUv3, getPayloadV5, and newPayloadV4 handlers accept
Unzen timestamps, but no driver has run Unzen through them yet, so run Unzen round trips against
both execution clients before the release.

Neither execution client builds or imports a pre-Unzen block through these methods. A node whose
head is before Unzen can catch up only through P2P sync or a post-Unzen snapshot; L1 derivation
alone stops at the pre-Unzen rejection. A devnet whose `--devnet-unzen-timestamp` lies in the future
cannot build blocks before that time.

Drivers keep computing the pre-Etna `l1Origin.buildPayloadArgsId` exactly as before: version byte 2
and no `parentBeaconBlockRoot`, although FCUv3 now carries a zero root. Both drivers compare the
fingerprint with stored origins to detect blocks they already inserted, so a changed fingerprint
would re-insert blocks preconfirmed before the upgrade. alethia-reth stamps every payload ID, Etna
jobs included, with version byte 2 over that same preimage and adds `parentBeaconBlockRoot` only
when it is nonzero. Its FCUv3 IDs for Unzen jobs therefore keep their V2-era values, while an Etna
job's ID also covers its nonzero root; `extraData` is hashed for every job. taiko-geth's FCUv3 IDs
carry version byte 3.

### V5 normalization and V4 import

`getPayloadV5.blockValue` carries the block's finalized zk gas as a hex quantity. The V4 payload
carries it as `headerDifficulty`, a decimal JSON number that is required and may be zero. Drivers
must normalize the response and pass all four positional arguments:

```text
root := the root sent with the FCU that returned this payload ID, never one from the current head
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
| A transaction with a malformed signature | Possibly a JSON-RPC internal error instead of `INVALID` (upstream Reth) |

Rows marked "current" are not the intended final contract; they are expected to change before
activation.

### FCU attributes

Every FCUv3 request with attributes sends `withdrawals: []` and a `parentBeaconBlockRoot`: zero for
Unzen and the nonzero anchor state root for Etna. For an Etna target, also send no
`anchorTransaction`, set `blockMetadata.timestamp` equal to `payloadAttributes.timestamp`, and send
exactly 13 bytes of `blockMetadata.extraData`. That field keeps its Base64 encoding; do not send hex.
An explicit transaction list, including an empty one, is derived input; an absent list selects from
the transaction pool.

## `taikoAuth` preselection

`taikoAuth_txPoolContent` and `taikoAuth_txPoolContentWithMinTip` keep their existing parameters.
Preselection simulates the next block under the parent's fork rules and never applies pre-execution
system calls. On an Etna parent it uses the parent's `extraData` (13 zero bytes for an Etna genesis
with empty `extraData`) and drops the legacy 2,000,000 zk-gas anchor reserve. At the boundary the
parent is still pre-Etna, so the first Etna block is selected with that reserve. Results are
estimates; the builder enforces the actual gas and zk-gas limits. A driver that still sends the
removed trailing `blockContext` argument gets no error, because extra positional parameters are
ignored; it then simulates under the parent's rules.

## Devnet activation override

`--devnet-etna-timestamp <TIMESTAMP>` or `ALETHIA_RETH_DEVNET_ETNA_TIMESTAMP=<TIMESTAMP>` applies only
to the canonical Taiko devnet chain spec. Omitted keeps `ForkCondition::Never`, `0` activates Etna at
genesis, and a nonzero value activates it at that timestamp. Startup rejects an Etna timestamp
earlier than the Unzen timestamp; equal timestamps are valid. Do not rewrite an existing chain's
genesis to activate the fork. Offline commands such as `stage run` and `re-execute` cannot reproduce
an overridden schedule.
