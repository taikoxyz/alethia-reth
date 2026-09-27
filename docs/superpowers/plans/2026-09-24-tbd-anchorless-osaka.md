# TBD Anchorless Osaka Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce an opt-in Taiko fork that removes the mandatory anchor transaction, commits the L1 anchor hash through EIP-4788, and serves the Osaka Engine API with zk-gas in `blockValue`.

**Architecture:** Gate all new behavior by the target block timestamp. Extend the existing execution-data sidecar to preserve Osaka header inputs, and keep build, import, replay, and proving on the shared executor. Separate fee-sharing context from legacy anchor identity while preserving the historical V2 path.

**Tech Stack:** Rust 1.95.0, the existing Cargo workspace, pinned Reth `f2eecc65482af4085e43eedb27a32024bf177a0d`, Alloy 2.x (currently resolved to 2.2.0), revm, jsonrpsee, and cargo-nextest. No production dependency upgrades are required.

**Spec:** [Approved design](../specs/2026-09-24-tbd-anchorless-osaka-design.md). Read it before executing any task. The inspected production baseline is `0fb47d966f290c032e0ce88bdc8877121768d253`; the design commit is `c82cf38`.

## Global Constraints

- `TBD` is the user-approved temporary fork identifier, not an unspecified execution rule.
- Activation dates are intentionally not scheduled: built-in networks remain at `ForkCondition::Never` until a coordinated activation release.
- `engine_forkchoiceUpdatedV3`; `engine_getPayloadV5`; `engine_newPayloadV4`.
- `getPayloadV5.blockValue = header.difficulty = finalized block zk-gas`.
- Internal payload selection continues to use actual fee accounting, not the overwritten RPC response field.
- For non-genesis blocks at or after activation, `parentBeaconBlockRoot` must be present and nonzero.
- Preserve genesis handling separately, including the canonical zero beacon root required by genesis processing.
- Before TBD, retain the legacy paths and historical acceptance rules.
- `extraData` retains the seven-byte Shasta layout.
- Explicit zero is valid, while missing or null is not. This applies to `headerDifficulty`.
- The new payload requires a complete transaction array, including an explicit empty array for empty blocks.
- Continue invoking the standard EIP-2935 and EIP-4788 calls through the shared executor for both build and import.
- Do not add a SignalService call, synthetic anchor transaction, synthetic receipt, or custom checkpoint-writing hook.
- Keep EIP-4396 operating on the real parent header.
- No Amsterdam/BAL support, new fee model, new zk-gas schedule, database migration, or unrelated refactor is included.
- Every new non-test Rust symbol, field, module, and implementation method needs purpose/contract Rustdoc, including trait implementations. Follow repository `AGENTS.md`.
- External clients, contracts, guests, and network activation remain release prerequisites. This plan changes only alethia-reth.

## Review Focus

1. A valid empty block loses explicit zero difficulty during driver normalization: preserve numeric zero, reject absence/null, and verify real system-contract writes (Tasks 2, 6, 8).
2. A retained build job is retrieved after the canonical head crosses or recrosses activation: gate the job's own timestamp, keep null-attributes FCU usable, and keep IDs distinct for different roots and selection modes (Tasks 5, 6, 8).
3. A golden-touch transaction is replayed without a preceding block-executor marker: no post-fork exemptions may be derived by either normal or inspected EVM factories (Tasks 3, 8).
4. Devnet Unzen changes rebuild the genesis header before applying the new override: explicit zero must remain distinguishable from omission, and fork-order validation must run after both overrides (Task 1).
5. The first ordinary transaction fails, or preselection crosses activation from a legacy parent: derived input may filter recoverable failures; canonical import must reject a mismatched body; explicit target context must control reserves and system calls (Tasks 4, 7, 8).

---

## File and interface map

Paths below are repository-relative. Each task lists its exact edit set; existing inline tests remain next to their implementation.

| Area | Ownership |
| --- | --- |
| `crates/chainspec`, `crates/cli`, EVM spec/schedule | Fork identity, configuration, and validation |
| `crates/primitives/src/engine/osaka.rs` (new) | New wire input, internal Osaka fields, conversion into execution data |
| `crates/primitives/src/tbd.rs` (new) | Small shared non-genesis root invariant; no chain-spec dependency |
| `crates/evm/src/{evm,alloy,factory,handler}.rs` | Explicit fee context and legacy-only anchor exemptions |
| `crates/evm/src/env.rs` (new) | Carry authoritative fee percentage when tracing recreates an EVM |
| `crates/block`, `crates/consensus`, debug RPC | Root propagation, system calls, canonical validation, proving/filtering |
| `crates/primitives/src/payload`, `crates/payload` | Fork-aware attributes, payload IDs, ordinary pool selection |
| `crates/rpc/src/engine` | Authenticated method registration, routing, V5 envelope, error mapping |
| `crates/rpc-types`, `crates/rpc/src/eth/auth` | Optional target block context for preselection |
| `crates/node/tests/tbd_engine.rs` (new) | Live Engine API round trips, boundary and reorg integration |
| `docs/engine-api-tbd.md` (new) | Driver wire contract, devnet usage, activation checklist |

Dependency order: Task 1 → Task 2 → Task 3 → Task 4 → Task 5 → Task 6 → Task 7 → Task 8 → Task 9. These are reviewable increments of one consensus change; none schedules network activation. Preserve existing public interfaces unless a task explicitly changes them. Snippets below define the essential implementation/test contracts; add imports and required Rustdoc in the owning files.

### Task 1: Register the fork and validate devnet configuration

**Files:**
- Modify: `crates/chainspec/src/hardfork.rs`, `crates/chainspec/src/spec.rs`.
- Modify: `crates/cli/src/lib.rs`, `crates/cli/src/command.rs`.
- Modify: `crates/evm/src/spec.rs`, `crates/evm/src/zk_gas/schedule.rs`, `crates/block/src/config.rs`.
- Test: inline modules in the files above.

