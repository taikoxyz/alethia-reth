# Etna fork and Engine API driver guide

This guide defines alethia-reth's side of the `Etna` fork: the block rules the execution layer
enforces and the Engine API wire contract that drivers use. Every built-in network leaves Etna
unscheduled (`ForkCondition::Never`), so the Etna rules are a handoff, not an activation notice.
The Engine API method change is not gated on Etna: it applies to Unzen traffic as soon as a node
runs this version (see [Breaking at deploy](#breaking-at-deploy)).

Etna block production and validation follow taiko-mono's
[`SPEC.md` section 3](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/SPEC.md#3-consensus-and-blocks),
its [section 4](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/SPEC.md#4-staking-and-committees)
on the staking contract that every Etna block calls, its
[section 11](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/SPEC.md#11-parameters)
on parameters, its
[appendix A](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/SPEC.md#appendix-a-encodings)
and its [anchorless design](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/ANCHORLESS.md),
as of `5cd86f0e6623ecb9fbbe3f06df099408325d087f`, the head of docs PR
[taikoxyz/taiko-mono#22425](https://github.com/taikoxyz/taiko-mono/pull/22425), which is not merged
yet.
The older proposal-based `Derivation.md` describes the pre-Etna flow. Activation
prerequisites and open work are listed in the description of
[taikoxyz/alethia-reth#248](https://github.com/taikoxyz/alethia-reth/pull/248).

## Block rules

Etna keeps Osaka execution, the Unzen zk-gas schedule, fee sharing, EIP-4396 base fees (with a
4-second block-time target), and strict post-Shasta timestamp ordering. It removes the anchor
transaction and adds one block-start call:

- No transaction position is reserved, nothing is prepended, and no synthetic transaction or
  receipt is inserted. A block may be empty.
- Golden-touch transactions receive ordinary balance, fee, and refund treatment.
- EIP-2935 and EIP-4788 run in every block, including empty ones, and their writes enter the state
  root and the execution witness.
- After those two writes and before its first transaction, every non-genesis Etna block calls the
  chain's `EtnaStaking` proxy once, with arguments from the block's own `extraData`
  ([block-start staking call](#block-start-staking-call)). A failed call leaves no state behind and
  never makes the block invalid.
- Canonical imports execute the complete committed body. Derivation filtering never allows an
  importer to drop transactions silently.

### Header commitments

| Field                          | Non-genesis Etna rule                                                                                                    |
| ------------------------------ | ------------------------------------------------------------------------------------------------------------------------ |
| `parentBeaconBlockRoot`        | Nonzero. It is the execution state root of the L1 block at the final `anchorBlockNumber`.                                |
| `extraData`                    | Exactly 29 bytes: `[100(1) \| generation(6) \| anchorBlockNumber(6) \| missBitmap(16)]` under the Etna production rules. |
| `withdrawalsRoot`              | The empty withdrawals root; the body carries no withdrawals.                                                             |
| `blobGasUsed`, `excessBlobGas` | `0`.                                                                                                                     |
| `requestsHash`                 | `sha256("")`.                                                                                                            |
| `difficulty`                   | The block's zk gas (`0` for an empty block).                                                                             |

Of the root and `extraData`, the execution layer checks only that the root is nonzero, that
`extraData` has 29 bytes and that its byte 0 is 100 (below). It decodes `generation` and
`missBitmap` only to build the [block-start call](#block-start-staking-call)'s calldata and never
validates their values. It does not fetch L1 data, decode the anchor number, compare anchors with
the parent's, check the generation, or recompute `missBitmap`. The Etna client (the taiko-client-rs
ABCI application) computes `missBitmap` and derives and checks every field under SPEC.md sections
3.2–3.4, and prover guests authenticate the anchor against L1 headers. The root is the L1 anchor's
execution state root, not its block hash. `l1Origin` is driver bookkeeping; the Etna client records
the anchor's number and hash there.

Offsets are zero-based and inclusive: `basefeeSharingPctg` is byte 0, `generation` bytes 1 through
6, `anchorBlockNumber` bytes 7 through 12, and `missBitmap` bytes 13 through 28 (the Rust range
`13..29`). SPEC.md section 3.2 fixes byte 0 at 100, so the beneficiary receives every base fee; the
EL applies byte 0 as the base-fee share and rejects any other value in non-genesis Etna headers and
in Etna FCUv3 attributes. Both `uint48` fields are big-endian and must fit without truncation, and
`anchorBlockNumber` is the final value after validation and inheritance. `missBitmap` is a
big-endian `uint128` in which seat s is bit s: seat 0 is the low bit of byte 28, the last byte, and
seat 127 the high bit of byte 13. Shasta and Unzen headers keep the 7-byte
`[basefeeSharingPctg | proposalId]` layout. The L2 genesis header keeps a zero root, is exempt from
the `extraData` and body rules, and performs no EIP-4788 call and no block-start staking call.

### Block-start staking call

In every non-genesis Etna block, the EL calls `onBlockStart(uint48 generation, uint128 missBitmap)`
on the chain's `EtnaStaking` proxy once, after the EIP-2935 and then the EIP-4788 write and before
the first transaction. The call counts the missed signatures that `missBitmap` reports, applies
the penalties they reach, releases the seats of bonds that have missed too many blocks in a row,
and stores the candidate snapshot
([SPEC.md section 3.2](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/SPEC.md#32-block-header)
and [section 4](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/SPEC.md#4-staking-and-committees)).

- **Caller and calldata.** The caller is the system address
  `0xfffffffffffffffffffffffffffffffffffffffe`. The calldata is 68 bytes: the selector `0xea987592`,
  then `generation` and `missBitmap` from the block's own `extraData`, each as one 32-byte
  big-endian ABI word with zero high bits.
- **Gas and fees.** The call's frame gets exactly 30,000,000 gas. No intrinsic gas is charged, the
  EIP-7825 transaction gas cap does not apply, no fee is paid, and the caller's nonce and balance
  never change.
- **Starting state.** The call starts with empty transient storage (EIP-1153). Warm at its start are
  the caller, the staking address, every active precompile and the block's coinbase: the set that
  EIP-2929 and EIP-3651 give a transaction from the caller to the proxy. The EIP-2935 and EIP-4788
  contracts are cold unless one of those rules warms them, and what the call accesses is cold again
  for the block's first transaction.
- **Outcome.** The call fails exactly when it reverts or halts exceptionally, running out of gas
  included. A failed call's state is discarded and the block stays valid. A successful call's state
  is committed before the first transaction, and a transaction the build leaves out does not roll
  it back. A call to an address without code succeeds and runs no code. Like any message call, it
  touches that address, and SPEC.md's CONS-3.2-13 requires what follows: an empty account there
  (zero nonce and balance, no code), which only a genesis can hold, is removed with its storage
  (EIP-161), and the call changes no other state
  ([vectors 3.2 13.n and 13.o](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/rules/vectors/3.2.md#cons-32-13)).
- **Node faults.** A fault of the node's own storage or environment during the call, such as a
  failed database read, is not a failure of the call. Execution stops with an internal error, and
  the node answers neither `VALID` nor `INVALID` on that basis ([error contract](#error-contract)).
- **Accounting.** Like the two system writes, the call is outside the block's `gasUsed`, its zk gas
  (`difficulty`), its receipts and its `logsBloom`, so its logs reach no receipt. `debug_trace*`
  results do not show it. Its reads, a failed call's included, enter the execution witness.
- **Once per block.** Every build, import, prover, witness and trace path makes the call in the
  block executor's pre-execution step, which makes it once even when that step runs twice.
  Pre-Etna blocks and the L2 genesis make no call.

[SPEC.md section 11](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/SPEC.md#11-parameters)
requires the proxy address to be built in for the L2 chain id and to equal the `staking` address
that the config hash commits. The EL reads it from neither L1 nor the genesis. The spec gives no
formula: the `getConstantAddress` rule below is this version's, and follows the convention
taiko-mono's genesis uses for its predeploy addresses.

| Chain id                                         | `EtnaStaking` proxy address                                                                                                    |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------ |
| 167001 (canonical devnet)                        | `0x1670010000000000000000000000000000010003`                                                                                   |
| 167000 (mainnet) and 167013 (Taiko Hoodi)        | None, so a node whose chain schedules Etna refuses to start                                                                    |
| Every other chain id, custom chainspecs included | `getConstantAddress(chainId, 10003)`: the chain id's decimal digits, then zero padding, then `10003`, 40 hex characters in all |

A new network's genesis predeploys the proxy at its address; while the address holds no code, every
call is a no-op. `alethia-reth node` refuses to start when its final chain spec, after the
[devnet override](#devnet-activation-override), schedules Etna (any activation other than
`ForkCondition::Never`) on a chain without an address. Mainnet and Taiko Hoodi have none yet; the
release that schedules Etna on either builds that chain's address in. Drivers read the address with
[`taikoAuth_etnaStakingAddress`](#staking-address).

This version makes the call and requires the 29-byte layout in every Etna block, which is ruleset 0
of
[SPEC.md section 10.1](https://github.com/taikoxyz/taiko-mono/blob/5cd86f0e6623ecb9fbbe3f06df099408325d087f/packages/protocol/docs/etna/SPEC.md#101-protocol-upgrades).
The EL does not read the rules schedule, so a later ruleset that changes the call or the layout
needs its own way for the EL to select its blocks.

### Gas limits and base fee

For Etna targets, `blockMetadata.gasLimit` and `taikoAuth`'s `blockMaxGasLimit` contain no legacy
1,000,000 gas anchor reserve, and the builder uses the complete supplied limit for ordinary
transactions. EIP-4396 always uses the raw parent header's `gasLimit` and `gasUsed`, including at
the boundary, where the parent still contains the anchor. SPEC.md section 3.2 fixes the block gas
limit to the chain constant `L2_BLOCK_GAS_LIMIT` (45,000,000, SPEC.md section 11); the EL builds with
`blockMetadata.gasLimit` and does not check it against that constant.

The EL builds with the attributes' `baseFeePerGas` as given and checks the EIP-4396 value when it
imports the block. The block-time target is that of the block itself, not of its parent: 4 seconds
for every Etna block, including the first, whose parent and grandparent are Unzen blocks, and
2 seconds for Shasta and Unzen blocks. The EIP-4396 parameters are listed under
[pinned parameters](#pinned-parameters).

### Pinned parameters

SPEC.md section 3.1 leaves the zk-gas values to the pinned execution layer, and Etna uses them
unchanged from Unzen. SPEC.md appendix A writes out the EIP-4396 formula and values; of these, Etna
changes only the block-time target, from 2 to 4 seconds. The block-start call's values come from
SPEC.md section 3.2 and appendix A. Section 11 only requires a built-in address per chain id; the
address rule is this version's ([address table](#block-start-staking-call)).

| Parameter                                | Value                                                                                                                                                                                                                                                                          |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| zk gas rules                             | taiko-mono [`zk_gas_spec.md` at `f47f1f3bd`](https://github.com/taikoxyz/taiko-mono/blob/f47f1f3bd692a462dd623ff2781a277637975b1a/packages/protocol/docs/zk_gas_spec.md), checked in CI against that revision's shared vectors ([`zk-gas-vectors.ref`](../zk-gas-vectors.ref)) |
| Block zk-gas limit                       | 100,000,000 (the spec's placeholder)                                                                                                                                                                                                                                           |
| Intrinsic zk gas per transaction         | 243,000                                                                                                                                                                                                                                                                        |
| Per-transaction gas cap                  | 16,777,216 (EIP-7825, active from Unzen)                                                                                                                                                                                                                                       |
| EIP-4396 block-time target               | 4 seconds for Etna blocks, the first included; 2 seconds before Etna                                                                                                                                                                                                           |
| EIP-4396 elasticity multiplier           | 2                                                                                                                                                                                                                                                                              |
| EIP-4396 base-fee max change denominator | 8                                                                                                                                                                                                                                                                              |
| EIP-4396 maximum adjusted gas target     | 95% of the gas limit                                                                                                                                                                                                                                                           |
| Base fee of block 1                      | 25,000,000 wei                                                                                                                                                                                                                                                                 |
| Base-fee bounds                          | 5,000,000 wei (10,000,000 wei on mainnet) to 1,000,000,000 wei                                                                                                                                                                                                                 |
| Block-start call frame gas               | 30,000,000, with no intrinsic gas, EIP-7825 cap or fee                                                                                                                                                                                                                         |
| Block-start call caller                  | `0xfffffffffffffffffffffffffffffffffffffffe`                                                                                                                                                                                                                                   |
| Block-start call selector and calldata   | `0xea987592` (`onBlockStart(uint48,uint128)`), then `generation` and `missBitmap` as two 32-byte words: 68 bytes                                                                                                                                                               |
| `EtnaStaking` proxy address              | `getConstantAddress(chainId, 10003)`, none for mainnet and Taiko Hoodi ([address table](#block-start-staking-call))                                                                                                                                                            |

The opcode and precompile multipliers, spawn estimates, intrinsic charge and block limit at
`f47f1f3bd` equal those of the revision SPEC.md section 3.1 pins, `ce31a49`. The later text also
settles that a spawn opcode that hands its call or create to frame dispatch is charged its spawn
estimate even when the dispatch then fails (call depth, balance, nonce or address collision);
alethia-reth meters it that way.

Standalone Etna simulations (`eth_call`, `eth_estimateGas`, `debug_traceCall` and similar calls, and
the replays behind transaction and block traces) charge each transaction's intrinsic zk gas, and
every run or replayed transaction starts from the full block zk-gas budget. `eth_simulateV1` runs
through the block executor instead, with one cumulative budget per simulated block. An over-limit
run answers JSON-RPC `-32603` with the message `Revm error: zk gas limit exceeded`. System calls
have no transaction intrinsic charge, and pre-Etna RPC accounting stays unchanged.

## Engine API methods

alethia-reth serves only the Ethereum Osaka method set, for Unzen and Etna blocks alike:

| Operation                                             | Method                       |
| ----------------------------------------------------- | ---------------------------- |
| Start a build, or set the head with `null` attributes | `engine_forkchoiceUpdatedV3` |
| Read a build                                          | `engine_getPayloadV5`        |
| Import a payload                                      | `engine_newPayloadV4`        |

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
and the transaction-list hash; `extraData` is hashed for every job, so an Etna job's `missBitmap` is
part of its fingerprint, and Etna payload IDs differ from those of the earlier 13-byte layout
(`payload_id_pins_every_preimage_field` pins one). After `extraData`, an Etna job
hashes `0x01 ‖ u64_be(maxBlockBytes)` (big-endian, eight bytes), so jobs with different byte
bounds get distinct IDs. Pre-Etna jobs carry no byte bound, so their IDs are unchanged.
alethia-reth stamps every payload ID this way, so its FCUv3 IDs for Unzen jobs keep their V2-era
values. taiko-client-rs computes the fingerprint with alethia-reth's `payload_id_taiko`, so it must
move to this alethia-reth version together with its FCUv3 switch: earlier versions also hash a zero
root, which changes every Unzen fingerprint. taiko-geth's FCUv3 IDs carry version byte 3.

On a chain that schedules Etna, this version also rejects the 13-byte Etna `extraData` of earlier
versions, in headers and in FCUv3 attributes alike, and refuses to start without a built-in
`EtnaStaking` address ([block-start staking call](#block-start-staking-call)), so Etna drivers move
to the 29-byte layout together with it.

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

| Condition                                                                                                                                           | Response                                                                                                                                                                        |
| --------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A V2 method                                                                                                                                         | JSON-RPC `-32601`                                                                                                                                                               |
| A target before Unzen                                                                                                                               | JSON-RPC `-38005`                                                                                                                                                               |
| Unknown or evicted payload ID                                                                                                                       | JSON-RPC `-38001`                                                                                                                                                               |
| FCUv3 attributes without `withdrawals` or `parentBeaconBlockRoot`, or with `slotNumber`                                                             | JSON-RPC `-38003`                                                                                                                                                               |
| FCUv3 attributes with `targetGasLimit`                                                                                                              | JSON-RPC `-32602`                                                                                                                                                               |
| Malformed V4 arguments or unsupported payload properties                                                                                            | JSON-RPC `-32602`                                                                                                                                                               |
| A nonzero root before Etna                                                                                                                          | JSON-RPC `-32602`                                                                                                                                                               |
| FCUv3 attributes whose `blockMetadata.timestamp` differs from `payloadAttributes.timestamp`                                                         | JSON-RPC `-32602`                                                                                                                                                               |
| Invalid Etna FCUv3 attributes, such as a zero root, a non-29-byte `extraData`, an `extraData[0]` other than 100, or a `txList` that does not decode | JSON-RPC `-32602` (current)                                                                                                                                                     |
| Etna FCUv3 attributes without `maxBlockBytes` or with `maxBlockBytes` below 1,024                                                                   | JSON-RPC `-32602`                                                                                                                                                               |
| FCUv3 attributes with `maxBlockBytes` before Etna                                                                                                   | JSON-RPC `-32602`                                                                                                                                                               |
| An Etna payload with a zero root, withdrawals, blob gas, or nonempty side arrays                                                                    | JSON-RPC `-32602` (current)                                                                                                                                                     |
| An Unzen payload with withdrawals, blob gas, or nonempty side arrays                                                                                | Payload status `INVALID`                                                                                                                                                        |
| Wrong Etna `extraData` length, or an `extraData[0]` other than 100, on import                                                                       | Payload status `INVALID`                                                                                                                                                        |
| An Etna block whose block-start call reverts, halts or finds no code                                                                                | Not an error: payload status `VALID` if the block is otherwise valid, and a build continues                                                                                     |
| A fault of the node's own storage or environment during the block-start call, such as a failed database read                                        | JSON-RPC `-32603` (internal error), never a payload status; a build that hits it answers like any other failed build                                                            |
| Block-hash mismatch                                                                                                                                 | Payload status `INVALID`                                                                                                                                                        |
| Etna zk-gas exhaustion or zk-gas/difficulty mismatch                                                                                                | Payload status `INVALID`                                                                                                                                                        |
| Unzen zk-gas exhaustion or zk-gas/difficulty mismatch                                                                                               | JSON-RPC `-32603` (historical internal-error mapping)                                                                                                                           |
| An Etna FCUv3 build that fails on a parent whose state is pruned                                                                                    | JSON-RPC `-38100` (`cannot build on parent`; `data` is diagnostic text)                                                                                                         |
| Any other failed build, from the FCUv3 that started it                                                                                              | JSON-RPC `-32603` (`missing payload`)                                                                                                                                           |
| `getPayloadV5` for a payload whose build failed                                                                                                     | JSON-RPC `-38001` (the FCUv3 already resolved the job)                                                                                                                          |
| An FCUv3 whose head is a canonical ancestor, including one below the finalized block                                                                | `VALID`; the head stays; with attributes, also a build on that ancestor (never `-38006`)                                                                                        |
| A payload transaction that cannot be decoded, has an unsupported type, or whose signer cannot be recovered                                          | Payload status `INVALID` (a given-list build drops a blob transaction or one whose signer cannot be recovered, and rejects an Etna `txList` that does not decode with `-32602`) |

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
`payloadAttributes.timestamp`. For an Etna target, also send no `anchorTransaction` and exactly 29
bytes of `blockMetadata.extraData`: the block's own header value, `missBitmap` included, from which
the block-start call takes its arguments. That field keeps its Base64 encoding; do not send hex.

Etna attributes carry one additional top-level member next to `anchorTransaction`:

| Member          | Type                                       | Rule                                                                                                                                                                                   |
| --------------- | ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `maxBlockBytes` | JSON number, like `blockMetadata.gasLimit` | Required on every Etna build, given-list builds included, and at least 1,024. Drivers send 124,000. Bounds the built block's length ([byte limit](#byte-limit)). Rejected before Etna. |

The existing `blockMetadata.txList` selects the build mode: present (including an empty list)
means a given-list build; absent means a pool build. A given list uses Ethereum's block-body
encoding: an RLP list whose items are a legacy transaction's bare RLP list or a typed transaction's
EIP-2718 bytes as an RLP byte string. This differs from the byte-string form the
[byte limit](#byte-limit) counts. An Etna `txList` that does not decode answers `-32602`; before
Etna it builds an empty block.

### Build modes

Every Etna build, the forced-inclusion singleton included, starts from the parent's state after the
block-start processing: the EIP-2935 and EIP-4788 writes, then the
[block-start staking call](#block-start-staking-call) with the attributes' `extraData`. It starts
with the block's full gas limit, the full zk-gas budget, and a transaction byte budget derived from
`maxBlockBytes` ([byte limit](#byte-limit)). A transaction that is left out leaves no state,
receipt, or fee behind. An ordinary EVM revert or out-of-gas is included and charged as usual.

- **Given list.** The list runs in order. A transaction is left out for a transaction-level reason,
  such as an unsupported type, a size above the remaining byte budget, a nonce, balance, base-fee,
  or intrinsic-gas failure against the current state, or a gas limit above the block's remaining
  gas. A transaction that exceeds the zk-gas limit or overflows its metering is discarded the same
  way and the rest of the list still runs; before Etna the first such transaction ends the list.
  A given-list build of `[tx]` therefore returns either a block holding `tx` or an empty block,
  never an error for a transaction-level reason; only failures unrelated to the transactions, such
  as a state read error, fail the build. The prover's candidate helper (`execute_derived_block`)
  applies the same rules.
- **Pool.** The builder selects from its own transaction pool by effective tip, within the gas,
  zk-gas, and byte budgets left. As build policy, not a consensus rule, a pool transaction that
  exceeds the zk-gas limit is discarded together with its sender's later nonces and selection
  continues with other senders.

#### Driver-owned forced inclusion

The driver owns the forced-inclusion queue and sends `[FI, ordinary…]` as a given list; the EL
applies the ordinary given-list rules. Only a completed empty build of `[FI]` alone on the same
parent and attributes is skip evidence; an omission from a longer list or an RPC failure is not.
The same attributes include the block's own 29-byte `extraData`, so the singleton build makes the
block-start call exactly as the block does, after the two system writes.

#### Byte limit

`maxBlockBytes` bounds the block's executionBlock, `rlp([header, [tx…]])`, including its outer list
header. Every transaction, legacy included, counts as an RLP byte string holding its EIP-2718
bytes, the form in which the consensus envelope and the published blob carry it. Ethereum's
block-body form keeps a legacy transaction as a bare RLP list, so here a legacy transaction also
counts the RLP string header around that list; a typed transaction counts the same in both forms.
The bound is inclusive. The builder reserves room for the largest header the job can seal, so a
block may stop up to 16 bytes short of the bound; it never exceeds it.

## `taikoAuth` methods

### Preselection

`taikoAuth_txPoolContent` and `taikoAuth_txPoolContentWithMinTip` keep their existing parameters.
Preselection simulates the next block under the parent's fork rules and `extraData`, and never
applies pre-execution system calls: it makes neither the EIP-2935 and EIP-4788 writes nor the
block-start staking call, so the state it simulates against lacks the call's effects. On an Etna
parent it drops the legacy 2,000,000 zk-gas anchor reserve. At the boundary the parent is still
pre-Etna, so the first Etna block is selected with that reserve. Results are estimates; the builder
enforces the actual gas and zk-gas limits. Etna builds also enforce `maxBlockBytes`
([byte limit](#byte-limit)), which preselection does not apply.

### Staking address

`taikoAuth_etnaStakingAddress` takes no parameters and returns the node's built-in `EtnaStaking`
proxy address ([address table](#block-start-staking-call)), or `null` on a chain without one. It
reads the block executor's chain spec, so it names the address that every block-start call
targets. The driver compares it with the `staking` address that its config hash commits.

## Devnet activation override

`--devnet-etna-timestamp <TIMESTAMP>` (or `ALETHIA_RETH_DEVNET_ETNA_TIMESTAMP`) schedules Etna at
that timestamp on canonical Taiko devnet or on a custom Genesis JSON whose fields outside `alloc`
match canonical devnet; on any other chain the flag fails startup. `0` activates Etna at genesis; a
genesis built with the [recipe](../README.md#custom-devnet-genesis-with-etna) works only with `0`.
On canonical devnet itself, the pre-Etna L2 `SignalService` keeps its checkpoint store open, so a
golden-touch `anchorV4` can save forged checkpoints, and EIP-4788 and EIP-2935 have no code.
Canonical devnet also has no code at its `EtnaStaking` address,
`0x1670010000000000000000000000000000010003`, so every block-start call there is a no-op and the
fallback keys sign every block. Offline commands such as `stage run` and `re-execute` cannot
reproduce an overridden schedule.
See the [custom genesis recipe](../README.md#custom-devnet-genesis-with-etna).
