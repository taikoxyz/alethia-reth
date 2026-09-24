# TBD fork: anchorless execution and Osaka Engine API

Status: design for user review; implementation has not started.

## Intent and agreed decisions

Implement the alethia-reth portion of [taiko-mono issue 22147](https://github.com/taikoxyz/taiko-mono/issues/22147): replace the mandatory anchor transaction with an L1 execution block hash carried in `parentBeaconBlockRoot` and recorded through the existing standard EIP-4788 call. Preserve historical block validation and replay.

The user selected the Osaka Engine API combination and requested continued use of `blockValue` for zk-gas when technically compatible. This design uses:

- `engine_forkchoiceUpdatedV3`;
- `engine_getPayloadV5`;
- `engine_newPayloadV4`;
- `getPayloadV5.blockValue = header.difficulty = finalized block zk-gas`.

`TBD` is the user-approved temporary fork identifier, not an unspecified execution rule. Activation dates are intentionally not scheduled: built-in networks remain at `ForkCondition::Never` until a coordinated activation release. Devnet supports an explicit timestamp override.

This document specifies changes in alethia-reth and the integration contract that geth, drivers and provers must implement. It does not authorize changes or deployment in those other repositories.

## Baseline and compatibility finding

The inspected alethia-reth baseline is `0fb47d966f290c032e0ce88bdc8877121768d253`. It exposes the Taiko V2 methods, executes Osaka rules from Unzen, and already runs the EIP-2935 and EIP-4788 pre-execution calls. Its Engine conversion currently reconstructs a zero beacon root and rejects nonzero roots supplied for building.

Osaka retains a 256-bit quantity named `blockValue`, so transporting finalized zk-gas has no type or encoding conflict. It does differ from Ethereum's documented meaning of expected fee-recipient revenue. This is an explicit, existing Taiko API convention; generic Ethereum builder-selection consumers must not interpret this value as revenue. Internal payload selection continues to use actual fee accounting, not the overwritten RPC response field.

`blockValue` belongs to the getPayload response envelope. Standard newPayloadV4 does not accept it and standard ExecutionPayloadV3 does not contain header difficulty. The import-side `headerDifficulty` extension must therefore remain. Go and Rust drivers already map the V2 envelope's `blockValue` to `headerDifficulty`; the new path preserves that mapping.

## Fork and header rules

Add `TaikoHardfork::TBD`, `TaikoSpecId::TBD`, and timestamp activation helpers on both chain-spec traits. Map the new EVM spec to Osaka and explicitly reuse the Unzen zk-gas schedule. Preserve all earlier fork identifiers and activation schedules. Update fork-ID ordering and filtering tests.

Add an optional `--devnet-tbd-timestamp` / `ALETHIA_RETH_DEVNET_TBD_TIMESTAMP` override. Omission means disabled; explicit zero means active from genesis. Reject a configuration that schedules TBD before Unzen or without Unzen. Changes to existing devnet Unzen overrides must preserve that invariant.

For non-genesis blocks at or after activation:

1. `parentBeaconBlockRoot` must be present and nonzero. No build, import or proving path may substitute zero for a missing value.
2. The driver derives the value from the L1 block at the derived anchor block number; guests verify the exact hash against authenticated L1 headers. An ordinary EL node checks the header rule and executes it without fetching L1 data.
3. No transaction position is reserved for an anchor. Empty transaction lists are valid when all other block rules hold. Legacy anchor-shaped transactions receive ordinary transaction treatment; disabling their privileged contract effects is a contract activation prerequisite.
4. `extraData` retains the seven-byte Shasta layout. Fee sharing, base-fee calculation, Osaka execution, blob prohibition, and Unzen zk-gas limits and difficulty commitment remain in effect.
5. Existing strict post-Shasta timestamp ordering remains in effect.

Preserve genesis handling separately, including the canonical zero beacon root required by genesis processing. An explicit genesis activation does not require inventing an L1 anchor hash for the genesis header. The first non-genesis block must carry a real nonzero hash.

Before TBD, retain the legacy paths and historical acceptance rules. Unzen-era builders and Engine imports continue using their zero-root convention. Do not introduce retrospective validation tightening while adding the new branch.

## Engine API contract

### Routing and activation

Both API families remain registered behind the existing authenticated Engine endpoint. Advertise the three existing V2 methods and the three new methods in `engine_exchangeCapabilities`; do not advertise methods merely because upstream types exist.

| Operation | Before TBD | At/after TBD |
| --- | --- | --- |
| Build with attributes | forkchoiceUpdatedV2 | forkchoiceUpdatedV3 |
| Retrieve built payload | getPayloadV2 | getPayloadV5 |
| Submit execution payload | newPayloadV2 | newPayloadV4 |

Select by the target payload timestamp, including historical replay and reorgs. A known method with a payload outside its supported range returns `Unsupported fork`. FCU calls with null attributes may update fork choice through either served FCU version: they have no target build timestamp and must support cross-fork recovery.

Preserve L1-origin persistence, preconfirmation handling, proposal-to-block mapping, JWT authentication, and payload-store behavior when sharing the FCU implementation. Gate getPayload using the stored job's timestamp. Bind payload IDs to the root and execution-relevant Taiko attributes; keep existing pre-TBD IDs unchanged and give new jobs a distinct version domain. The payload-ID domain is internal and is not assumed to equal every RPC method suffix.

### forkchoiceUpdatedV3

Use standard V3 attributes, including mandatory `parentBeaconBlockRoot`, together with the existing Taiko metadata necessary for deterministic derivation and pool building. Require the two existing timestamp representations to agree. Preserve the current transaction-source distinction: an explicit list is derived input and an absent list requests pool selection.

At/after TBD, reject a supplied `anchorTransaction`. The legacy field remains available to the old API path. Preserve proposal ID and base-fee-sharing metadata, and require a nonzero root before accepting a build job.

### getPayloadV5

Return the Osaka `ExecutionPayloadEnvelopeV5` shape:

- `executionPayload`: ExecutionPayloadV3;
- `blockValue`: finalized block zk-gas from the built header difficulty, encoded as the existing hexadecimal quantity;
- `blobsBundle`: empty commitments, proofs and blobs;
- `executionRequests`: an empty array;
- `shouldOverrideBuilder`: false unless a separately defined builder-selection policy requires otherwise.

The payload carries `withdrawals: []`, zero blob-gas fields, and the actual committed transaction list. There is no additional zk-gas output field. Do not return fee revenue in `blockValue` on this path.

The standard envelope does not return `parentBeaconBlockRoot`. The driver retains the value used in FCU, associated with that payload job; receivers of externally built blocks obtain it from the original header/envelope. Do not recover it from a mutable latest-head lookup or assume `l1Origin.l1BlockHash` is the anchor hash: the origin field identifies inclusion, which is a different concept.

### newPayloadV4

Use the standard four positional arguments, with the existing Taiko difficulty extension in the first object:

```text
engine_newPayloadV4(
    executionPayloadV3WithHeaderDifficulty,
    expectedBlobVersionedHashes,
    parentBeaconBlockRoot,
    executionRequests
)
```

`executionPayloadV3WithHeaderDifficulty` contains the standard V3 payload fields plus required `headerDifficulty`. Preserve the established cross-client wire convention: drivers send `headerDifficulty` as a decimal JSON integer, converting the hexadecimal `blockValue` quantity numerically. The value is bounded by the existing zk-gas schedule; no lossy integer conversion is allowed. Explicit zero is valid, while missing or null is not.

The second and fourth arguments must be empty arrays, and the third must be a nonzero 32-byte value. Require empty withdrawals and zero blob-gas fields. Reject blob transactions and unsupported Amsterdam attributes/fields. The new payload requires a complete transaction array, including an explicit empty array for empty blocks; do not carry the legacy missing-transactions fallback into this route.

Preserve the Taiko difficulty extension in the internal execution sidecar. Restore difficulty and root before validating the supplied block hash, derive transaction and withdrawals roots from the actual body, and derive the requests hash from the empty request array. The new route does not use the legacy `txHash` or `withdrawalsHash` root overrides. Recompute zk-gas during execution and compare it with the supplied difficulty; it is not trusted because the driver supplied it.

Malformed arguments return the appropriate parameter/attribute error; invalid block commitments produce an invalid payload result. Tests pin missing-field versus zero-field behavior and version errors across all entrances, including direct Engine-tree payload submission.

This remains a Taiko-adapted Osaka API: both the extra difficulty field and the meaning of blockValue are documented deviations from Ethereum. It is not advertised as interchangeable with an unmodified Ethereum consensus client.

## Execution and transaction selection

Continue invoking the standard EIP-2935 and EIP-4788 calls through the shared executor for both build and import. Ensure their state changes are included in state roots and witnesses, even for blocks containing no transactions. Do not add a SignalService call, synthetic anchor transaction, synthetic receipt, or custom checkpoint-writing hook.

Separate ordinary fee-sharing context from legacy anchor identity. For TBD execution, install the fee-sharing percentage explicitly from block extraData and disable anchor identity and exemption derivation. Remove the marker call from the new execution branch, including the golden-touch nonce read used only by that marker. Keep the old branch intact for history. Full-block replay and witness paths receive the same authoritative fee context; standalone simulations must not manufacture an anchor exemption.

For post-TBD pool building, select ordinary transactions directly and use the supplied block gas limit without subtracting the one-million anchor reserve. Derived transaction lists similarly contain only ordinary transactions. The first ordinary transaction consumes DA, EVM gas and zk-gas by the same rules as every subsequent transaction.

Make index-zero anchor detection fork-dependent in both prover execution entry points and the debug tx-list witness RPC. After TBD, recoverable first-transaction failures follow ordinary derivation filtering and zk-gas truncation rules. Canonical imported block bodies must still match the actually committed receipts and transactions; import must not silently accept an invalid body by filtering it.

Keep EIP-4396 operating on the real parent header. At activation, the parent may still contain an anchor and its gas reserve; do not rewrite the parent's gas limit or gas used using the child's rules. Driver/protocol derivation must remove the reserve according to each relevant block's fork when translating manifest gas limits.

## Tx-pool simulation RPC

The existing taikoAuth preselection path uses the parent timestamp, no root, and a fixed two-million zk-gas anchor reserve. These assumptions cannot be used for a TBD target block.

Extend the two tx-pool selection methods with an optional final `blockContext` object containing target `timestamp`, `parentBeaconBlockRoot`, and seven-byte `extraData`. Other existing arguments continue supplying fee recipient, base fee and gas limits. Explicit context selects the execution rules for that target, runs pre-execution changes for the simulated block, and removes the anchor reserve only when TBD is active.

Legacy callers may omit context while simulating pre-TBD state. Once the selected parent is post-TBD, omission is an error. Updated drivers must provide context for every TBD target, including the first block crossing activation. This keeps approximate transaction preselection separate from final block validity: the actual build still validates its authoritative target attributes and applies all limits.

Update lightweight RPC request types and downstream client serialization together. Preserve the existing response shape and multi-list limits; this change does not redesign multi-list selection.

## Component boundaries

| Component | Responsibility |
| --- | --- |
| chainspec and CLI | Fork identity, activation, ordering, devnet override and prerequisite validation |
| primitives and Engine RPC | Versioned wire types, root/difficulty transport, payload IDs, method routing and errors |
| consensus | Post-fork root rule, fork-dependent anchor validation and canonical-body validation |
| block configuration and assembler | Carry the exact root and difficulty through all block/payload execution paths |
| EVM and block executor | Explicit fee-sharing context, legacy exemptions, standard system calls and unchanged zk-gas rules |
| payload builder and taikoAuth | Anchorless building, gas budget changes and target-aware simulation |
| debug/prover helpers | Fork-aware filtering, replay and witness parity |

Changes are limited to these integration points and their documentation/tests. No Amsterdam/BAL support, new fee model, new zk-gas schedule, database migration, or unrelated refactor is included.

## External activation prerequisites

These are release dependencies, not extra implementation work in this repository:

1. Coordinate the same activation timestamp and API wire contract across taiko-geth and both drivers. Driver normalization must retain explicit zero difficulty, the original anchor root and its L1-header provenance, including preconfirmation transport.
2. Roll out the geth build/import EIP-4788 parity fix before deploying the canonical beacon-roots contract. Deploy and test EIP-2935 and EIP-4788 before activation on existing networks. Provision canonical contract code in genesis only for newly created networks; do not rewrite an existing chain's genesis.
3. Disable the legacy L2 Anchor checkpoint-writing path no later than the first anchorless block. The issue discussion confirms that its old ancestor guard does not automatically disable the first such block. Deploy fork-gated contract changes in advance while preserving proxy storage and historical state.
4. Update guests to authenticate the derived L1 hash and inherited anchor using header commitments and verified L1 header preimages. The first TBD block whose parent is legacy needs a legacy parent-anchor lookup. Forced/default blocks must not depend on a permissionless reveal already having occurred.
5. Upgrade SignalService and relayer/UI consumers for permissionless checkpoint reveal. Reveals are ordinary transactions and are not an EL block-validity requirement. Keep the existing treasury available for fee distribution.

## Verification and acceptance

Implementation must demonstrate:

1. Unchanged pre-TBD historical block hashes, receipts, fee balances and zk-gas results, including Unzen history through V2.
2. Correct fork activation, fork IDs and old/new API selection at the boundary; forward activation and reorg back across the boundary; null-attributes FCU recovery.
3. A full FCU V3 -> getPayload V5 -> driver normalization -> newPayload V4 round trip with nonzero root and nonzero difficulty, preserving block hash, state root, receipts and zk-gas. Include decimal headerDifficulty encoding and zero difficulty for empty blocks.
4. Rejection of missing/zero roots, missing difficulty, mismatched block hash or recomputed difficulty, nonempty unsupported arrays, and unsupported method/fork combinations. Cover direct tree and block-to-payload conversion paths too.
5. Real canonical 4788/2935 bytecode in fixtures so calls write storage. Verify root records and block-hash history through build, import and prover execution, including empty blocks and reorg rollback. Missing-contract fixtures alone are insufficient evidence of parity.
6. Ordinary transaction behavior at index zero, including invalid nonce/signature, unsupported transaction type, EVM gas rejection and zk-gas truncation. All-imported-transactions-must-match-receipts remains enforced.
7. Post-fork golden-touch calls have normal balance checks, fees and refunds in execution and tracing. Ordinary fee sharing is unchanged, and a reveal transaction receives no special fee treatment.
8. Full post-fork gas budget and no anchor zk-gas reserve in payload building and explicit-context preselection, with the legacy reserve retained before activation.
9. Cross-client and prover agreement for the activation block, a normal block, an empty block, a forced/default block and a cross-fork reorg, before network activation is scheduled.

For the implementation, run targeted regression tests, then the repository-required `just fmt`, `just clippy` and `just test`. Every new non-test Rust symbol must carry the required purpose/contract documentation. A design-document-only change does not establish that an implementation passes those checks.

## Source references

- [Issue 22147](https://github.com/taikoxyz/taiko-mono/issues/22147), particularly the [accepted corrections](https://github.com/taikoxyz/taiko-mono/issues/22147#issuecomment-5756503026) and [standard-EIP-4788 direction](https://github.com/taikoxyz/taiko-mono/issues/22147#issuecomment-5756531478).
- [Cancun Engine API](https://github.com/ethereum/execution-apis/blob/main/src/engine/cancun.md), [Prague Engine API](https://github.com/ethereum/execution-apis/blob/main/src/engine/prague.md), and [Osaka Engine API](https://github.com/ethereum/execution-apis/blob/main/src/engine/osaka.md), inspected on 2026-09-24.
- [Current Go driver normalization](https://github.com/taikoxyz/taiko-mono/blob/4a0e3d19327e1cbe341798c9c615896c5fa13627/packages/taiko-client/pkg/rpc/engine_unzen.go).
- [Current Rust driver request encoding and tests](https://github.com/taikoxyz/taiko-mono/blob/4a0e3d19327e1cbe341798c9c615896c5fa13627/packages/taiko-client-rs/crates/rpc/src/auth.rs).
- [Reth hardfork integration checklist](https://github.com/paradigmxyz/reth/blob/main/HARDFORK-CHECKLIST.md), also retrieved through Context7.