**Interfaces:**
- Add `TaikoHardfork::TBD`, `TaikoSpecId::TBD`, and `name::TBD = "TBD"`.
- Add `fn is_tbd_active(&self, timestamp: u64) -> bool` to both `TaikoHardforks` and `TaikoExecutorSpec`, matching their existing Unzen forwarding patterns.
- Add `TaikoChainSpec::validate_tbd_fork_order(&self) -> Result<(), TbdForkOrderError>`. Define the error in `spec.rs`, with variants for missing Unzen, unsupported activation condition, and TBD preceding Unzen.
- Add `TaikoDevnetConfigExt::clone_with_devnet_fork_timestamps(&self, unzen_timestamp: u64, tbd_timestamp: Option<u64>) -> Result<Option<Self>, TbdForkOrderError>`. Retain `clone_with_devnet_unzen_timestamp` for existing consumers; use the combined entry in CLI startup so devnet identity is checked before either mutation.
- Add `TaikoNodeExtArgs::devnet_tbd_timestamp(&self) -> Option<u64>`; `NoArgs` returns `None`.

- [x] Add activation/order tests, including this direct fixture:

```rust
let mut spec = (*TAIKO_DEVNET).as_ref().clone();
assert!(!TaikoHardforks::is_tbd_active(&spec, u64::MAX));
spec.inner.hardforks.insert(TaikoHardfork::TBD, ForkCondition::Timestamp(100));
assert!(!TaikoHardforks::is_tbd_active(&spec, 99));
assert!(TaikoHardforks::is_tbd_active(&spec, 100));
assert!(spec.validate_tbd_fork_order().is_ok());
spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(101));
assert!(spec.validate_tbd_fork_order().is_err());
```

Add CLI parser cases for omitted option, `--devnet-tbd-timestamp 0`, timestamp 100, and the environment variable. Exercise combined overrides `(Unzen=100, TBD=100)`, `(100, 99)`, `(100, None)`, and `(0, Some(0))`; preserve genesis hash behavior of the existing Unzen override. Check all built-in chains default to Never and that disabled TBD does not change current fork IDs.

- [x] Run `cargo test -p alethia-reth-chainspec -p alethia-reth-cli -p alethia-reth-evm tbd --all-features`; expect missing variants/methods before implementation.
- [x] Register the fork in each activation table and name conversion. Map it to `SpecId::OSAKA`. Extend the schedule match explicitly to `TaikoSpecId::UNZEN | TaikoSpecId::TBD`, returning the existing schedule reference. Check TBD before Unzen in `taiko_spec_by_timestamp_and_block_number`.
- [x] Add the optional CLI field and combined override. For canonical devnet, apply the existing Unzen override to a clone, then insert the optional TBD timestamp and validate the final ordering. For non-devnet preserve the existing no-op override convention; always validate a custom chain's final fork order before database initialization. Never derive omission from a numeric zero.

```rust
#[arg(long, env = "ALETHIA_RETH_DEVNET_TBD_TIMESTAMP")]
pub devnet_tbd_timestamp: Option<u64>,
```

- [x] Run the targeted command plus `cargo test -p alethia-reth-block config --all-features`. Assert the old and new specs share the same zk-gas schedule and all legacy mapping/fork-ID tests still pass.
- [x] Commit only this task's files: `feat(chainspec): register the opt-in TBD fork`.

### Task 2: Preserve Osaka inputs through execution-data conversion

**Files:**
- Create: `crates/primitives/src/engine/osaka.rs`, `crates/primitives/src/tbd.rs`.
- Modify: `crates/primitives/src/lib.rs`, `crates/primitives/src/engine/mod.rs`, `crates/primitives/src/engine/types.rs`.
- Modify: `crates/primitives/Cargo.toml` to expose the existing workspace `serde_json` dependency under the `serde` feature for the inbound extension map; keep its resolved version unchanged.
- Test: inline modules in the new files and existing engine type tests.

**Interfaces:**
- `validate_tbd_root(is_tbd_active: bool, block_number: u64, root: Option<B256>) -> Result<(), MissingTbdBeaconRoot>` in `primitives::tbd`. Define `MissingTbdBeaconRoot { block_number: u64 }` with `Display` and `Error`. The error covers missing/zero non-genesis roots only; legacy and genesis return `Ok(())`.
- `TaikoExecutionPayloadV3` in `primitives::engine::osaka`: flattened `execution_payload: ExecutionPayloadV3`, required decimal `header_difficulty: u64`, and flattened `extra_fields: BTreeMap<String, serde_json::Value>` for rejecting legacy override/Amsterdam fields. Gate the JSON-specific field and serde behavior consistently with the crate's `serde` feature. A `u64` is sufficient for the existing bounded zk-gas schedule; conversions into it must be checked.
- `TaikoOsakaPayloadFields`: `withdrawals: Vec<Withdrawal>`, `blob_gas_used: u64`, `excess_blob_gas: u64`, `parent_beacon_block_root: B256`, `expected_blob_versioned_hashes: Vec<B256>`, `execution_requests: Vec<Bytes>`.
- Add `osaka: Option<TaikoOsakaPayloadFields>` to `TaikoExecutionDataSidecar`, serialized with `default` and `skip_serializing_if = "Option::is_none"`.
- `TaikoExecutionPayloadV3::into_execution_data(self, expected_blob_versioned_hashes: Vec<B256>, parent_beacon_block_root: B256, execution_requests: Vec<Bytes>) -> Result<TaikoExecutionData, TaikoOsakaInputError>`. Define the input error in `osaka.rs`; reject unknown extension keys instead of dropping them. Structural validation remains in the common validator in Task 6.

- [x] Write root-invariant tests covering `(legacy, 1, None)`, `(TBD, 0, zero)`, `(TBD, 1, None)`, `(TBD, 1, zero)`, and `(TBD, 1, nonzero)`.

```rust
assert!(validate_tbd_root(false, 1, None).is_ok());
assert!(validate_tbd_root(true, 0, Some(B256::ZERO)).is_ok());
assert!(validate_tbd_root(true, 1, None).is_err());
assert!(validate_tbd_root(true, 1, Some(B256::ZERO)).is_err());
assert!(validate_tbd_root(true, 1, Some(B256::with_last_byte(1))).is_ok());
```

- [x] Add serde tests by serializing `ExecutionPayloadV3::default()` into a JSON object and inserting `headerDifficulty` as a number. Pin zero, nonzero, missing, null, overflow, missing/null transactions, and explicit empty transactions. Test inbound `txHash`, `withdrawalsHash`, `blockAccessList`, `slotNumber`, and `targetGasLimit` rejection after normalization. Preserve a snapshot of the legacy V2 serialized object without any new fields.

