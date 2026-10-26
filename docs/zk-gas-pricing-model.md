# zkGas Shadow Pricing Model

## Status and scope

This document describes the current SP1 Prover Gas Unit (PGU) Shadow pricing export compiled into
alethia-reth. The runtime artifact has model identity
`93d04eddda3779393807808859eb36561d35aad0cdd8a8a3960ec3e279a93046` and was introduced by
[alethia-reth PR #253](https://github.com/taikoxyz/alethia-reth/pull/253). If
`RUNTIME_SPEC_SHA256` differs from that value, this document is stale.

The artifact calls itself `accepted_runtime_export`. In this document, that means only that the
raiko2 calibration pipeline selected it for this runtime integration. It is not a protocol
specification, governance approval, admission rule, or consensus rule. The words
`accepted_absolute` and `Composition::Accepted` below are evaluator identifiers, not additional
approval claims.

> **Not the Unzen zk gas limit.** This PGU model is separate from the consensus Unzen zk gas meter
> in `crates/evm/src/zk_gas/{schedule,unzen,meter}.rs`, specified in
> [taiko-mono's zk gas specification](https://github.com/taikoxyz/taiko-mono/blob/main/packages/protocol/docs/zk_gas_spec.md).
> The Unzen meter enforces `BLOCK_ZK_GAS_LIMIT` where Unzen is active and commits its total to
> `header.difficulty`, regardless of `--zkgas-mode`. `--zkgas-mode` controls only this Shadow PGU
> model. PGU and Unzen zk gas are different units and must not be compared numerically.

`Disabled` remains the default runtime mode. `Shadow` collects EVM-local selectors and evaluates an
incomplete subtotal without rejecting transactions or blocks and without replacing the Unzen
meter, block limit, or header-difficulty commitment.

### What is live

| Surface | Current status |
| --- | --- |
| EVM-local collection | Live in `Shadow` for executions that use `TaikoBlockExecutor`. |
| Runtime log | One `tracing` event per successful executor finish, target `zkgas`, for the committed prefix. `Incomplete` carries the model ID, status, EVM-local subtotal, and fixed/operation/local-resource components; `Diagnostic` carries only the model ID and coarse status. |
| Prover outcomes | With the `prover` feature, `CommittedBlockExecutionOutcome` and `DerivedBlockExecutionOutcome` return `zk_gas_shadow_report`, `attempted_zk_gas_shadow_report`, and `attempted_transaction_count`. The reports are EVM-local and incomplete, not complete block quotes. |
| Exact block completion | Public evaluator API and tests exist, but this repository has no production caller. |
| Proposal-once pricing | Public evaluator API and tests exist, but this repository has no production caller. |
| Exact resources, block/parent MPT, and proposal selectors | No production producer is wired in this repository, and no published caller is identified by the artifact or documentation. |
| Admission | Not implemented. There is no `Admission` mode. |

The live log does not contain a block number or hash, exposes no metric, and does not include the
underlying typed error behind a coarse diagnostic status. Payload-build jobs and supported RPC
simulations can therefore be interleaved with canonical execution. The log covers only the committed
prefix and is skipped if Unzen difficulty validation fails before the executor records it. The prover
outcomes expose both prefixes to callers, but neither surface is a complete block quote or a
ready-made SP1 comparison feed.

## Sources of truth

This document is the human-readable guide. The checked-in sources divide authority as follows:

- [`runtime-spec.json`](../crates/evm/src/zk_gas/pricing/runtime-spec.json) carries the current
  coefficients, route records, embedded provenance, declared domains, and recorded validation.
- [`generated.rs`](../crates/evm/src/zk_gas/pricing/generated.rs) is generated from that JSON and
  must not be edited by hand.
- [`model.rs`](../crates/evm/src/zk_gas/pricing/model.rs) defines executable evaluation,
  composition, validation, and some hard-coded domain rules.
- [`types.rs`](../crates/evm/src/zk_gas/pricing/types.rs) defines typed selectors, results, errors,
  and the hard-coded selector bound.
- [`mode.rs`](../crates/evm/src/zk_gas/pricing/mode.rs) defines Shadow report and completion types.
- [`accumulator.rs`](../crates/evm/src/zk_gas/pricing/accumulator.rs) defines transactional selector
  collection, committed and attempted prefixes, EVM-local unique-resource accounting, and the
  attempt-boundary contract on `ZkGasAttemptedBlockSnapshot`.
- [`executor.rs`](../crates/block/src/executor.rs) and
  [`derived_block.rs`](../crates/block/src/derived_block.rs) own the runtime log and prover outcome
  fields that expose those prefixes.
- [`runtime.rs`](../crates/evm/src/zk_gas/runtime.rs),
  [`alloy.rs`](../crates/evm/src/alloy.rs), [`evm.rs`](../crates/evm/src/evm.rs), and
  [`adapter.rs`](../crates/evm/src/zk_gas/adapter.rs) define extraction, Anchor detection,
  resource observation, spawn confirmation, and inspector integration. `runtime.rs` also contains
  parts of the separate consensus Unzen meter, as do `adapter.rs`, `alloy.rs`, and `evm.rs`; edits to
  those files can therefore affect consensus metering too.
- [`generate.py`](../crates/evm/src/zk_gas/pricing/generate.py) verifies the artifact's address and
  generates the Rust route table. It does not recreate coefficients from calibration measurements.

If prose conflicts with code, the executable evaluator and extractor control current behavior. The
JSON content address covers the canonical JSON fields; it does not cryptographically bind evaluator
constants, extraction rules, or ownership code. In particular, the blob/source domain and selector
bound are currently duplicated in Rust rather than generated from the JSON. Their agreement relies
on review: current tests do not compare those Rust definitions with the JSON fields. The artifact
hash alone does not enforce that agreement.

## Objective and non-goals

The Shadow model is a host-computable estimate of SP1 work. Its intended future use is to identify
execution that is cheap under Ethereum gas but expensive for the configured guest, so an admission
policy can bound prover work without first running the prover.

The current design aims for:

- deterministic integer evaluation for identical committed execution;
- explicit ownership of committed operation, state, trie, payload, block, and proposal work;
- conservative tariffs or typed failure outside a calibrated route;
- transactional collection, so discarded candidates remain in an attempted prefix without
  polluting the committed prefix; and
- conservative safety taking priority over mean prediction error.

The current export was calibrated against normalized SP1 `ExecutionReport::gas()` values, called
PGU here. The artifact embeds a production proposal ELF hash and VK hash, but it does not identify a
public raiko2 release or tag for that guest. It therefore cannot establish from public material that
a currently deployed guest still matches. Resolving that provenance is required before promotion
to admission.

The model does not claim to price RISC0 cycles, wall-clock proof latency, or another SP1 guest. A
different backend, guest, cost-bearing trace schema, or coefficient set requires independent
calibration and a new model identity.

The current implementation deliberately does **not**:

- replace or modify the Unzen zk gas schedule;
- choose or enforce a new per-block limit;
- enforce a cumulative proposal budget;
- guarantee a wall-clock proof deadline;
- expose an `Admission` mode;
- enforce an admission policy over executed-then-discarded candidate work; or
- produce the exact external inputs required for complete block or proposal pricing.

## Model identity, arithmetic, and rounding

The artifact distinguishes the cost-bearing trace-schema version from the JSON runtime-export
schema version. Refer to `execution_identity.trace_schema_version` and top-level `schema_version`
instead of copying their current numbers into integrations.

`artifact_sha256` is SHA-256 over sorted-key compact JSON with the self-referential
`artifact_sha256` field removed. Reports carry this address so completion cannot silently combine
different JSON artifacts. The address does not cover the surrounding Rust semantics described in
the source-of-truth section.

Coefficients are fixed-point PGU values using the artifact's `arithmetic.scale`. The exporter has
already rounded coefficient numerators upward. Evaluation uses checked signed arithmetic; selector
bounds and scope term limits come from the artifact and Rust constants. Invalid selectors,
excessive term counts, overflow, and negative composed components are typed errors.

Some individual fitted coefficients are intentionally negative, including parts of EXP,
zero-length KECCAK256, and cold SLOAD/SSTORE. EXP and zero-length KECCAK256 use within-route fitted
offsets; the cold-access offsets may reflect cost also carried by unique-resource tariffs. Do not
“fix” them to zero; the evaluator requires the composed result, not every fitted coefficient, to be
nonnegative.

The live EVM-local subtotal, complete block result, and proposal-once result each expose their own
integer ceiling. For a complete proposal comparison, callers must combine scaled values and apply a
single final ceiling:

```text
proposal_scaled = proposal_once.total_scaled + sum(complete_block[i].total_scaled)
proposal_pgu = ceil(proposal_scaled / scale)
```

Do not sum `total_pgu` from blocks and `price_proposal_once().total_pgu`; those values were rounded
independently. No in-repository helper currently performs the proposal composition above.

## Scope decomposition and ownership

The complete block-local fixed-point price is:

```text
block_price =
    block_base
  + committed_non_anchor_transaction_tariff * committed_non_anchor_transactions
  + native_value_transfer_tariff * native_value_transfers
  + operation_price
  + exact_unique_resource_price
  + exact_block_mpt_price
```

The Rust field and artifact still use the historical name `started_non_anchor_transactions`. It
counts non-Anchor transactions in its prefix: included/committed transactions in
`ZkGasBlockSnapshot` returned by `committed_snapshot`, and attempted transactions in
`ZkGasAttemptedBlockSnapshot.block`. The same fixed-term aggregation is evaluated independently for
each prefix; the formula above describes the committed block price.

The proposal-once fixed-point price is:

```text
proposal_once_price =
    proposal_startup
  + canonical_transaction_byte_tariff * canonical_transaction_bytes
  + nonempty_payload_transaction_tariff * nonempty_payload_transactions
  + true_unattempted_transaction_tariff * true_unattempted_transactions
  + extra_blob_tariff * extra_blob_count
  + extra_source_tariff * extra_source_count
  + exact_parent_mpt_price
```

Coefficient values and exhaustive route membership live in `runtime-spec.json` and generated Rust.
This document names only behavior needed to use the evaluator correctly.

### Anchor ownership

The Anchor transaction is not a separate operation-pricing domain. Its executed ordinary opcodes,
precompiles, state access, and confirmed spawn wrappers use the corresponding normal routes. A
spawning CALL/CREATE-family step is the exception described below: the confirmed wrapper owns that
step, rather than an ordinary opcode route owning it.

The Anchor does not receive the committed non-Anchor transaction tariff. Its envelope and protocol
orchestration are covered by the block fixed term. This avoids a second whole-Anchor estimate while
retaining the execution routes that the collector actually emits.

### Transactional ownership

Selectors are staged in an in-flight transaction scope. Committing merges that scope into both the
committed and attempted prefixes. Resetting a rejected or discarded candidate retains it only in the
attempted prefix, preserving the previously committed prefix. An included transaction that halts or
reverts is committed by the executor and retains its observed work in both prefixes.

The attempt boundary is the start of the EVM pricing scope in `transact_raw`, before revm
transaction/header validation. Invalid nonce or insufficient balance can therefore contribute an
attempt, including declared access-list resources, even without executing an opcode. Discarded
execution, such as a commit-predicate rejection or Unzen zk gas truncation, is retained through
`zk_gas_attempted_pricing_snapshot` and `attempted_zk_gas_shadow_report`.

Neither prefix covers executor precheck rejections for remaining native block gas or intrinsic zk
gas, the interpreter opcode step whose meter charge first exceeds the zk gas budget, or transactions
skipped without execution (zero-signer non-Anchor transactions and transactions after zk gas
exhaustion). For a truncated attempt, completed steps and resources from the finalized journal are
retained. Completed precompile bodies remain observed even when their meter charge triggers
truncation; resetting the rejected transaction retains that work in the attempted prefix.

`attempted_transaction_count` includes the Anchor and counts only scopes that began; it is zero when
Shadow is disabled or its collector is unavailable. Subtracting it from a transaction-list length
does not define `true_unattempted_transactions`: precheck rejections and skipped transactions need
an explicit producer policy. The attempted report includes committed work, so do not add the two
reports together. Future admission must choose a prefix and account for its residual gaps
consistently with the proving workload; observing attempted work does not implement that policy.

## Operation routes and composition

Authenticated routes use three namespaces:

- `opcode:0xNN` for supported ordinary interpreter steps;
- `precompile:0xNN` for direct precompile bodies; and
- `opcode:0xNN:confirmed_spawn_wrapper` for confirmed CALL/CREATE-family child-frame dispatch.

The JSON's `block_local.operation_routes` array is the exhaustive route and model-family map. The
route accumulator has four candidate fields, but one event does not generally emit all four:

- a normally modeled opcode emits its `accepted_absolute` and any configured `incremental`
  candidate;
- a supported opcode whose typed extraction fails, that halts exceptionally, or whose REVERT has
  nonzero raw gas emits `feature_error` instead of its normal candidates;
- `structural` floors exist on precompile routes, which do not have `feature_error`; and
- undefined opcode bytes have no route at all and turn the block report into a
  `collector_unknown_route` diagnostic.

Candidates accumulate by route across the committed block, then use the route's composition. The
same aggregation rules apply independently to the attempted prefix:

```text
Composition::Accepted:
    route_normal = accepted_absolute

Composition::Maximum:
    route_normal = max(accepted_absolute, incremental, structural)

route_total = route_normal + feature_error
operation_price = sum(route_total for every committed route)
```

The maximum is applied after route-wide accumulation, not per event. `feature_error` remains a
separate accumulated field, although a single opcode event selects either its normal extraction or
its feature-error path.

### Model families and current exceptions

The evaluator implements fixed, fixed-with-memory, linear/floored-linear, static-raw-gas,
raw-gas-affine, EXP, KECCAK256, memory-access, memory-copy, copy, LOG-piecewise, RETURN, storage, and
declared-context families. Integrations must use the route's `accepted_absolute_model.kind` (and,
when it is `opcode_model`, `accepted_absolute_model.model_kind`) and parameters rather than
inferring a model from the opcode name.

Important current exceptions are:

- `opcode:0x39` (CODECOPY) uses a flat aggressive tariff inside its raw-gas envelope and switches
  to a raw-gas fallback above it. The transition is not monotone.
- Only LOG1 and LOG2 use the LOG-piecewise family. LOG0 is a raw-gas linear floor; LOG3 and LOG4 use
  a fixed tariff plus shared memory.
- `max_constant_and_linear` precompile routes take the maximum of the constant and native-gas
  linear terms; they do not add them. The route's structural and incremental candidates are then
  composed according to `Composition::Maximum`. Here native precompile gas is the forwarded gas
  limit minus the gas remaining in the outcome; a failing precompile that consumes its allowance
  is priced on that full forwarded allowance.
- RETURN's out-of-envelope raw-gas coefficient is currently zero. Its repeated-boundary branch
  multiplies a chunk body that includes the whole event's memory price, so memory work is repeated
  for every conservative chunk.

Routes of kind `fixed` or `fixed_with_optional_shared_memory` have a flat per-event body, and several
precompile routes have large constant floors. Current round conservative tariffs that can dominate
Shadow subtotals include:

| Route | Conservative body or floor (PGU per event) |
| --- | --- |
| `opcode:0x32` (ORIGIN), `opcode:0x38` (CODESIZE), `opcode:0xfd` (normal REVERT), `opcode:0x39` (CODECOPY within its raw-gas envelope) | 5,000,000 |
| `opcode:0xa3`, `opcode:0xa4` (LOG3/LOG4) | 5,000,000 plus shared memory |
| `opcode:0x3b` (EXTCODESIZE) | 1,000,000 |
| `opcode:0xf1:confirmed_spawn_wrapper` (CALL) | 1,000,000 |
| `opcode:0xfa:confirmed_spawn_wrapper` (STATICCALL) | 5,000,000 |
| `precompile:0x01`, `0x02`, `0x06`, `0x07`, `0x08` | 20,000,000 constant floor in `max_constant_and_linear` |
| Every opcode `feature_error_absolute_model` | `max(5,000,000, coefficient * interpreter_raw_gas)` |

These are conservative tariff choices, not claims of equal measured cost. In particular, the
1,000,000 PGU CALL-wrapper and EXTCODESIZE bodies are tariff placeholders, not fitted constants;
their separately composed incremental candidates are distinct from those flat bodies. The table
does not replace route composition: other candidates can raise a route's total, and the exceptional
halt/REVERT rule below can select `feature_error` instead of the normal body. Read current values
from the artifact when integrating, rather than treating this illustrative list as an exhaustive
route map.

The shared memory selectors are per-operation values. Rust `MemorySelectors.growth_events` and
`boundary_4k_events` are zero/one flags, not page counts; the first 4 KiB page does not count toward
the latter. They correspond to the artifact coefficients `memory_growth_event` and
`memory_4k_boundary_event`. Rust `evm_gas_delta` is the EVM memory expansion delta and corresponds to
`memory_evm_gas_delta`. The evaluator in `model.rs` validates byte/word and raw-gas relationships;
a contradiction is invalid input rather than a zero-cost event.

Storage models classify SLOAD by warm/cold access and SSTORE by warm/cold access plus its semantic
branch. Declared-context models include ADDRESS/CALLER, CALLVALUE, CALLDATALOAD, CALLDATASIZE, and
TIMESTAMP classes. The artifact is authoritative for the exact per-route assignment.
TIMESTAMP accepts only the nonzero class; a zero value raises `InvalidInput("timestamp_value_class")`
and becomes a collector diagnostic.

### Spawn wrappers

When a CALL/CREATE-family instruction selects a new frame, the collector returns before recording
the ordinary opcode route, raw gas, or shared memory. If frame dispatch confirms the spawn, only the
confirmed-wrapper route prices that spawning step. Child operations are priced normally and are not
multiplied by forwarded gas.

Confirmation occurs before later depth or balance checks in the dispatched frame. A selected spawn
that is never dispatched is not charged. A call into a precompile charges both the confirmed wrapper
and the precompile body. Consumers must not add the ordinary CALL/CREATE opcode or assume its buffer
growth was captured by the shared-memory model.

## Block fixed, resource, and trie terms

### Fixed block terms

The block fixed vector owns block/Anchor orchestration, one tariff per committed non-Anchor
transaction, and eligible native value transfers that do not enter executable code or a precompile.
The block base is not an opcode estimate for the Anchor.
An eligible native transfer is a successful non-Anchor `Call` transaction with nonzero value and no
observed opcode, spawn, or precompile event.

### EVM-local and exact resources

Resource selectors are unique sets or byte totals across a prefix, not per-transaction sums. The
committed and attempted prefixes deduplicate their own resources independently. The EVM-local
collector's semantics are deliberately broader than net state changes:

- accounts include transaction callers and call targets, access-list accounts, the beneficiary,
  treasury, finalized-journal state, and accounts selected by relevant opcodes;
- storage-access sets include access-list keys even when never loaded and every observed SLOAD or
  SSTORE location;
- an observed SSTORE contributes an access and a write even for a no-op or an instruction that
  halts, because storage observation is not gated on the instruction result; and
- code selectors track unique nonempty code hashes and total bytes for those hashes, including
  executing frame bytecode (also CREATE initcode) and code from finalized-journal state (also
  EIP-7702 delegation designators), not just deployed contract bytecode.

Journal resources are observed before an execution error is propagated. A rejected run therefore
retains its observed journal accounts, code and slots in the attempted prefix only; the observation
is not restricted to successful execution results.

An exact producer must start with these collector semantics and add any relevant pre-block or
outside-EVM loads. Defining "written" as net-changed slots or "accessed" as successful loads is
incompatible. Completion rejects any exact resource dimension below the EVM-local value, then
**replaces** the provisional local resource contribution with the exact contribution; it never adds
both.

No production exact-resource producer currently exists. Consequently these rules are a lower-bound
compatibility contract, not evidence that complete block quotes are live.

### Merkle Patricia Trie work

Block and proposal-parent Merkle Patricia Trie (MPT) inputs contain path-node visits,
materialization work units, hydration hash blocks, dirty encoded nodes, dirty hash blocks,
structural branch/extension churn nodes, and dropped trie nodes. Each scope is a dot product with
its tariff vector. Block MPT is an absolute snapshot for the canonically aggregated block state;
proposal-parent MPT is proposal-once.

This repository defines the fields and evaluator but not operational producers for them. It does
not specify such details as the exact materialization unit, hash-block width, or inclusion of storage
tries. Those definitions must be supplied and reviewed with the producer before completion can be a
portable contract; callers must not invent per-transaction path-depth approximations.

## Proposal payload and blob/source terms

`price_proposal_once` takes a count of canonical signed EIP-2718 encoded bytes
(`canonical_transaction_bytes`), a nonempty-payload transaction count, and a true-unattempted
transaction count. No in-repository production producer or complete operational definition for
these selectors is currently published, including treatment of undecodable or zero-signer
transactions. They are API inputs, not live observations.

The blob/source evaluator accepts the artifact's BPO2 topology; “BPO2” is an unexpanded internal
label in the current source. The exact source/blob domain is declared in
`proposal_once.blob_source_domain` and independently hard-coded in `validate_blob_source_domain`.
The base topology has no separate increment; `extra_blob_count = sum(source_blob_counts) - 1` and
`extra_source_count = len(source_blob_counts) - 1` are charged. Inputs outside the hard-coded domain
are rejected rather than extrapolated.

## Conservative and failure behavior

The calibration uses measured models where available, conservative envelopes where dimensions are
understood, and aggressive tariffs or typed failure for unresolved work. The aggressive routes
listed above are intentional compatibility prices, not evidence of equal measured cost.

At runtime:

- a supported executed opcode whose typed inputs cannot be extracted, that halts exceptionally
  (for example out-of-gas, stack underflow, invalid jump, INVALID, or a static-context violation), or
  whose REVERT has nonzero raw gas uses its route's authenticated `feature_error` model, currently
  `max(5,000,000 PGU, coefficient * interpreter_raw_gas)` per event, not a sum of the floor and
  raw-gas term; a plain zero-raw-gas REVERT uses its normal route;
- unknown routes, including undefined opcode bytes, produce a collector diagnostic rather than a
  feature-error price;
- missing, contradictory, or out-of-range selectors are typed errors;
- arithmetic overflow and negative composed components are errors; and
- every `Diagnostic`, every mismatched model ID, and every unexpected incompleteness marker causes
  completion to fail.

Because the only modes are `Disabled` and non-enforcing `Shadow`, these failures do not reject EVM
execution. They produce bounded-cardinality Shadow diagnostics. Any future admission mode must
define fail-closed behavior before enforcing a limit.

## Shadow runtime contract

Runtime mode is selected with either:

```text
--zkgas-mode disabled|shadow
ALETHIA_RETH_ZKGAS_MODE=disabled|shadow
```

`Disabled` installs no new selector collector. `Shadow` enables the collector only through
`TaikoBlockExecutor`. Covered paths include sync, payload building, `eth_simulateV1`,
`debug_executionWitness`, and taikoAuth transaction-list execution. It does not cover standalone
`eth_call`, `eth_estimateGas`, or `debug_trace*` paths that bypass that executor.
Transaction-replay helpers can also execute preceding transactions through a block executor without
calling `finish`, paying collection overhead without emitting its finish-time log.

Shadow never changes receipts, gas, state, assembled headers, Unzen validation, or Unzen zk gas
admission. It can add substantial CPU overhead:
[#253's review benchmarks](https://github.com/taikoxyz/alethia-reth/pull/253#discussion_r4310322230)
measured roughly 6–17× on opcode-heavy workloads, not a general overhead bound. Because collection
also runs during payload building and taikoAuth transaction-list execution, it is intended for
diagnostics on non-producing nodes, not default operation. Measure before enabling it on a
block-producing or preconfirming node. The collector is not gated on Unzen activation and can
observe pre-Unzen execution without changing the canonical outcome. Separately,
`DerivedBlockExecutionOutcome.hashed_state` is sufficient only for Unzen-and-later state-root
computation, as documented on that field; Shadow does not widen that prover-helper contract.

The live executor logs the committed prefix as either an `Incomplete` EVM-local subtotal or a coarse
`Diagnostic`. With the `prover` feature, the two outcome structs also expose committed and attempted
reports plus the attempted scope count; each report can independently be `Incomplete` or
`Diagnostic`. `complete_block_shadow_report`, `price_block`, and `price_proposal_once` are evaluator
APIs used by tests; no in-repository production path calls them. Completing the committed block would
authenticate the model and subtotal, reject every diagnostic, validate exact-resource coverage, and
price exact resources and block MPT. An attempted report is a separate observation, not an exact
quote for the committed state. Proposal pricing remains a separate call whose `total_scaled` must be
composed with complete block scaled values by an external owner.

## Calibration and evidence

The JSON embeds relative source paths, file hashes, authority hashes, guest hashes, and recorded
validation. The source paths do not identify a fetchable repository and commit, and the named
calibration files and exporter are not vendored here. The guest hashes likewise do not map to a
named public raiko2 release in the artifact or this repository's documentation.

Alethia-reth vendors the runtime JSON, the JSON-to-Rust generator, and the executable evaluator. The
checked-in commands can verify the JSON's self-address and reproduce `generated.rs` from that JSON.
They cannot recreate the runtime JSON or its coefficients from raw measurements. Public
re-derivation is therefore impossible with the material currently linked here.

The artifact's `validation` object is the source for recorded replay counts and underquote results.
Its historical-block count is consistent with expanding the same recorded proposal replays, not
evidence of an independent corpus. Those fields validate only the frozen evidence claimed by the
artifact; they are not a mathematical statement about future blocks, proof latency, or capacity.
A real SP1 comparison also requires exact/proposal producers and attributable block/proposal output
beyond this repository's EVM-local log and prover-outcome reports.

## Capacity and future admission

Model selection and limit selection are separate decisions. This model does not encode hardware
throughput, proposal interval, utilization reserve, queueing policy, or acceptable false-reject
rate. It exposes no per-transaction quote API, and its complete block and proposal APIs are not wired
to production inputs.

Before adding admission, a reviewed change must at least provide:

- versioned, operational definitions and producers for exact resources, block/parent MPT, and
  proposal selectors;
- a policy choosing committed or attempted observations, covering residual unobserved work or
  justifying its exclusion from the proving workload;
- attributable block/proposal telemetry suitable for comparison with current SP1 PGU;
- a public calibration derivation and named guest release, or an explicitly reviewed replacement;
- explicit capacity and limit assumptions;
- fail-closed handling for every typed diagnostic;
- candidate rollback/replacement semantics;
- historical compatibility and adversarial saturation results; and
- an activation and rollback plan that cannot reinterpret existing Unzen difficulty.

## Reproduction and change policy

Verify the checked JSON address and generated Rust table with:

```bash
python3 -m unittest crates/evm/src/zk_gas/pricing/generate_test.py
python3 crates/evm/src/zk_gas/pricing/generate.py --check
```

Regenerate `generated.rs` from an already-reviewed `runtime-spec.json` with:

```bash
python3 crates/evm/src/zk_gas/pricing/generate.py
just fmt
```

A new runtime JSON requires updating the expected hash in `generate.py`, the identity line at the
top of this document, and the pinned assertions in `crates/evm/src/zk_gas/tests.rs` (identity, scale,
and route count), then regenerating and validating the table. Changes to evaluator constants,
extraction, ownership, hard-coded domains, or selector bounds also change effective semantics even
when the JSON address does not change; they require explicit review, compatibility evidence, and a
deliberate identity/version decision.

Do not add a compatibility alias that lets reports from two model identities be completed as though
they were the same model. Documentation-only clarification may retain the identity only when it does
not change the artifact or executable semantics.
