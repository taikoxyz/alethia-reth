# Etna fork and Engine API driver guide

This guide defines alethia-reth's side of the `Etna` fork: the block rules the execution layer
enforces and the Engine API wire contract that drivers use. Every built-in network leaves Etna
unscheduled (`ForkCondition::Never`), so the Etna rules are a handoff, not an activation notice.
The Engine API method change is not gated on Etna: it applies to Unzen traffic as soon as a node
runs this version (see [Breaking at deploy](#breaking-at-deploy)).

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
`engine_newPayloadV4` reject the same blocks. alethia-reth's execution ignores the withdrawals and
blob-gas commitments, while taiko-geth's Taiko `Finalize` credits `body.Withdrawals` and its header
check requires only a non-nil `withdrawalsHash`, so every client must reject these commitments at
import.

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
alone stops at the pre-Unzen rejection. The `--devnet-unzen-timestamp` override is removed, so the
devnet always activates Unzen at genesis.

Drivers keep computing the pre-Etna `l1Origin.buildPayloadArgsId` exactly as before: version byte 2
and no `parentBeaconBlockRoot`, although FCUv3 now carries a zero root. Both drivers compare the
fingerprint with stored origins to detect blocks they already inserted, so a changed fingerprint
would re-insert blocks preconfirmed before the upgrade. An Etna job's fingerprint is the same
preimage with version byte 2 plus its nonzero `parentBeaconBlockRoot`, hashed between the withdrawals
and the transaction-list hash; `extraData` is hashed for every job. After `extraData`, an Etna job
hashes `0x01 ‖ u64_be(maxBlockBytes)` (big-endian, eight bytes) and then, only when present,
`0x02 ‖ keccak256(forcedTransaction)` over the raw transaction bytes, so mixed and pool jobs that
differ only in these fields get distinct IDs. Pre-Etna jobs carry neither field, so their IDs are
unchanged. alethia-reth stamps every payload ID this way, so its FCUv3 IDs for Unzen jobs keep their
V2-era values. taiko-client-rs computes the fingerprint with alethia-reth's `payload_id_taiko`, so it
must move to this alethia-reth version together with its FCUv3 switch: earlier versions also hash a
zero root, which changes every Unzen fingerprint. taiko-geth's FCUv3 IDs carry version byte 3.

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
| Etna FCUv3 attributes with both `forcedTransaction` and `blockMetadata.txList` | JSON-RPC `-32602` |
| A `forcedTransaction` that is not exactly one EIP-2718 transaction or whose signer cannot be recovered | JSON-RPC `-32602` |
| FCUv3 attributes with `forcedTransaction` or `maxBlockBytes` before Etna | JSON-RPC `-32602` |
| An Etna payload with a zero root, withdrawals, blob gas, or nonempty side arrays | JSON-RPC `-32602` (current) |
| An Unzen payload with withdrawals, blob gas, or nonempty side arrays | Payload status `INVALID` |
| Wrong Etna `extraData` length on import | Payload status `INVALID` |
| Block-hash mismatch | Payload status `INVALID` |
| Etna zk-gas exhaustion or zk-gas/difficulty mismatch | Payload status `INVALID` |
| Unzen zk-gas exhaustion or zk-gas/difficulty mismatch | JSON-RPC `-32603` (historical internal-error mapping) |
| An Etna FCUv3 build that fails on a parent whose state is pruned | JSON-RPC `-38100` (`cannot build on parent`; `data` is diagnostic text) |
| Any other failed build, from the FCUv3 that started it | JSON-RPC `-32603` (`missing payload`) |
| `getPayloadV5` for a payload whose build failed | JSON-RPC `-38001` (the FCUv3 already resolved the job) |
| A transaction with a malformed signature | Possibly a JSON-RPC internal error instead of `INVALID` (upstream Reth) |

Rows marked "current" are not the intended final contract; they are expected to change before
activation.

`-38100` is definitive: the node cannot build on that parent and the client must halt rather than
retry, as it does for `-38006` (an FCU head below the finalized block, answered before any build
starts). Reth reports every failed build as `missing payload`, so after an Etna FCUv3 build fails
the node reads the parent's state once more and answers `-38100` only when that read reports the
state pruned: `StateAtBlockPruned`, or `InsufficientChangesets` for a block below the history prune
boundary. Every other failure keeps its answer and is transient: timeouts, `SYNCING`, unknown
blocks, a missing Finish checkpoint, and any other build error. `getPayloadV5` never answers
`-38100`.

### FCU attributes

Every FCUv3 request with attributes sends `withdrawals: []`, a `parentBeaconBlockRoot` (zero for
Unzen and the nonzero anchor state root for Etna), and a `blockMetadata.timestamp` equal to
`payloadAttributes.timestamp`. For an Etna target, also send no `anchorTransaction` and exactly 13
bytes of `blockMetadata.extraData`. That field keeps its Base64 encoding; do not send hex.
An explicit transaction list, including an empty one, is derived input; an absent list selects from
the transaction pool.

Etna attributes carry two more top-level members next to `anchorTransaction`. Omit a member to
leave it absent; sending either one for a pre-Etna target is rejected:

| Member | Type | Rule |
| --- | --- | --- |
| `maxBlockBytes` | JSON number, like `blockMetadata.gasLimit` | Required on every Etna build, given-list builds included, and at least 1,024. Drivers send 124,000. The built block's `rlp([header, [tx…]])`, including its outer list header and with transactions in block-body form, is at most this many bytes. |
| `forcedTransaction` | `0x`-hex | The raw EIP-2718 bytes whose keccak256 is the forced-inclusion entry's `txHash`, not the RLP-wrapped network form. They must decode as exactly one transaction, with no trailing bytes, whose signer recovers. |

The Etna build mode follows from presence:

| `blockMetadata.txList` | `forcedTransaction` | Build |
| --- | --- | --- |
| present | absent | Given list: the list is derived input |
| absent | present | Mixed: the forced transaction is executed first, then the pool fills the block |
| absent | absent | Pool |
| present | present | Rejected with `-32602` |

[Build modes](#build-modes) describes how each mode fills the block.

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
  continues with other senders. The builder remembers such transactions and bounds the work spent on
  overflowing trials: once the gas limits of remembered transactions that overflow again add up to
  the pool's gas budget, it skips remembered transactions, and once those of first-time overflows
  do, it stops selecting.
- **Mixed.** The forced transaction runs first, at position 0, on the parent's state with the full
  budgets, exactly as the given-list build of `[forcedTransaction]` with the same attributes runs
  it. Inclusion means transaction 0 only. The forced transaction is transaction 0 exactly when that
  build would include it; pool selection then continues from the state after it with the gas and
  bytes it left, and a copy of it in the pool is not included again, though its sender's next nonce
  can follow. Otherwise it is left out of position 0 and the pool fills the block with the full
  budgets; the given-list build of the forced transaction alone on the same parent and attributes is
  then empty as well, which is the execution-layer condition for skipping the entry. A copy of a
  left-out forced transaction that the pool also holds may still be selected later in the block, for
  example after an earlier pool transaction fills its sender's nonce gap or funds its sender; that
  copy is an ordinary pool transaction, not an inclusion. The pool's remembered zk-gas overflows
  never apply to the forced transaction. Drivers detect inclusion by comparing keccak256 of
  transaction 0's EIP-2718 encoding with the entry's `txHash`.

#### Byte limit

`maxBlockBytes` bounds the block's executionBlock, `rlp([header, [tx…]])`, including its outer list
header and with transactions in block-body form: a typed transaction counts as its EIP-2718 bytes
plus their RLP string header. The bound is inclusive. The header is sealed only after execution, so
the builder reserves room for the longest header the job can seal: the job's own header fields,
fixed-size roots and bloom, `gasUsed` equal to `gasLimit`, and a `difficulty` of `u64::MAX`. What
remains after that reserve and the list headers is the transaction byte budget, which the forced
transaction and the selected transactions share. In every mode a transaction larger than what is
left is left out and the build moves on. Before returning a block, the builder checks its exact
executionBlock length and fails the job rather than return a block above `maxBlockBytes`.

## `taikoAuth` preselection

`taikoAuth_txPoolContent` and `taikoAuth_txPoolContentWithMinTip` keep their existing parameters.
Preselection simulates the next block under the parent's fork rules and `extraData`, and never
applies pre-execution system calls. On an Etna parent it drops the legacy 2,000,000 zk-gas anchor
reserve. At the boundary the parent is still pre-Etna, so the first Etna block is selected with that
reserve. Results are estimates; the builder enforces the actual gas and zk-gas limits. Drivers' Etna
branches that send a trailing `blockContext` argument get no error from alethia-reth, which ignores
extra positional parameters and simulates under the parent's rules. taiko-geth rejects the extra
argument, so drivers must stop sending it.

## Devnet activation override

`--devnet-etna-timestamp <TIMESTAMP>` or `ALETHIA_RETH_DEVNET_ETNA_TIMESTAMP=<TIMESTAMP>` applies only
to canonical Taiko devnet and custom Genesis JSON whose parsed fields outside `alloc` all match
canonical devnet, including the complete chain config and its extra fields. It restores canonical
devnet's Taiko/Ethereum fork table (Unzen/Osaka at genesis), rebuilds the genesis header from the
custom allocation, and schedules Etna at the requested timestamp. `0` activates Etna at genesis;
a nonzero value activates it at that timestamp. Other chains log a warning and keep their schedule.

Omitting the flag leaves built-in devnet Etna unscheduled. Custom JSON without the flag uses the
generic Ethereum parser and does not gain Taiko forks implicitly. See the
[custom genesis recipe](../README.md#custom-devnet-genesis-with-etna) for predeploys. Use a fresh data
directory when changing genesis. Offline commands such as `stage run` and `re-execute` cannot
reproduce an overridden schedule.

No built-in genesis, the devnet's included, predeploys the EIP-2935 or EIP-4788 contracts. On a
devnet with the Etna override, both system calls are no-ops until the keyless deployments run or
an alloc-only custom genesis predeploys the canonical runtimes. Storage writes start in block 1.