```rust
let mut value = serde_json::to_value(ExecutionPayloadV3::default()).unwrap();
value["headerDifficulty"] = serde_json::json!(0);
let payload: TaikoExecutionPayloadV3 = serde_json::from_value(value.clone()).unwrap();
assert_eq!(payload.header_difficulty, 0);
value.as_object_mut().unwrap().remove("headerDifficulty");
assert!(serde_json::from_value::<TaikoExecutionPayloadV3>(value).is_err());
```

- [x] Run `cargo test -p alethia-reth-primitives --all-features`; expect the new-type tests to fail before implementing them.
- [x] Implement the root guard without a chainspec dependency:

```rust
if is_tbd_active && block_number != 0 && root.is_none_or(|root| root.is_zero()) {
    return Err(MissingTbdBeaconRoot { block_number });
}
Ok(())
```

- [x] Implement wire normalization. Move the V1 core into the existing `TaikoExecutionPayloadV1`, retain all V3 fields in `osaka`, and set `header_difficulty: Some(U256::from(self.header_difficulty))`. Derive transaction/withdrawal commitments from the actual body when converting to a block; never use legacy roots in the Osaka branch. Keep the inbound extra-field map until its emptiness has been checked; test serde flatten behavior with the pinned Alloy version.
- [x] Update `ExecutionPayload` trait getters and `into_payload()` to return V3 when Osaka fields exist. Update `TaikoEngineTypes::block_to_payload` to capture the original root, withdrawals, blob-gas fields, requests/body information, and difficulty before consuming the block. Its lack of chainspec means it must preserve header data without deciding fork activation. Keep BAL/slot rejection sentinels. Update all struct literals with `osaka: None` where they represent legacy input.
- [x] Add block → execution-data → V3 checks with a nonzero root/difficulty, plus an empty block. Ensure a header with unsupported requests or missing Osaka components cannot be silently normalized into a valid block: the original block hash must remain the comparison target, and Task 6 must reject inconsistent reconstructed fields.
- [x] Run the primitives tests; commit `feat(engine): preserve Osaka payload header inputs`.

### Task 3: Separate fee sharing from legacy anchor exemptions

**Files:**
- Create: `crates/evm/src/env.rs`; export it from `crates/evm/src/lib.rs`.
- Modify: `crates/evm/src/evm.rs`, `crates/evm/src/alloy.rs`, `crates/evm/src/factory.rs`, `crates/evm/src/handler.rs`.
- Modify: `crates/block/src/config.rs` to populate fee context from the header/attributes/payload's extraData.
- Update environment types in tests/helpers: `crates/evm/src/zk_gas/tests.rs`, `crates/block/src/assembler.rs`, `crates/block/src/testutil.rs`, `crates/rpc/src/eth/auth/tests.rs`.
- Test: existing fee/refund/replay test modules in these files.

**Interfaces:**
- Add `TaikoEvmExtraExecutionCtx::for_anchorless_block(base_fee_share_pctg: u64) -> Self`.
- Add `TaikoEvmExtraExecutionCtx::matches_legacy_anchor(&self, caller: Address, nonce: u64, to: Option<Address>, treasury: Address) -> bool`.
- Add `TaikoEvmExtraExecutionCtx::has_authoritative_fee_context(&self) -> bool`.
- Extend existing `TaikoAnchorEvm` with `fn set_block_fee_context(&mut self, base_fee_share_pctg: u64)`. Implement it on `TaikoEvmWrapper` by installing the anchorless context and disabling anchor derivation.
- Retain old `new`/`derived` constructor behavior and legacy accessors where existing consumers require them. Add a private boolean for legacy-anchor eligibility; its default must be false. Rename the private authority flag to describe fee context rather than a marker call.
- Define `TaikoBlockEnv { inner: BlockEnv, base_fee_share_pctg: Option<u64> }` and `type TaikoEvmEnv = EvmEnv<TaikoSpecId, TaikoBlockEnv>` in `env.rs`. The optional field distinguishes an authoritative zero percentage from a standalone environment with no header context. Implement `Default`, `Clone`, `Debug`, `From<BlockEnv>` (percentage None), `Deref`/`DerefMut` to the inner environment, revm `Block`, and Alloy `BlockEnvironment` by delegation. Set `TaikoEvmContext`'s block parameter and both EVM/factory `BlockEnv` associated types to `TaikoBlockEnv`; update `block()`/`finish()` signatures accordingly.
- `TaikoBlockEnv::with_base_fee_share_pctg(mut self, percentage: u64) -> Self` sets the optional field. `TaikoEvmConfig` fills it only for TBD using `decode_shasta_basefee_sharing_pctg`; legacy remains None so historical standalone trace behavior stays unchanged. Define `InvalidTbdExtraData { len: usize }` in block config for malformed post-fork header/attribute extraData at environment construction. Existing block-context decoding remains authoritative for execution.

- [x] Add a pure context test and normal/inspected execution tests:

```rust
let ctx = TaikoEvmExtraExecutionCtx::for_anchorless_block(25);
let golden = Address::from(TAIKO_GOLDEN_TOUCH_ADDRESS);
let treasury = Address::with_last_byte(9);
assert_eq!(ctx.base_fee_share_pctg(), 25);
assert!(ctx.has_authoritative_fee_context());
assert!(!ctx.matches_legacy_anchor(golden, 0, Some(treasury), treasury));
```

Reuse the existing `replay_env`, `replay_db`, `anchor_tx`, and `user_tx` fixtures in `alloy.rs`, updating their environment type to `TaikoEvmEnv`. Set `env.cfg_env.spec = TaikoSpecId::TBD`; an underfunded golden-touch call must fail balance validation in both factory variants. A funded call must pay ordinary gas, receive the ordinary refund, and distribute base fee with the installed percentage. Assert treasury and beneficiary deltas numerically from the existing fee formula. Preserve all current legacy dust-balance/anchor tests.

Add a recreated-EVM regression: construct `evm_env` from a real TBD header with a 25% share, clone it, create a fresh inspected EVM from the clone after pre-execution, and check its fee deltas against full-block execution. Repeat for `Some(0)` and a raw `None` context. This specifically exercises the upstream trace path, which applies pre-execution on one executor and then creates another EVM for replay/inspection.

