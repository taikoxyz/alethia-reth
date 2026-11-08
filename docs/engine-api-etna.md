# Etna fork and Engine API driver guide

This guide defines alethia-reth's side of the `Etna` fork: the block rules the execution layer
enforces and the Engine API wire contract that drivers use. Every built-in network leaves Etna
unscheduled (`ForkCondition::Never`), so the Etna rules are a handoff, not an activation notice.
The Engine API method change is not gated on Etna: it applies to Unzen traffic as soon as a node
runs this version (see [Breaking at deploy](#breaking-at-deploy)).

Etna block production and validation follow taiko-mono's
[`SPEC.md` section 3](https://github.com/taikoxyz/taiko-mono/blob/49df560256c3f20f0868f6c6e555443c2b1fd669/packages/protocol/docs/etna/SPEC.md#3-consensus-and-blocks)
and its [anchorless design](https://github.com/taikoxyz/taiko-mono/blob/49df560256c3f20f0868f6c6e555443c2b1fd669/packages/protocol/docs/etna/ANCHORLESS.md).
The older proposal-based `Derivation.md` describes the pre-Etna flow. Activation
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
| `extraData` | Exactly 13 bytes: `[100(1) \| generation(6) \| anchorBlockNumber(6)]` under the Etna production rules. |
| `withdrawalsRoot` | The empty withdrawals root; the body carries no withdrawals. |
| `blobGasUsed`, `excessBlobGas` | `0`. |
| `requestsHash` | `sha256("")`. |
| `difficulty` | The block's zk gas (`0` for an empty block). |

For the two anchor fields, the execution layer checks only that the root is nonzero and that
`extraData` has 13 bytes. It does not fetch L1 data, decode the anchor number, compare anchors with
the parent's, or enforce the first byte's value or the generation. The Etna client (the
taiko-client-rs ABCI application) derives and checks those fields under SPEC.md sections 3.2–3.4,
and prover guests authenticate the anchor against L1 headers. The root is the L1 anchor's execution
state root, not its block hash. `l1Origin` is driver bookkeeping; the Etna client records the
anchor's number and hash there.

Offsets are zero-based and inclusive: `basefeeSharingPctg` is byte 0, `generation` bytes 1..6, and
`anchorBlockNumber` bytes 7..12. SPEC.md CONS-3.2-5 fixes byte 0 at 100; the EL applies byte 0 as
the base-fee share but does not enforce its value. Both `uint48` fields are big-endian and must fit
without truncation, and `anchorBlockNumber` is the final value after validation and inheritance.
Shasta and Unzen headers keep the 7-byte `[basefeeSharingPctg | proposalId]` layout. The L2 genesis
header keeps a zero root, is exempt from the `extraData` and body rules, and performs no EIP-4788
call.

### Gas limits and base fee

For Etna targets, `blockMetadata.gasLimit` and `taikoAuth`'s `blockMaxGasLimit` contain no legacy
1,000,000 gas anchor reserve, and the builder uses the complete supplied limit for ordinary
transactions. EIP-4396 always uses the raw parent header's `gasLimit` and `gasUsed`, including at
the boundary, where the parent still contains the anchor. SPEC.md section 3.2 fixes the block gas
limit to the chain constant and defines the EIP-4396 base fee.

Standalone Etna simulations (`eth_call`, `eth_estimateGas`, `debug_traceCall` and similar calls, and
the replays behind transaction and block traces) charge each transaction's intrinsic zk gas, and
every run or replayed transaction starts from the full block zk-gas budget. `eth_simulateV1` runs
through the block executor instead, with one cumulative budget per simulated block. An over-limit
run answers JSON-RPC `-32603` with the message `Revm error: zk gas limit exceeded`. System calls
have no transaction intrinsic charge, and pre-Etna RPC accounting stays unchanged.

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
only while the unchanged upstream payload store retains it. That store caches only the most recently
resolved successful payload, so once another build succeeds, an earlier ID answers `-38001`.

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

Neither alethia-reth nor taiko-geth builds or imports a pre-Unzen block through these methods. A node whose
head is before Unzen can catch up only through P2P sync or a post-Unzen snapshot; L1 derivation
alone stops at the pre-Unzen rejection.

Drivers keep computing the pre-Etna `l1Origin.buildPayloadArgsId` exactly as before: version byte 2
and no `parentBeaconBlockRoot`, although FCUv3 now carries a zero root. Both drivers compare the
fingerprint with stored origins to detect blocks they already inserted, so a changed fingerprint
would re-insert blocks preconfirmed before the upgrade. An Etna job's fingerprint is the same
preimage with version byte 2 plus its nonzero `parentBeaconBlockRoot`, hashed between the withdrawals
and the transaction-list hash; `extraData` is hashed for every job. After `extraData`, an Etna job
hashes `0x01 ‖ u64_be(maxBlockBytes)` (big-endian, eight bytes), so jobs with different byte
bounds get distinct IDs. Pre-Etna jobs carry no byte bound, so their IDs are unchanged.
alethia-reth stamps every payload ID this way, so its FCUv3 IDs for Unzen jobs keep their V2-era
values. taiko-client-rs computes the fingerprint with alethia-reth's `payload_id_taiko`, so it must
move to this alethia-reth version together with its FCUv3 switch: earlier versions also hash a zero
root, which changes every Unzen fingerprint. taiko-geth's FCUv3 IDs carry version byte 3.

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
| FCUv3 attributes with `targetGasLimit` | JSON-RPC `-32602` |
| Malformed V4 arguments or unsupported payload properties | JSON-RPC `-32602` |
| A nonzero root before Etna | JSON-RPC `-32602` |
| FCUv3 attributes whose `blockMetadata.timestamp` differs from `payloadAttributes.timestamp` | JSON-RPC `-32602` |
| Invalid Etna FCUv3 attributes, such as a zero root or a non-13-byte `extraData` | JSON-RPC `-32602` (current) |
| Etna FCUv3 attributes without `maxBlockBytes` or with `maxBlockBytes` below 1,024 | JSON-RPC `-32602` |
| FCUv3 attributes with `maxBlockBytes` before Etna | JSON-RPC `-32602` |
| An Etna payload with a zero root, withdrawals, blob gas, or nonempty side arrays | JSON-RPC `-32602` (current) |
| An Unzen payload with withdrawals, blob gas, or nonempty side arrays | Payload status `INVALID` |
| Wrong Etna `extraData` length on import | Payload status `INVALID` |
| Block-hash mismatch | Payload status `INVALID` |
| Etna zk-gas exhaustion or zk-gas/difficulty mismatch | Payload status `INVALID` |
| Unzen zk-gas exhaustion or zk-gas/difficulty mismatch | JSON-RPC `-32603` (historical internal-error mapping) |
| An Etna FCUv3 build that fails on a parent whose state is pruned | JSON-RPC `-38100` (`cannot build on parent`; `data` is diagnostic text) |
| Any other failed build, from the FCUv3 that started it | JSON-RPC `-32603` (`missing payload`) |
| `getPayloadV5` for a payload whose build failed | JSON-RPC `-38001` (the FCUv3 already resolved the job) |
| An FCUv3 whose head is a canonical ancestor, including one below the finalized block | `VALID`; the head stays; with attributes, also a build on that ancestor (never `-38006`) |
| A payload transaction that cannot be decoded, has an unsupported type, or whose signer cannot be recovered | Payload status `INVALID` (a given-list build drops such a transaction instead) |

Rows marked "current" are not the intended final contract; they are expected to change before
activation.

`-38100` is definitive: the node cannot build on that parent, so the client must halt rather than
retry. It appears only when an Etna FCUv3 build fails because the parent's state is pruned, and
`getPayloadV5` never answers `-38100`. Every other build failure answers `-32603` without saying
whether it is transient; the deterministic ones, such as a sealed block above `maxBlockBytes`, are
not reachable with valid attributes.

### FCU attributes

Every FCUv3 request with attributes sends `withdrawals: []`, a `parentBeaconBlockRoot` (zero for
Unzen and the nonzero anchor state root for Etna), and a `blockMetadata.timestamp` equal to
`payloadAttributes.timestamp`. For an Etna target, also send no `anchorTransaction` and exactly 13
bytes of `blockMetadata.extraData`. That field keeps its Base64 encoding; do not send hex.

Etna attributes carry one additional top-level member next to `anchorTransaction`:

| Member | Type | Rule |
| --- | --- | --- |
| `maxBlockBytes` | JSON number, like `blockMetadata.gasLimit` | Required on every Etna build, given-list builds included, and at least 1,024. Drivers send 124,000. Bounds the built block's length ([byte limit](#byte-limit)). Rejected before Etna. |

The existing `blockMetadata.txList` selects the build mode: present (including an empty list)
means a given-list build; absent means a pool build.

### Build modes

Every Etna build starts from the parent's state after the block-start EIP-2935 and EIP-4788 calls,
with the block's full gas limit, the full zk-gas budget, and a transaction byte budget derived from
`maxBlockBytes` ([byte limit](#byte-limit)). A transaction that is left out leaves no state,
receipt, or fee behind. An ordinary EVM revert or out-of-gas is included and charged as usual.

- **Given list.** The list runs in order. A transaction is left out for a transaction-level reason,
  such as an unsupported type, a size above the remaining byte budget, a nonce, balance, base-fee,
  or intrinsic-gas failure against the current state, or a gas limit above the block's remaining
  gas. The first transaction that exceeds the zk-gas limit is discarded and ends the list. A
  given-list build of `[tx]` therefore returns either a block holding `tx` or an empty block, never
  an error for a transaction-level reason; only failures unrelated to the transactions, such as a
  state read error, fail the build.
- **Pool.** The builder selects from its own transaction pool by effective tip, within the gas,
  zk-gas, and byte budgets left. As build policy, not a consensus rule, a pool transaction that
  exceeds the zk-gas limit is discarded together with its sender's later nonces and selection
  continues with other senders.

#### Driver-owned forced inclusion

The driver owns the forced-inclusion queue and sends `[FI, ordinary…]` as a given list; the EL
applies the ordinary given-list rules. Only a completed empty build of `[FI]` alone on the same
parent and attributes is skip evidence; an omission from a longer list or an RPC failure is not.

#### Byte limit

`maxBlockBytes` bounds the block's executionBlock, `rlp([header, [tx…]])`, including its outer list
header. Every transaction, legacy included, counts as an RLP byte string holding its EIP-2718
bytes, the form in which the consensus envelope and the published blob carry it. Ethereum's
block-body form keeps a legacy transaction as a bare RLP list, so here a legacy transaction also
counts the RLP string header around that list; a typed transaction counts the same in both forms.
The bound is inclusive. The builder reserves room for the largest header the job can seal, so a
block may stop up to 16 bytes short of the bound; it never exceeds it.

## `taikoAuth` preselection

`taikoAuth_txPoolContent` and `taikoAuth_txPoolContentWithMinTip` keep their existing parameters.
Preselection simulates the next block under the parent's fork rules and `extraData`, and never
applies pre-execution system calls. On an Etna parent it drops the legacy 2,000,000 zk-gas anchor
reserve. At the boundary the parent is still pre-Etna, so the first Etna block is selected with that
reserve. Results are estimates; the builder enforces the actual gas and zk-gas limits. Etna builds
also enforce `maxBlockBytes` ([byte limit](#byte-limit)), which preselection does not apply.

## Devnet activation override

`--devnet-etna-timestamp <TIMESTAMP>` (or `ALETHIA_RETH_DEVNET_ETNA_TIMESTAMP`) schedules Etna at
that timestamp on canonical Taiko devnet or on a custom Genesis JSON whose fields outside `alloc`
match canonical devnet; on any other chain the flag fails startup. `0` activates Etna at genesis; a
genesis built with the [recipe](../README.md#custom-devnet-genesis-with-etna) works only with `0`.
On canonical devnet itself, the pre-Etna `Anchor` stays callable and EIP-4788 and EIP-2935 have no
code. Offline commands such as `stage run` and `re-execute` cannot reproduce an overridden schedule.
See the [custom genesis recipe](../README.md#custom-devnet-genesis-with-etna).