- [x] Run `cargo test -p alethia-reth-evm --all-features`; expect the new context tests to fail before implementing the interface.
- [x] Add the explicit context and replace duplicated caller/nonce/treasury checks in handler reward, balance, and refund branches with `matches_legacy_anchor`. Use `has_authoritative_fee_context()` for ordinary fee sharing. The anchorless constructor sets fee authority true and legacy eligibility false.
- [x] Implement the environment wrapper and delegate every `Block` getter (`number`, `beneficiary`, `timestamp`, `gas_limit`, `basefee`, `difficulty`, `prevrandao`, `blob_excess_gas_and_price`, `slot_num`) to `inner`; `BlockEnvironment::inner_mut` returns `&mut self.inner`. Wrap the three environment constructors in block config and populate the percentage from the original seven-byte extraData for TBD. This avoids mutable global state or a database-dependent fee lookup during tracing.

```rust
let mut block_env = TaikoBlockEnv::from(block_env);
if spec.is_enabled_in(TaikoSpecId::TBD) {
    if extra_data.len() != 7 {
        return Err(AnyError::new(InvalidTbdExtraData { len: extra_data.len() }));
    }
    block_env = block_env.with_base_fee_share_pctg(
        u64::from(decode_shasta_basefee_sharing_pctg(extra_data.as_ref())),
    );
}
```

Here `extra_data` is the original header extraData in `evm_env`, the next-block attribute extraData in `next_evm_env`, and the supplied payload extraData in `evm_env_for_payload`. Migrate explicit `EvmEnv<TaikoSpecId>` annotations in affected fixtures to `TaikoEvmEnv` and use `.into()` for raw `BlockEnv` literals; do not alter their values.
- [x] In both `TaikoEvmFactory` creation paths disable derivation when `spec.is_enabled_in(TaikoSpecId::TBD)`. Guard legacy marker recognition in `transact_system_call` by the active spec as well, so a direct post-fork call cannot reinstall a privileged context. Standard 2935/4788 system calls continue through their existing execution path.

```rust
let allow_legacy_anchor = !spec.is_enabled_in(TaikoSpecId::TBD);
evm.set_anchor_ctx_derivation_enabled(allow_legacy_anchor);
if !allow_legacy_anchor {
    if let Some(percentage) = base_fee_share_pctg {
        evm.set_block_fee_context(percentage);
    }
}
```

Capture `base_fee_share_pctg = input.block_env.base_fee_share_pctg` before moving the input into the revm context. Full-header trace environments therefore preserve fee authority when recreated; raw standalone environments still never derive an anchor exemption.

- [x] Verify normal and inspected factories agree on status, gas/refunds, and balance deltas; run EVM tests and commit `feat(evm): decouple fee sharing from anchor identity`.

### Task 4: Enforce the root and execute anchorless blocks consistently

**Files:**
- Modify: `crates/block/src/config.rs`, `crates/block/src/executor.rs`, `crates/block/src/derived_block.rs`, `crates/block/src/testutil.rs`.
- Modify: `crates/consensus/src/validation/mod.rs`, `crates/consensus/src/validation/anchor.rs`.
- Modify: `crates/rpc/src/debug.rs`.
- Test: inline modules in those files; extend existing prover/zk-gas tests.

**Interfaces:**
- Consume Task 1 `is_tbd_active`, Task 2 `validate_tbd_root`, Task 3 `TaikoAnchorEvm::set_block_fee_context`.
- Change private `normalize_parent_beacon_block_root` to accept `(is_unzen_active: bool, is_tbd_active: bool, block_number: u64, root: Option<B256>) -> Result<Option<B256>, MissingTbdBeaconRoot>`.
- Add test-utils helpers `tbd_chain_spec() -> TaikoChainSpec`, `tbd_evm_env() -> TaikoEvmEnv`, and `tbd_execution_ctx<'a>(root: B256) -> TaikoBlockExecutionCtx<'a>`. They wrap the existing Unzen fixtures (whose environment return type was migrated in Task 3), set TBD at zero, use seven zero extraData bytes, and set the nonzero root. Add `db_with_system_contracts(accounts: &[(Address, u64)]) -> InMemoryDB`, described below.

- [x] Test missing/zero roots through all three configuration entrances: `context_for_block`, `context_for_next_block`, `context_for_payload`. Preserve Unzen fallback to zero; require exact nonzero TBD root. Check genesis independently, and check malformed extraData fails through existing validation rather than being silently replaced.

```rust
let root = B256::with_last_byte(7);
assert_eq!(normalize_parent_beacon_block_root(true, false, 1, None).unwrap(), Some(B256::ZERO));
assert_eq!(normalize_parent_beacon_block_root(true, true, 1, Some(root)).unwrap(), Some(root));
assert!(normalize_parent_beacon_block_root(true, true, 1, None).is_err());
```

- [x] Add genuine system contracts to the shared test DB using existing `insert_contract`:

```rust
use alloy_eips::{eip2935, eip4788};
let mut db = db_with_contracts(accounts);
insert_contract(&mut db, eip4788::BEACON_ROOTS_ADDRESS,
    Bytecode::new_raw(eip4788::BEACON_ROOTS_CODE.clone()));
insert_contract(&mut db, eip2935::HISTORY_STORAGE_ADDRESS,
    Bytecode::new_raw(eip2935::HISTORY_STORAGE_CODE.clone()));
```

Execute an empty block at number 1, timestamp 1, with nonzero root and nonzero parent hash. Assert storage slot `1` at the 4788 address equals timestamp 1, slot `1 + 8191` equals the root, and history storage slot `0` equals the parent hash. Assert zero transaction receipts, zero ordinary transaction gas, and the expected finalized zk-gas. Do not assert absence of state changes merely because the block is empty.

- [x] Run `cargo test -p alethia-reth-block -p alethia-reth-consensus -p alethia-reth-rpc tbd --all-features`; verify the new root/storage/anchorless tests fail first.
- [x] Call the root guard in consensus header validation, configuration entrances, and shared pre-execution validation (to cover callers constructing execution contexts directly). In the normalizer validate before applying the legacy zero fallback. Do not tighten pre-TBD header acceptance.
- [x] Keep the two existing standard system calls in the executor. Branch the following fee initialization on `self.spec.is_tbd_active(timestamp)`: post-fork decode the existing fee percentage and call `set_block_fee_context`; legacy executes the current marker including its nonce access. Preserve existing decoding/error rules.
- [x] Fork-gate anchor validation and both prover index-zero branches plus debug tx-list witness handling:

```rust
let requires_legacy_anchor = !spec.is_tbd_active(timestamp);
let is_anchor_position = requires_legacy_anchor && index == 0;
```

After activation every position uses ordinary signature recovery, type checks, DA/gas accounting, and recoverable-error filtering. Keep canonical imported body/receipt checks intact. Preserve the existing legacy empty-body acceptance; do not add a new pre-fork empty-block rejection.
- [x] Add first-position invalid nonce, invalid signature, unsupported type, excessive EVM gas, and zk-gas exhaustion cases. For derivation assert the exact resulting transaction list and truncation point; for canonical import assert rejection of the unfiltered body. Repeat the valid first transaction through both prover execution entry points and debug witness generation; a skip-difficulty option must not bypass root validation.
- [x] Run targeted tests plus existing consensus and prover suites; commit `feat(block): execute TBD blocks without an anchor transaction`.

### Task 5: Build ordinary transactions with fork-specific payload IDs

**Files:**
- Modify: `crates/primitives/src/payload/attributes.rs`, `crates/primitives/src/payload/builder.rs`.
- Modify: `crates/payload/src/builder/mod.rs`, `crates/payload/src/builder/execution.rs`.
- Test: inline payload normalization, ID, and pool-execution tests.

**Interfaces:**
- Add `PAYLOAD_ID_VERSION_TBD: u8 = 3` (an internal domain, not an RPC method suffix).
- Add `TaikoPayloadBuilderAttributes::try_new_for_fork(parent: B256, attributes: TaikoPayloadAttributes, is_tbd_active: bool) -> Result<Self, alloy_rlp::Error>`. Existing `try_new` delegates with `false` for legacy callers. The actual payload builder passes the chain's target-timestamp result.
- Add `payload_id_version(attributes: &TaikoPayloadAttributes) -> u8`: nonzero root selects the new hashing domain; this is only a stable ID convention, never authorization to activate a fork. Actual validation independently checks the timestamp.
- Retain `payload_id_taiko(parent: &B256, attributes: &TaikoPayloadAttributes, payload_version: u8) -> PayloadId`. Both attribute trait and normalized builder must call it with the same chosen domain.
- `PoolExecutionContext::anchor_tx` becomes `Option<&Recovered<EthTransactionSigned>>`. Rename `execute_anchor_and_pool_transactions` to `execute_pool_transactions`; preserve its current generic bounds and return type `Result<ExecutionOutcome, PayloadBuilderError>`.

- [x] Extend the existing `test_payload_config` fixture for TBD: timestamp 100 in both fields, root `B256::with_last_byte(1)`, seven-byte extraData, no anchor, and either absent txList (pool) or RLP empty list (derived). Test normalization succeeds only with the explicit fork-aware constructor. Test mismatched timestamp, nonzero high U256 timestamp bits, supplied anchor, nonempty withdrawals, and missing/zero root fail.
- [x] Add ID tests using the existing attributes fixture. Mutate one item at a time: root, full base fee, gas limit, metadata beneficiary, extraData, pool-vs-explicit-empty source, and tx list. Each execution-relevant mutation must change the new ID; cloned attributes must retain it. Pin an existing V2 ID as a literal before changing the hashing code.

```rust
let before = payload_id_taiko(&parent, &attributes, PAYLOAD_ID_VERSION_TBD);
let mut changed = attributes.clone();
changed.block_metadata.gas_limit += 1;
assert_ne!(before, payload_id_taiko(&parent, &changed, PAYLOAD_ID_VERSION_TBD));
assert_eq!(before, payload_id_taiko(&parent, &attributes, PAYLOAD_ID_VERSION_TBD));
```

- [x] Run `cargo test -p alethia-reth-primitives -p alethia-reth-payload --all-features`; the new normalization and gas-budget cases should fail first.
- [x] Implement fork-aware normalization before decoding transactions. Compare the metadata timestamp as U256 to `U256::from(attributes.payload_attributes.timestamp)` before any narrowing. Reject anchorTransaction after TBD and enforce the existing supported-attributes constraints. Pass the chain spec into private `normalize_payload_config` and update its tests.
- [x] Keep the V2 hash bytes unchanged. For version 3 hash a fixed `b"taiko-tbd-payload-v1"` tag plus an unambiguous encoding: parent, timestamp, prevRandao, suggested recipient, root, withdrawals commitment, full 32-byte base fee, metadata beneficiary/gas/timestamp/mixHash, extraData length+bytes, source discriminator, and txList length+bytes. Bind L1-origin metadata as well to prevent two persistence jobs with different origins sharing a cached ID: append `block_id` (32 bytes), `l2_block_hash` (32), optional `l1_block_height` (presence byte and 32), optional `l1_block_hash` (presence byte and 32), `build_payload_args_id` (8), `is_forced_inclusion` (one byte), and `signature` (65). Encode integers and lengths big-endian, lengths as u64, booleans/presence as 0 or 1. Overwrite ID byte zero with 3 after hashing. Test the exact field order and both absent/empty distinctions without changing V2 fixtures.
- [x] Fork-gate the pool path. Legacy still requires and executes its supplied anchor with the existing one-million gas reservation. TBD requires no anchor, calls common pool selection directly, and passes the full supplied gas limit. Keep derived-list filtering and internal total-fee accounting unchanged.

```rust
let gas_limit = if is_tbd_active {
    attributes.gas_limit
} else {
    attributes.gas_limit.saturating_sub(ANCHOR_V3_V4_GAS_LIMIT)
};
```

- [x] Add a selection test whose transaction fits the full post-fork budget but not the legacy reduced budget, and compare pool/derived builds of the same ordinary list. Ensure cancellation and zk-gas exhaustion still return their existing outcomes. Run targeted tests and commit `feat(payload): build anchorless TBD payloads`.

### Task 6: Serve and validate the Osaka Engine API family

**Files:**
- Modify: `crates/rpc/src/engine/api.rs`, `crates/rpc/src/engine/validator.rs`, `crates/rpc/src/engine/builder.rs`.
- Test: inline Engine API and validator tests.

**Interfaces:**
- Add methods to existing `TaikoEngineApi` and its server/client implementations:

```rust
async fn new_payload_v4(
    &self,
    payload: TaikoExecutionPayloadV3,
    expected_blob_versioned_hashes: Vec<B256>,
    parent_beacon_block_root: B256,
    execution_requests: Vec<Bytes>,
) -> RpcResult<PayloadStatus>;

async fn fork_choice_updated_v3(
    &self,
    fork_choice_state: ForkchoiceState,
    payload_attributes: Option<Engine::PayloadAttributes>,
) -> RpcResult<ForkchoiceUpdated>;

async fn get_payload_v5(
    &self,
    payload_id: PayloadId,
) -> RpcResult<Engine::ExecutionPayloadEnvelopeV5>;
```

- Add free conversion helper `convert_built_payload_to_execution_payload_envelope_v5(built_payload: EthBuiltPayload) -> ExecutionPayloadEnvelopeV5` alongside the existing V2 converter.
- Add private routing helper `validate_taiko_api_fork(is_tbd_active: bool, wants_tbd: bool) -> Result<(), EngineApiError>`; mismatch returns `EngineObjectValidationError::UnsupportedFork.into()` (`-38005`). Do not use Ethereum Osaka activation as this gate: Unzen already activates Osaka.
- Share existing FCU persistence behavior through a private `fork_choice_updated(version: EngineApiMessageVersion, state: ForkchoiceState, attributes: Option<TaikoPayloadAttributes>) -> RpcResult<ForkchoiceUpdated>` helper in the concrete implementation.

- [x] Test the fork matrix with `(timestamp=99, activation=100)` and `(100,100)` for all six served methods. Test unknown payload ID remains `Unknown payload`, a stored pre-fork job remains V2 after a head change, and stored post-fork job remains V5 after reorg. Assert capability registration contains exactly the three V2 methods plus FCU V3/get V5/new V4; exchangeCapabilities itself is served but not advertised.
- [x] Add validator tests for nonzero root+difficulty roundtrip and mutations: root missing/zero, difficulty missing/null, empty body with explicit zero, wrong difficulty with self-consistent header hash, wrong block hash, nonempty withdrawals, nonzero blob gas, blob transaction, nonempty versioned hashes/requests, incomplete transactions, and Amsterdam/legacy-override fields. Invoke the common validator directly as well as the version check to cover direct tree submission.
- [x] Run `cargo test -p alethia-reth-rpc engine --all-features`; confirm the new tests fail before implementing registration/routing.
- [x] Implement four-argument V4 normalization and common validation. For TBD require the Osaka sidecar, full transactions, difficulty, nonzero root, empty withdrawals/hashes/requests, and zero blob fields. Build the V3 block from the body; restore difficulty and root; set the empty requests hash; only then verify the original supplied block hash. Leave the existing legacy root overrides in the legacy timestamp branch. Continue post-execution zk-gas equality validation from the shared executor.
- [x] Implement FCU V3 with target-timestamp gate and Task 5 attribute checks. Extract existing V2 persistence once and call it from both versions. Null attributes bypass only the build-version gate, not normal forkchoice validation. Preserve proposal-to-last-block updates, preconfirmation handling, and origin transaction atomicity. A failed attributes request must not start a job or publish origin metadata.
- [x] Implement get V5 from the payload store and gate using the resolved job timestamp. Capture difficulty before envelope conversion, then set `block_value`. Keep standard V3 body, empty V2 blobs bundle, empty requests, and `should_override_builder=false`. Do not mutate `EthBuiltPayload` fee data used for ranking.

```rust
let difficulty = built_payload.block().difficulty;
let mut envelope: ExecutionPayloadEnvelopeV5 = built_payload.into();
envelope.block_value = difficulty;
envelope.should_override_builder = false;
```

Assert that the pinned Reth conversion supplies the required empty arrays and zero blob fields; explicitly set them if conversion does not. Add the V5 associated-type equality to all relevant generic bounds so the wrapper uses the concrete envelope.
- [x] Preserve authenticated module construction in `engine/builder.rs`; only merge the added methods into the current module. Test both FCU methods with null attributes and parity of L1-origin/proposal writes. Run Engine tests and commit `feat(rpc): expose the TBD Osaka Engine API`.

### Task 7: Make tx-pool preselection use the target block context

**Files:**
- Modify: `crates/rpc-types/src/lib.rs`, `crates/rpc-types/Cargo.toml` if the existing workspace serde quantity helper is needed.
- Modify: `crates/rpc/src/eth/auth/types.rs`, `crates/rpc/src/eth/auth/mod.rs`, `crates/rpc/src/eth/auth/tests.rs`.

**Interfaces:**
- Export `TxPoolBlockContext { timestamp: u64, parent_beacon_block_root: B256, extra_data: Bytes }` from `alethia-reth-rpc-types`; camelCase fields, timestamp encoded as a hex quantity, root/bytes as normal Alloy hex values.
- Add `block_context: Option<TxPoolBlockContext>` to both parameter structs, with serde default/skip-none; preserve it in their `From` conversion.
- Append `block_context: Option<TxPoolBlockContext>` as the final positional parameter of both existing RPC methods and generated clients. Do not reorder existing arguments.
- Add private `resolve_tx_pool_block_context(chain_spec: &TaikoChainSpec, parent: &Header, supplied: Option<TxPoolBlockContext>) -> Result<TxPoolBlockContext, EthApiError>`.

- [x] Test old JSON parameter structs deserialize without context and reserialize identically. Test a new context with timestamp `"0x64"`, nonzero root, and seven-byte extraData, including conversion into the min-tip variant.

```rust
let context = TxPoolBlockContext {
    timestamp: 100,
    parent_beacon_block_root: B256::with_last_byte(1),
    extra_data: Bytes::from(vec![0; 7]),
};
let value = serde_json::to_value(&context).unwrap();
assert_eq!(value["timestamp"], "0x64");
assert_eq!(serde_json::from_value::<TxPoolBlockContext>(value).unwrap(), context);
```

- [x] Extend auth tests with activation at 100, parent at 99, explicit target at 100. Assert target rules remove the reserve and use the provided root/extraData. A post-fork parent with omitted context must error. Legacy omission preserves parent-time simulation and the two-million zk-gas reserve. Explicit context requires a later timestamp and validates the target fork's root and seven-byte extraData; test missing/zero root and overflow in existing multi-list gas multiplication.
- [x] Run `cargo test -p alethia-reth-rpc-types -p alethia-reth-rpc tx_pool --all-features`; expect new interface/behavior failures before implementation.
- [x] Implement the resolver. Without context, reject an already-TBD parent; otherwise preserve the current legacy timestamp/root/extraData behavior. With context, use its target timestamp and exact root/extraData. Reject a target not later than its parent where strict Shasta timestamp rules apply. Never derive an L1 hash from L1-origin storage or latest state.
- [x] Pass resolved context into `TaikoNextBlockEnvAttributes`. For an explicit context run `apply_pre_execution_changes()` before selecting transactions, once per simulated block. Use the Task 4 shared executor so the canonical contracts and fee context are applied. Retain the existing legacy omission path's simulation semantics. Gate `reserve_anchor_zk_gas_for_tx_pool_selection` on `!is_tbd_active(target_timestamp)`; preserve the response and multi-list selection algorithm.

```rust
if !chain_spec.is_tbd_active(context.timestamp) {
    reserve_anchor_zk_gas_for_tx_pool_selection(&mut executor)?;
}
```

- [x] Reuse `BENCH_NEAR_LIMIT_TARGET` from block testutil to show a near-limit transaction is selected with post-fork context but excluded by the legacy anchor reserve. Add a real-contract fixture asserting simulation can read the newly recorded root. Run all auth tests and commit `feat(rpc): add target context to tx-pool preselection`.

### Task 8: Verify live Engine round trips, real state, and cross-fork reorgs

**Files:**
- Create: `crates/node/tests/tbd_engine.rs`, `crates/node/tests/support/mod.rs`.
- Create: `crates/node/tests/fixtures/tbd-genesis.json`, `crates/node/tests/fixtures/tbd-cases.json`.
- Modify: `Cargo.toml`, `Cargo.lock`, `crates/node/Cargo.toml` for test-only dependencies; existing workspace pins remain unchanged.
- Extend: block/prover/debug tests from Task 4 when state/witness comparisons need their local fixtures.

**Interfaces:**
- Add workspace `reth-e2e-test-utils` at the exact existing Reth revision, used only as a node dev-dependency for `NodeTestContext` and `NodeHelperType<TaikoNode>`. Do not use its generic `setup`: that helper requires `From<EthereumPayloadAttributes>` and `From<ChainSpec>`, which the Taiko types do not implement. Do not add production conversions solely to satisfy a test helper. Add existing workspace Alloy Engine, jsonrpsee client, serde_json, signing, node-core, RPC server types, and task dependencies as node dev-dependencies; enable block `test-utils`/`prover` and node-builder/database `test-utils`.
- Test support functions: `fixture_chain_spec() -> Arc<TaikoChainSpec>`, `fixture_attributes(timestamp: u64) -> TaikoPayloadAttributes`, `normalize_v5(envelope: ExecutionPayloadEnvelopeV5) -> serde_json::Value`, and `launch_test_node(chain_spec: Arc<TaikoChainSpec>, runtime: reth_tasks::Runtime) -> impl Future<Output = eyre::Result<NodeHelperType<TaikoNode>>>` (implemented as `async fn`).
- Define test-only `TaikoTables: TableSet` by chaining `reth_db::Tables::ALL` and `alethia_reth_db::model::Tables::ALL`, following `create_taiko_test_provider_factory_with_chain_spec` in auth tests. Initialize an `Arc<TempDatabase<DatabaseEnv>>` with `init_db_for::<PathBuf, TaikoTables>` so origin persistence exercises actual Taiko tables. Configure a unique temporary datadir, unused RPC/network ports, disabled discovery, no automatic mining, and the small test tree cache before launch.
- `fixture_chain_spec` parses the test-only genesis, constructs fresh headers/hashes through the normal chain-spec builder, enables Unzen at genesis and TBD at timestamp 100, funds the existing deterministic test signer, and installs the exact canonical system-contract code. Never edit built-in genesis files.
- `fixture_attributes` uses the given timestamp in both fields, seven-byte extraData, no anchor, root `B256::with_last_byte(1)` at/after 100 and zero before, and an explicit RLP transaction list. `tbd-cases.json` supplies deterministic signed raw transactions and expected normalized request/response shapes.
- `normalize_v5` serializes `envelope.execution_payload`, converts `envelope.block_value` with checked `u64::try_from`, and inserts a numeric `headerDifficulty`, including zero. The root remains a separately retained per-job value.

- [x] Add the live test harness using the pinned node builder. Within `launch_test_node`, `config` is the configured `NodeConfig<TaikoChainSpec>`, `database` is the temporary database above, and `runtime` is its argument:

```rust
let taiko = TaikoNode;
let handle = NodeBuilder::new(config)
    .with_database(database)
    .with_launch_context(runtime)
    .with_types_and_provider::<TaikoNode, BlockchainProvider<_>>()
    .with_components(taiko.components_builder())
    .with_add_ons(taiko.add_ons())
    .launch_with_fn(|builder| {
        let launcher = EngineNodeLauncher::new(
            builder.task_executor().clone(),
            builder.config().datadir(),
            TreeConfig::default().with_cross_block_cache_size(1024 * 1024),
        );
        builder.launch_with(launcher)
    })
    .await?;
NodeTestContext::new(handle.node, fixture_attributes).await
```

Launch two independent nodes with `Runtime::test()` and the same fixture spec. Use each node's authenticated RPC handle/client and initialize forkchoice to its genesis through null-attributes FCU. Drive FCU/get/new through JSON-RPC requests, not `advance_block()` shortcuts that select upstream Ethereum method versions. Keep safe/finalized at the genesis ancestor until reorg tests finish. Bound async waits with `tokio::time::timeout` so a missing build cannot hang CI.
- [x] Add a test that builds via FCU V3/get V5 on node A and imports via the exact four new V4 arguments on node B. Assert `VALID`, same block hash/state root/receipts/difficulty, and correct root storage on B after FCU. Include a nonzero-zk-gas ordinary transaction and an empty block.

```rust
let root = B256::with_last_byte(1); // retain the original FCU job root
let payload = normalize_v5(envelope);
assert!(payload["headerDifficulty"].is_number());
let params = jsonrpsee::rpc_params![payload, Vec::<B256>::new(), root, Vec::<Bytes>::new()];
let status: PayloadStatus = client.request("engine_newPayloadV4", params).await?;
assert!(status.status.is_valid());
```

- [x] Add boundary cases: build/import a legacy block at 99 through V2, a TBD child at 100 through the new family, then reorg to an alternative legacy head and build forward again. Hold an old payload ID across the head changes and verify retrieval routing remains tied to its timestamp. Run null-attributes FCU through both versions. Verify state rollback removes the orphaned 4788 record and restores canonical 2935 history; preserve origin/proposal/preconfirmation behavior.
- [x] Add a malicious roundtrip where the payload declares a different difficulty and has its block hash recomputed accordingly. It must fail execution's zk-gas comparison, proving the hash check alone is insufficient. Add a different nonzero root without recomputing block hash to pin hash preservation. Send a valid block through direct `block_to_payload`/tree input and compare with the RPC path; retain unsupported-field sentinels.
- [x] Run `cargo test -p alethia-reth-node --test tbd_engine --all-features`; these integration cases must fail on missing behavior before fixes. Fix only gaps in the responsible earlier task's implementation, retaining its regression tests.
- [x] Compare build/import/prover/debug witness results on the same real-contract fixture: exact state/receipt roots, finalized zk-gas, 4788/2935 storage, and code/storage witness availability. For a full-block trace verify ordinary fees for golden-touch and a checkpoint-reveal-shaped call. Run first-failure derivation/canonical-import pairs from Task 4 and ensure system writes survive an empty filtered result.
- [x] Add an activation base-fee regression using the actual pre-TBD parent's gas limit and gas used. Assert EIP-4396 uses those values unchanged; do not remove an anchor reserve from the parent header. Preserve golden historical snapshots of V2 block hashes, receipts, fee balances, and zk-gas across existing fork fixtures.
- [x] Store deterministic activation, normal, empty, forced/default, and reorg cases in `tbd-cases.json` with parent headers, input tx lists, exact roots, requests, and expected commitments. Use a test to load and verify them; do not hand-enter unverified hashes. These are the cross-client handoff vectors, not evidence that another client has already passed.
- [x] Run the integration command plus block/prover/debug targeted tests; commit `test(engine): cover TBD round trips and fork reorgs`.

### Task 9: Document the wire contract and complete repository checks

**Files:**
- Create: `docs/engine-api-tbd.md`.
- Modify: `README.md` with a short link to the fork/API guide.
- Update: this plan's checkboxes and the approved design's implementation status only after the corresponding work is actually done.

**Interfaces:** No new production interfaces. The document is the handoff contract for drivers and release coordinators.

- [x] Document the six-method fork matrix, target timestamp routing, null FCU handling, required root provenance, decimal `headerDifficulty`, hex `blockValue`, four positional newPayload arguments, empty arrays, and retained per-job root. Show the checked normalization algorithm, including explicit zero:

```text
root := the original root associated with this FCU payload ID
zkGas := parse getPayloadV5.blockValue as an unsigned integer
require zkGas fits the existing zk-gas limit and decimal u64 representation
payload := getPayloadV5.executionPayload
payload.headerDifficulty := zkGas as a decimal JSON number, including 0
newPayloadV4(payload, [], root, [])
```

- [x] Document `--devnet-tbd-timestamp` / `ALETHIA_RETH_DEVNET_TBD_TIMESTAMP`, omitted versus zero behavior, the Unzen ordering requirement, and both taikoAuth trailing contexts. State explicitly that generic Ethereum clients expect revenue in blockValue and cannot assume this Taiko endpoint has Ethereum semantics. Internal builder ranking remains fee-based.
- [x] Include an unchecked activation checklist covering geth EIP-4788 parity, deployed 2935/4788 contracts, disabling privileged Anchor writes by the first new block, both drivers' normalization/context support, guest verification of L1 hash and legacy parent inheritance, permissionless reveal consumers, and successful consumption of Task 8 cross-client vectors. Keep all network activation conditions at Never. Existing-chain genesis must not be rewritten.
- [x] Run `just fmt`. Inspect the diff for unrelated formatting; retain only necessary changes. Run `just clippy` (mandatory documentation gate) and `just test` (workspace/all features). Address failures and rerun affected checks before broadening again. Report infrastructure failures accurately if tools/dependencies cannot be obtained; never substitute an unrun command with a passing claim.
- [x] Run `git diff --check` and review all modified production symbols for purpose/contract docs, including trait implementation methods. Check the final diff for accidental Amsterdam support, changed legacy fixture hashes, activation timestamps, or dependency upgrades.
- [x] Commit documentation and any gate fixes with focused Conventional Commits, including `docs(engine): document the TBD driver wire contract`. Request whole-branch review using the execution method selected by the user; do not schedule activation or modify external repositories.

## Coverage and completion record

| Approved design requirement | Owning tasks |
| --- | --- |
| Fork naming, Never defaults, devnet override/order, Osaka and Unzen schedule | 1 |
| Root/difficulty preservation, mandatory full body, genesis exception | 2, 4, 6 |
| Fee sharing without anchor privileges, standalone replay safety | 3, 4, 8 |
| Standard system calls and real storage/witness parity | 4, 7, 8 |
| Ordinary first transaction, empty blocks, filtering vs canonical import | 4, 5, 8 |
| Payload IDs, gas reserve removal, fee-based ranking | 5, 6, 7 |
| All six API methods, routing/errors, persistence/authentication | 6, 8 |
| Target-aware taikoAuth and serialization compatibility | 7 |
| Historical replay, activation, reorg, EIP-4396 parent semantics | 8 |
| External release dependencies and cross-client acceptance | 8, 9 |
| Required formatting, documentation, lint and test gates | 9 |

Planning self-review: checked every spec section against the table; checked the five Review Focus conditions against their owning test tasks; checked interface names and dependency order; reserved `TBD` exclusively as the agreed fork identifier. No implementation or test pass is claimed by this document. Network readiness additionally requires the external activation checklist and cross-client/prover agreement.
