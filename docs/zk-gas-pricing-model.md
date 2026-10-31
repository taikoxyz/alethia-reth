# zkGas Shadow Pricing Model

## Status and scope

This document describes the current SP1 Prover Gas Unit (PGU) Shadow pricing export compiled into
alethia-reth. The runtime artifact has model identity
`bbce9be0cd9792bfa9c46369203f6441c3fd288787ea357e311eae8ebd48aac4`. Relative to immutable parent
`2fb4bb71092f11cca2217e69b25e94217342ffb46a540a961e7a77a483cb5601`, it changes only the normal
`BLOCKHASH (0x40)` fixed price from **32.117684584888432025 to 700 PGU/event**.
All other routes, memory coefficients, floors, fixed costs, payload/resources, MPT tariffs, global
discount, authorization/access-list tariffs and code-preparation units remain unchanged. The inherited
DELEGATECALL wrapper is 6000 PGU/event; code units are 64 PGU per unique code byte and 8192 per accepted
nonempty preparation plus 32 per processed byte. This remains the initial Shadow model with no public
model selector. If `RUNTIME_SPEC_SHA256` differs from this value, this document is stale.

The original calibration calls itself `accepted_runtime_export`; the current artifact and its
immediate parent call themselves `conservative_runtime_successor`. These identify calibration and
runtime integration artifacts, not a protocol specification, governance approval, admission rule, or consensus rule. The words
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
| Runtime log | One `tracing` event per successful executor finish, target `zkgas`, for the committed prefix. `Incomplete` carries the model ID, status, EVM-local subtotal, and fixed/authorization/access-list/code-preparation/operation/local-resource components; `Diagnostic` carries only the model ID and coarse status. |
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
  exact successor replay, then generates the Rust route table.
- [`blockhash_successor.py`](../crates/evm/src/zk_gas/pricing/blockhash_successor.py)
  derives the current fixed BLOCKHASH unit from immutable model2fb and the sealed
  [twelve-row density evidence](../crates/evm/src/zk_gas/pricing/successors/blockhash-density-v1.json).
  Exact replay authenticates the parent, policy, measurement identities and fifteen matched constraints,
  and rejects every unrelated runtime or provenance change.
- [`delegatecall_successor.py`](../crates/evm/src/zk_gas/pricing/delegatecall_successor.py)
  derives the inherited fixed DELEGATECALL unit from immutable modela298 and the sealed
  [eight-row density evidence](../crates/evm/src/zk_gas/pricing/successors/delegatecall-wrapper-v1.json).
  Exact replay rejects changed coefficients, ownership, evidence, inherited authority fields or
  tooling provenance. Preparation derivation remains in `code_preparation_successor.py` and its
  92-row evidence, raw-item derivation in `access_list_successor.py`, authorization derivation in
  `authorization_successor.py`, CALL memory repair in `call_memory_successor.py`, and EXP derivation
  in `successor.py`. Those historical generator identities remain recorded in immutable snapshots;
  only the current BLOCKHASH successor is regenerated by the commands below.
- [`accepted-runtime-spec.sha256`](../crates/evm/src/zk_gas/pricing/accepted-runtime-spec.sha256)
  pins the accepted successor separately from the generators, avoiding a self-reference cycle.

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

The parent was calibrated against normalized SP1 `ExecutionReport::gas()` values, called PGU
here. The artifact retains the parent's production proposal ELF hash and VK hash, but does not identify a
public raiko2 release or tag for that guest. It therefore cannot establish from public material that
a currently deployed guest still matches. Resolving that provenance is required before promotion
to admission.

The refinement evidence and inherited MULMOD evidence use a different frozen local proposal ELF
and SDK, as detailed below. This does not establish parity with that parent calibration guest or a deployed production SDK.

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

### Arithmetic and MCOPY block-cost refinement

The [sealed evidence](../crates/evm/src/zk_gas/pricing/successors/block-cost-evidence-v1.json)
contains 127 canonical inputs across six routes, deduplicated within each route after validating
native counts, canonical inputs, public outputs, complete parent quotes and SP1 observations.
Two duplicate inputs retain the higher observed PGU. Previously reserved holdouts, including the
ADDMOD interior-160 case, are excluded. Four zero-word MCOPY controls are validation-only and never
enter a division or fit. This compact record binds six source-file digests per row (construction,
GuestInput, native report, parent quote, SP1 report and measurement status), the frozen SDK/core
executor 6.3.0, ELF, estimator and executor options. The generator separately pins its canonical
digest; changing a source hash or execution count requires review of a new evidence manifest.
The raw sources and complete witnesses remain external experiment material.

For arithmetic, `C` is the parent's fitted whole-event cost, including dispatch, `Q` is the
unrounded complete-proposal quote, `A` is actual PGU and `N` is the observed normal-event count.
The fit uses `max(C, C + (A - Q) / N)` across retained observations, then multiplies by `5/4` and
rounds upward to a 100-PGU grid using exact fractions. Normally executed DIV, SDIV, MOD, SMOD and
ADDMOD receive structural floors of **800, 900, 800, 800 and 1,100 PGU per event**, respectively.
The existing route-wide maximum retains their fitted accepted models; feature-error events retain
their separate fallback and receive no normal-event floor.

For MCOPY, `C` is the parent's accepted `copy_words` coefficient, and `N` is the total observed
copied words (`executed_primary_count * ceil(copy_length_bytes / 32)`). The same deficit calculation
and `5/4` headroom, rounded upward to an 8-PGU grid, produce **48 PGU per copied word**. Only that
parameter changes. Dispatch, constant and shared-memory expansion remain additive; zero words
still pay dispatch/constant and any observed memory charge. No alignment, direction, operand or
other new selector is introduced. All unrelated tariffs, MULMOD 1,600, payload ownership, Unzen,
default modes, limits and public APIs remain unchanged.

The parent complete quotes underpriced 51 of these observations. Replacing only the relevant
contribution projects complete-quote coverage for all 127, including zero-word controls. This is
a conservative empirical fit, not a bound for every operation mix or production guest. Node-known
subtotals remain incomplete and need not cover full SP1 observations. The local frozen ELF/SDK
identity does not establish parity with the inherited calibration ELF or production SDK.
Generation records projected fit coverage only; historical mainnet replay and prospective holdout
results must be reported separately and do not mutate these retained observation identities.

### Arithmetic/copy validation checkpoint (2026-10-28)

After sealing model `80459b4d…`, native re-quoting checked 298 retained inputs, including 297 with
accepted SP1 observations. All measured cases in the six refined routes received complete quotes
covering their observations. Overall complete underquotes fell from 56 to three rows representing
two distinct dense EXP cases (exponent lengths four and five), motivating the subsequent EXP
refinement below. Canonical inputs, public outputs, gas/Unzen scores and non-price
selectors were unchanged. These are complete-proposal coverage checks; node-known subtotals keep
their narrower scope.

Seven fresh prospective arithmetic and overlapping/unaligned MCOPY cases were measured only after
the fit was sealed. Their parent complete quotes all underpriced actual PGU; the candidate covered
all seven, with complete quote/actual ratios from 1.045025 to 1.136045. Primary fit counts describe
controlled user-program events. Repricing also applies to qualifying anchor/common execution,
which can add conservative cost beyond the primary-only fit projection.

Paired native replay of two disjoint historical mainnet cohorts covered 180 proposals and 34,560
blocks. Statistical medians below average the two central quote/actual ratios within each cohort;
weighted ratios divide total complete quote by total observed PGU.

| Mainnet cohort | Proposals / blocks | Median complete quote/actual, model0a3 → model80459 | Weighted complete quote/actual, model0a3 → model80459 | Maximum proposal price uplift | Complete underquotes, model0a3 → model80459 |
| --- | --- | --- | --- | --- | --- |
| October 19–22, 2026 | 90 / 17,280 | 2.006040 → 2.031883 | 2.085497 → 2.105347 | 1.450460% | 0 → 0 |
| October 23–26, 2026 | 90 / 17,280 | 2.102982 → 2.128748 | 2.144603 → 2.164265 | 1.447198% | 0 → 0 |

Both cohorts preserved public outputs, canonical input identities, gas/Unzen scores and non-price
selectors. Exact operation deltas propagated through block node/prove totals with the existing
rounding. Median and weighted increases remained below 5%, and every proposal increase remained
below 20%, preserving the empirical roughly-2x conservative mainnet conclusion in these cohorts.
Independent adversarial review and behavioral verification passed.

These results use raw native quotes paired with retained external actuals; generation itself does
not rerun SP1 or embed these replay outcomes. Historical actuals retain their original ELF/backend
identities. The fresh local observations use frozen ELF `416f5a47…` and SDK/core executor 6.3.0;
neither native re-quoting nor these local measurements establishes production SDK/ELF parity,
proof validity, or a universal PGU upper bound. No guest rebuild, activation or deployment occurred.

### EXP per-event cost refinement

The [EXP evidence](../crates/evm/src/zk_gas/pricing/successors/exp-evidence-v1.json) retains 29
canonical inputs with the same local ELF/SDK identity. It covers dense exponents of 3–16 bytes,
32-byte dense/sparse exponents, matched equal-popcount exponent shapes, and repetitions over
1, 4 and 16 blocks. All source identities, parent quotes, normal EXP counts and public outputs
were validated before sealing. Twenty-one complete parent quotes underpriced their observations;
the eight already-covered rows validate the candidate without constraining a higher floor.

For a normal EXP with typed exponent byte length `L` in `0..=32`, the accepted event price is:

```text
max(original accepted event price(L), 24 * (10 + 50 * L)) PGU
```

The original dispatch, small-bucket and quadratic coefficients remain unchanged. The gas-shaped
term uses the already-collected byte length, so it adds no input selector. It is a calibrated
conservative floor, not a claim that Ethereum gas captures every aspect of SP1 execution. The
frozen guest's exponent loop includes conditional multiplication and a byte-scanning zero test;
bit density and zero-run placement affect actual cost even at equal byte length.

The generator derives the coefficient with exact fractions. For each underquoted observation,
solve `R = (C + (A - Q) / N) / (10 + 50 * L)`, where `C` is the original accepted event price,
`A` is actual PGU, `Q` is the unrounded complete parent quote, and `N` is the observed normal EXP
count. Take the maximum `R`, multiply by `5/4`, and round upward to an integer: **24 PGU per
canonical EXP gas unit**. Already-covered rows impose no new floor because their original event
curve is retained. Replacing only the EXP contribution projects complete coverage of all 29.
Fresh holdouts and native historical replay qualify the implementation separately from this fit.

Independent native replay checked all 29 fit/control inputs, six fresh shape/mixed-length
holdouts excluded from fitting, and 13 retained EXP observations. All 48 complete quotes cover
their saved actuals; the fresh six have complete quote/actual ratios 1.1597–1.2361. Every block's
price delta equals the sum of its event-wise EXP increases, with non-EXP prices and execution
selectors unchanged. Paired replay of the two 90-proposal mainnet cohorts above (34,560 blocks)
produces identical node and complete prices under model80459 and the EXP successor; complete
underquotes remain zero in each cohort. This is empirical coverage in those workloads, not a
universal bound or production SDK qualification.

The maximum is applied **per event before summing**, retaining `Composition::Accepted`. A maximum
of route-wide sums could dilute the floor when small/large exponents choose the original curve
while intermediate lengths choose the floor. Feature-error fallback remains separate and unchanged.
Impossible typed lengths above 32 are rejected. The initial Shadow model name, default modes,
Unzen meter, limits and every unrelated route remain unchanged. Node-known subtotal remains the
workload-search denominator; complete-proposal coverage does not make that subtotal a full quote.

### Inherited MULMOD operand-cost refinement

The following measurements describe the earlier 93d-to-model0a3 refinement. Its 1,600-PGU floor
and payload ownership are preserved by the current artifact; the historical quote deltas below
are not measurements of the new arithmetic/MCOPY refinement.

Ordinary, normally executed `opcode:0x09` events now use the existing route-aggregate maximum
composition with a structural floor of **1,600 PGU per event**, selected by
`constant_one_per_event`. The fitted accepted model remains
`12.974884691486860714 + 32.012267798378978094 * interpreter_raw_gas`, or
`269.073027078518685466 PGU` for MULMOD's raw gas of eight. The route maximum retains whichever
candidate is larger. Feature-error events retain their separate original conservative fallback;
they do not also receive the normal-event floor. That earlier refinement preserved all other
routes, selectors, discounts, limits, runtime collection and public APIs.

The versioned [evidence manifest](../crates/evm/src/zk_gas/pricing/successors/mulmod-evidence-v1.json)
contains 30 accepted synthetic local execution records: 16 original arithmetic/proposal cases,
12 operand/modulus boundaries and two promoted larger boundary cases. It records explicit
32-byte operands, counts observed by native execution, canonical input/public-value hashes,
unrounded parent complete-proposal prices, actual PGU and digests of the original request, native
receipt report, quote, SP1 report and measurement status. Those digests identify retained external
experiment files; the compact manifest does not contain their full witnesses or independently
replay them during generation. Two subsequently selected holdouts are outside the fit manifest.

The exact 30 fit inputs, two holdouts, original observations and successor quotes are preserved in
the immutable [MULMOD fixture Release](https://github.com/taikoxyz/raiko2-atlas/releases/tag/fixture-unzen-mulmod-operand-boundaries-v1).
Its [pinned metadata](https://github.com/taikoxyz/raiko2-atlas/tree/e01443f794fe52af864ea0081d0d4d9b01e41514/fixtures/benchmarks/unzen-mulmod-operand-boundaries-v1)
records compressed/decoded lengths, checksums and archive-relative sample paths. The optional
historical asset adds successor observations and references the existing mainnet input archive;
it does not duplicate those inputs or prescribe a regression runner.

For each record, let `Q` be its parent complete-proposal price before ceiling, `A` its actual PGU,
`N` its observed MULMOD count and `C = 269.073027078518685466`. The required per-event price is
`max(C, C + (A - Q) / N)`. The chosen floor is `ceil_to_100(max(required) * 5/4)`, computed with
exact rational arithmetic. The maximum required value is
`890599921653913252718984671 / 720000000000000000000000`, approximately `1236.944335630435`,
from the 32-block, 90,000-MULMOD-per-block wide-operand/modulus-193 case. This yields 1,600.
This is an empirical conservative flat floor for these constructed cases, not a universal upper
bound on every possible operand, guest, instruction mix or proposal.

All 30 records use SDK/core executor **6.3.0**, frozen ELF
`416f5a475b17aa15914fb9387159d9eae9ecd474e7104da65ac22512ab56da2e` and unchanged estimator
options (gas-trace chunk threshold 134,217,728; two slots). The inherited parent calibration ELF is
`ba5ab7026643730c4416550a069fd0776f0e11ae09155ec5fce413acbe5e59c7`; production SDK 6.1.0
parity has not been established. Execution is local gas estimation, without proof generation.

The successor binds the canonical evidence digest and both generator source digests. Its complete
parent snapshot is authenticated against the pinned parent content address, and exact replay rejects
scope changes or weakened floors. `parent_generator`, `parent_validation`, `authorities`, `sources`
and `execution_identity` retain parent provenance. The successor's `validation` reports projected
local coverage and zero historical replay results embedded during generation; it does not assert
that historical replay was repeated for the successor. Later independent execution/replay reports
remain external evidence and do not mutate this frozen model identity.

An independently rebuilt native collector and quote tool replayed all 30 fit inputs plus the two
holdouts. Every complete quote covered its corresponding local SP1 observation. The wide 32-block
case changed as follows; the program and observation remained fixed:

| Surface | Parent PGU | Successor PGU | Local SP1 observation PGU |
| --- | --- | --- | --- |
| Node-known proposal sum | 1,315,914,592 | 5,148,984,274 | 4,283,103,975 |
| Complete proposal, startup once | 1,495,634,607 | 5,328,704,289 | 4,283,103,975 |

The node sum is intentionally incomplete; the full observation is shown for context, not as a
claim that both surfaces have the same scope. The complete successor quote is 1.244x the local
observation. The small-operand control has the same successor quote and a 1,613,748,933 PGU
observation: conservative flat pricing deliberately overprices this constructed cheap case.

Native replay of another 90 historical mainnet proposals (17,280 blocks, October 23–26, 2026)
preserved all canonical input identities, public outputs, Unzen scores and non-MULMOD pricing
components. It identified 315 normal MULMOD events through exact scaled tariff differences.
Fourteen proposals and 25 blocks changed price. Maximum complete-proposal uplift was 0.0033674%;
maximum block node-known uplift was 0.0408478%. Medians were zero. These are parent/successor
pricing-impact measurements; they neither select an admission limit nor certify production
SDK parity. The replay used the complete GuestInputs preserved by
[Raiko2 Atlas](https://github.com/taikoxyz/raiko2-atlas/releases/tag/fixture-mainnet-sp1-20261023-26-v1).

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

## EIP-7702 authorization validation

Unzen selects Osaka and permits type-4 transactions. Alloy transaction conversion eagerly recovers
**every signed authorization** before EVM collection begins. REVM can later ignore an entry for a
stale nonce, wrong chain, ineligible authority code or invalid recovered authority. Repeated entries
are not cached or deduplicated by conversion. Opcode/precompile observations, unique-account counts
and signed-envelope byte pricing therefore do not represent this validation work.

`TransactionMetadata.authorization_validations` records the raw `TxEnv.authorization_list.len()`
for non-Anchor type-4 transactions before execution on both plain and inspected paths. Other
transaction types and Anchor contribute zero. Included user reverts retain the count; omitted scopes
remain attempted only. The collector checks per-transaction and accumulated selector bounds and
preserves a diagnostic on invalid inputs or overflow without partially merging counts.

Ownership is committed outer transactions for the committed quote and begun EVM scopes for the
attempted quote. Conversion can also recover authorizations of outer input transactions that are
filtered or rejected before an EVM scope begins; neither prefix records those entries. Accounting for
that earlier filtered/unattempted input work remains deferred under the existing honest-input scope.
This term covers all inner entries of included type-4 transactions, not every recovery across all
proposal inputs.

The initial tariff is **200,000 PGU per entry after normalization**, stored as `200000 * scale`.
It is additive to all existing payload, fixed, resource and operation costs; no second discount is
applied. The contribution appears in `authorization_scaled` and is part of the node/mining-known
subtotal. Complete block pricing retains it once when replacing local resources and adding MPT;
proposal-once pricing adds no authorization term. Callers constructing `BlockPricingInput` directly
must supply the snapshot's `authorization_validations`.

The sealed evidence retains 12 whole-proposal SP1 observations and their input/report digests using
the same guest ELF `b76baf2e0f3fa3fe6f8a20fe53dbf82f58601ed14b370dfc0f8d31db2069e034`
and SDK 6.3.0. Nine parent complete quotes underpriced actual PGU. The common single-entry sample
used 161,121,234 actual PGU against 6,156,879 node-known and 161,113,477 complete PGU. Sequential
256-entry work used 186,558,632 actual PGU against 12,390,735 node-known and 167,347,333 complete
PGU; repeated 256-entry work used 184,275,517 actual against 12,358,991 node-known and 167,315,589
complete PGU. The count-zero reference is EIP-1559, not an empty type-4 transaction.

The operator-selected 200,000 tariff is a conservative initial policy motivated by measured
per-entry increments up to roughly 127,000 PGU and a 137,041-PGU single-entry/reference difference.
These are finite observations; they establish no universal PGU upper bound, EVM-gas-saturated maximum
or deployed admission guarantee. Full retained raw fixtures remain external; the versioned policy
binds their audit and compact measurement identities. Existing global discounts and all old terms
remain numerically identical when the authorization count is zero.

## Raw access-list address items

Non-legacy preexecution traverses every declared address item and every storage key before merging
warm sets. Repeated address items remain work even when their addresses and keys are identical.
Existing signed-payload bytes and unique-resource tariffs remain charged; the new term prices only
raw address-item multiplicity. No raw-key tariff is added.

The common plain/inspected `transact_raw` hook records `TxEnv.access_list.0.len()` when `tx_type != 0`,
matching REVM's non-legacy warming gate. This includes Anchor items when carried; it does not assume
Anchor always has an empty list. Legacy TxEnv contributes zero to this term. Metadata, committed and
attempted snapshots, and direct `BlockPricingInput` use `access_list_items`; sealed known breakdowns
expose that count and `access_list_scaled()`. Raw keys and uniqueness do not multiply the item count.
Included user REVERTs retain their entries. Committed reports own included scopes; attempted reports
also retain begun scopes later discarded. Earlier filtered/unattempted outer-input work remains
outside these prefixes, as described for authorization validation above.

The effective tariff is **8,192 normalized PGU per address item**, stored as `8192 * scale` with no
second discount. It is included once in node/mining-known subtotals and complete block totals, and
revalidated from the count during Shadow completion. Proposal-once pricing adds zero access-list
contribution. Counts and all prefix additions use checked bounds/arithmetic; a failed merge retains a
diagnostic without partially changing other selectors. Runtime mode, Unzen metering, gas limits and
header-difficulty rules are unchanged.

### Original evidence and exact derivation

Only the original 23 canonical access-list observations constrain this fit. The sealed evidence
binds their input/native/quote/SP1/resource-receipt digests, the original semantic summary and the
separate final independent raw audit. The same experimental SDK 6.3.0 / ELF
`b76baf2e0f3fa3fe6f8a20fe53dbf82f58601ed14b370dfc0f8d31db2069e034` was used. Native construction
and retained actuals were independently checked; source bindings are provenance, not a clean-build or
bit-rebuild attestation. Prospective holdouts are excluded from the fitting evidence.

Let `A` be actual PGU, `Q` the parent complete scaled price divided by scale, `N` the parent
node-known scaled price divided by scale, and `I` the raw address-item count. Compute the maximum of:

```text
0
(5/4) * max(A - Q, 0) / I                  for each observation with I > 0
((5/4) * delta(A) - delta(N)) / delta(I)   for selected pairs with delta(I) > 0 and delta(A) > 0
```

The selected pair collections are shared-baseline increments, same-pattern adjacent increments,
grouping comparisons and matched three-block references. The zero-item references must already be
covered. The exact maximum is **19,375,485 / 4,094 PGU per added item**; take the integer ceiling
**4,733**, then round upward to the next power of two: **8,192**.

The binding grouping case compares 2,048 repeated keys in one item with 2,048 repeated items carrying
one key each. Actual work increases by 17,391,154 PGU while parent node pricing increases by
12,051,200 PGU: a 5,339,954-PGU uncovered increment for 2,047 extra raw address items. The fit preserves
all existing payload and unique-key/address charges.

Pairs with zero raw-item delta remain shape/order diagnostics, without forced slope fitting. In
particular, reversing the same 4,096-key set produced a **1,502-PGU actual difference at the same
price**. This tariff does not cover that increment because it gives both lists the same item price.
The sealed arithmetic projects original full underquotes from **3 to 0**; it is not native repricing
of the new model. Native consumer validation and independent holdouts remain separate work. These
finite cases establish neither a universal hash-distribution bound nor EVM-gas-saturated coverage;
there is no new admission activation or guest rebuild.

## Code preparation occurrences and byte loading

The current refinement separates two observed dimensions:

- Existing `ResourceSelectors.unique_code_bytes` retains its per-block, hash-deduplicated
  semantics. Its normalized unit is 64 PGU/byte, including observed executing initcode.
- `CodePreparationSelectors { nonempty_events, total_bytes }` adds actual accepted create-frame
  processing work per occurrence. Repeated identical runtime code contributes repeatedly.
  Its normalized price is `(8192 * nonempty_events + 32 * total_bytes) * scale`.

`TaikoEvm::frame_return_result` observes successful `FrameResult::Create` with an address and
nonempty original output, before handing the result to the inner EVM. REVM has already passed
code size, EF-prefix and deposit-gas checks and performed code hashing/analysis/registration.
Top-level creation and internal CREATE/CREATE2 share this hook. Empty output, unsuccessful
creation, missing addresses and early balance/depth/nonce/collision failures contribute zero.
A later ancestor REVERT does not undo work already performed. The selected plain collector or
inspector collector owns each occurrence once; no second frame-marker stack or state-derived
counter is used. Disabled remains unchanged and does not install this collector accessor.

Inspector completion callbacks precede the common hook. The quantitative contract therefore
requires a **semantics-preserving inspector**, including current NoOp/resource/fixture observers.
It does not promise counts for arbitrary inspectors that rewrite output, status or address.

Snapshots expose `code_preparation` in both committed and attempted prefixes. Direct
`BlockPricingInput` requires it explicitly; sealed known reports expose `code_preparation()` and
`code_preparation_scaled()`. Complete `PricingBreakdown` includes the contribution once under
`code_preparation_scaled`, and proposal-once pricing contributes zero. Completion revalidates
raw selector consistency, the sealed contribution and subtotal. Transaction/prefix additions are
checked before either field or other prefix selectors change; errors retain a diagnostic without
changing EVM execution. Each selector is bounded by 10^9, empty events/bytes must agree, and
aggregate bytes must lie between event count and 24576 times event count. Collector deltas also
validate each event. The current domain is post-Unzen's 24K code cap; a future code-size fork
requires an explicit domain update.

The evidence contains **92 execute-only measurements / 88 canonical inputs**, using the same
experimental b76baf2e ELF and SP1 SDK6.3: 44 creation probes, 38 followups and 10 initial-code
loading controls. All three independent raw audits passed; the original **18 underquoted rows /
17 distinct inputs** remain recorded. Native qualification and integrity audits establish the
input/result joins; they do not turn finite prices into a universal bound or certify real proving
throughput.

The exact loading-byte requirement is 242969/4096 PGU/byte. Integer ceiling 60 and upward
power-of-two rounding select 64. The one-byte versus empty-runtime event pair leaves an exact
5525.449319692974457273 PGU after a 25% workload-increment margin and the original price;
ceiling 5526 selects 8192. Within-block N2/N4 repeated identical 24K code uses a difference of
differences against same-count zero-runtime controls and the N1 difference. This removes shared
transaction/account work and the one-time unique-byte term. With 8192/event, the largest
remaining byte requirement is 25.138897231643035 PGU/byte; ceiling 26 selects 32. Multiblock
carried-but-unused code is deliberately excluded from causal fitting. Equal-selector content
sensitivity remains visible rather than being assigned an invented selector.

The sealed arithmetic projects full underquotes from 18 to zero. These are **projections**, with
preparation counts derived from independently verified successful top-level receipt/runtime
expectations. They are not observations from the new counter or actual successor native replays.
The 16 prospective validation cases were not used for fitting. Reprice all original inputs and
historical proposals with the actual successor executable before claiming compatibility. The
older 50-proposal history uses a separate SDK6.1/416f program identity; the new 92-row evidence
uses SDK6.3/b76. Their projections and program identities must not be conflated. No guest rebuild,
admission activation, real proof or hardware-capacity claim follows from this refinement.

## Scope decomposition and ownership

The complete block-local fixed-point price is:

```text
block_price =
    block_base
  + committed_non_anchor_transaction_tariff * committed_non_anchor_transactions
  + native_value_transfer_tariff * native_value_transfers
  + authorization_validation_tariff * raw_authorization_validations
  + access_list_item_tariff * raw_access_list_address_items
  + code_preparation_event_tariff * nonempty_code_preparation_events
  + code_preparation_byte_tariff * prepared_code_bytes
  + operation_price
  + exact_unique_resource_price
  + committed_transaction_payload_price
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
  + canonical_transaction_byte_tariff * additional_canonical_transaction_bytes
  + nonempty_payload_transaction_tariff * additional_nonempty_payload_transactions
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

- a normally modeled opcode emits its `accepted_absolute` and any configured `incremental` or
  `structural` candidate; arithmetic opcodes 0x04–0x09 emit their configured normal-event floors;
- a supported opcode whose typed extraction fails, that halts exceptionally, or whose REVERT has
  nonzero raw gas emits `feature_error` instead of its normal candidates;
- precompile routes can also emit `structural` floors and do not have `feature_error`; and
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

- `opcode:0x04`–`0x09` take the maximum of their fitted accepted price and the configured
  normal-event structural floor. Their feature-error paths remain separate.
- `opcode:0x5e` (MCOPY) adds 48 PGU per copied word to its existing dispatch, constant and shared
  memory costs, keeping every memory expansion charge additive.
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
| `opcode:0x04`, `0x05`, `0x06`, `0x07`, `0x08` (DIV, SDIV, MOD, SMOD, ADDMOD) | 800, 900, 800, 800, 1,100 structural floors |
| `opcode:0x09` (normally executed MULMOD) | 1,600 structural floor |
| `opcode:0x32` (ORIGIN), `opcode:0x38` (CODESIZE), `opcode:0xfd` (normal REVERT), `opcode:0x39` (CODECOPY within its raw-gas envelope) | 5,000,000 |
| `opcode:0xa3`, `opcode:0xa4` (LOG3/LOG4) | 5,000,000 plus shared memory |
| `opcode:0x3b` (EXTCODESIZE) | 1,000,000 |
| `opcode:0xf1:confirmed_spawn_wrapper` (CALL) | 1,000,000 plus shared memory |
| `opcode:0xfa:confirmed_spawn_wrapper` (STATICCALL) | 5,000,000 plus shared memory |
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

When a CALL/CREATE-family instruction selects a new frame, the collector defers the wrapper until
frame dispatch confirms it. The four CALL-family wrappers carry `OperationModelInput::SpawnMemory`:
parent-frame growth count, EVM memory-expansion gas delta, and the existing extra-4KiB-page event.
The selectors are measured after the interpreter reserves the combined input/output ranges and
before child execution. Overlapping ranges are counted once; preexpanded memory and zero-length
ranges add no growth. The unchanged shared-memory tariff is additive to the existing fixed wrapper
body. CREATE/CREATE2 retain their existing payload-free pricing.

Confirmed wrappers contain neither interpreter raw gas nor native gas: the CALL interpreter gas
delta includes forwarded child gas. Child operations and ordinary copy opcodes retain their own
memory expansions; consumers must not add an aggregate memory pass or the ordinary spawning opcode
route. This repair adds no independent input-copy or output-copy tariff.

Confirmation occurs before later depth or balance checks in the dispatched frame. A selected spawn
that is never dispatched is not charged, and transaction completion/discard clears its pending
payload. A call into a precompile charges both the confirmed wrapper and the precompile body.
Child revert or exceptional halt does not undo work already observed in the parent wrapper.

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

The block executor records `TransactionSigned::encode_2718_len()` and whether calldata is
nonempty after each execution scope opens and before that scope is committed or discarded.
`TransactionPayloadSelectors` is staged with the transaction: included Anchor, REVERT and HALT
receipts retain their bytes; rejected/omitted transactions do not enter the committed prefix.
The separate attempted prefix retains started scopes. Raw `TxEnv`/RPC execution has no signed
envelope and produces `MissingSignedTransactionPayload` instead of a zero payload quote.

`ZkGasModel::price_transaction_payload` uses the existing 256 PGU/byte and 16,384 PGU/nonempty
calldata tariffs for both block-local and additional-input work. The model artifact names
`block.transaction_payload` ownership explicitly. The coefficients remain stored once under
`proposal_once.transaction_payload_tariffs`; their storage path does not imply proposal-only
ownership. The known block report seals both selectors and their price, and completion
revalidates them before composing exact resources/MPT.

`price_proposal_once` retains its `canonical_transaction_bytes` and
`nonempty_payload_transactions` field names, but they now mean **additional input payload not
already charged in committed block scopes**. The consumer must reconcile exact committed
canonical transactions against the block report and charge only the remaining candidate/input
bytes. It must reject mismatches rather than clamp a negative residual to zero. The separate
true-unattempted transaction tariff remains prover-only. This preserves the complete proposal
price while making the committed portion observable at the node, without distributing startup,
blob topology or parent witness cost backward to blocks. Future admission activation remains a
separate change; default Unzen limits and difficulty behavior are unchanged.

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

The immutable parent embeds relative source paths, file hashes, authority hashes, guest hashes and
historical validation. Its original calibration files and exporter are not vendored here; recreating
those fitted coefficients from raw measurements requires the external parent experiment material.

### Inherited DELEGATECALL refinement: modela298 to model2fb

This historical section describes immutable parent
`a2988920125e304121acb629b9b96fd1ad85ee707c89978240408c81e2a42f3e` and its successor
`2fb4bb71092f11cca2217e69b25e94217342ffb46a540a961e7a77a483cb5601` (model2fb).
Its native, prospective and historical validation results below apply to model2fb, not the current
BLOCKHASH successor. The historical `delegatecall_successor.py` exporter uses the retained a298
snapshot and eight density observations to solve every same-terminal N0/N64 constraint exactly:

`required = max(old_wrapper + (5/4 * actual_delta - old_node_delta) / extra_confirmed_calls)`

The maximum requirement is
`90621022421143230291790619 / 16352000000000000000000`, approximately 5541.8923 PGU/event.
Rounding upward to the next 1000 PGU gives 6000. The ten matched pairs contain only the eight
opened STOP/RETURN(0,0) density inputs. The policy was frozen before any fresh PUSH0 holdout actuals;
no prospective measurement is used to fit this value. This is composite loop/child evidence,
not an isolated-frame cost measurement or a universal bound.

The evidence binds original input/output/report hashes, SDK6.3, the frozen local b76 ELF,
the parent quote and the independent raw audit. The exporter projects one complete-quote deficit
before the change and zero afterward; this arithmetic projection is **not** successor native replay,
a fresh holdout result or a historical replay. Those validations remain separate. The runtime
artifact preserves inherited authority fields as historical metadata and identifies the new unit's
authority separately through the sealed density evidence and pre-holdout policy. All earlier
coefficients and their calibration provenance remain inherited.

Subsequent validation used a separately built native quote for frozen model2fb;
the DELEGATECALL exporter's fit/projection record above is intentionally unchanged.
Same-input repricing confirmed that only the fixed F4 contribution changes.

| Validation cohort | Observations | Parent full deficits | Successor full deficits |
| --- | ---: | ---: | ---: |
| Density development | 8 | 1 | 0 |
| Previously measured checked references | 16 | 0 | 0 |
| Fresh PUSH0 prospective inputs | 6 | 1 | 0 |

The six prospective inputs use STOP/empty RETURN, four PUSH0 arguments and
0/4096/65536 calls in a fixed 64-call body. None selected the tariff. Actual
SP1 SDK6.3 / ELFb76 execution and exact native repricing cover every complete
quote and all six N0/N4096 increments. The largest STOP case uses 7,696,427
EVM gas and measures 459,270,055 actual PGU versus 415,712,505 node-known and
570,680,872 complete quoted PGU. The former development failure now quotes
367,867,019 complete PGU against its unchanged 311,589,746 actual PGU.

Actual native replay of the same retained 50 Mainnet proposals / 9600 blocks
changes total full quotes from 150,234,443,229 to 150,255,527,803 PGU (+0.01403%;
largest proposal +0.02056%). Their original SDK6.1 / ELF416f actuals remain
69,980,978,094 PGU, so weighted full/actual changes from 2.14679x to 2.14709x,
with zero full deficits in both versions. This is a historical sensitivity
check, not current b76 accuracy certification or a new SP1 proving run.

Independent source, native, actual-workload and historical-report audits passed for model2fb.
That comparison quote used an explicit temporary dependency override to the
reviewed upstream source, not a committed Raiko pin or guest update. These
finite observations do not establish a universal node-admission bound, proof
throughput, production activation or deployment readiness.

Fixed inputs, original failures, successor quotes and independently checked
reports are retained in [Atlas PR20](https://github.com/taikoxyz/raiko2-atlas/pull/20)
and its [immutable fixture Release](https://github.com/taikoxyz/raiko2-atlas/releases/tag/fixture-unzen-delegatecall-density-v1).
Reusable producers and measurement tools remain in Raiko. The historical
comparison retains reports and source identities, not historical GuestInputs.

### Export replay boundaries

The checked-in commands below reproduce the current BLOCKHASH JSON, acceptance record and Rust table without
rerunning SP1 or vendoring full GuestInputs. Repeating measurements requires the external fixed
inputs and recorded program/SDK. `parent_validation` records inherited results, not reexecution;
this refinement does not establish parity with a deployed production guest, wall-clock proof
latency or capacity.
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

## BLOCKHASH density correction

### Original calibration semantic erratum

The original BLOCKHASH calibration has a confirmed generator and semantic-validation defect.
Its 90 normalized measurement records and rational refit still reproduce the inherited
32.117684584888432025 PGU coefficient. That numerical reproducibility does not establish the
ancestor-lookup coverage claimed by its `recent_ancestor_hit_1` and `recent_ancestor_hit_256` labels.
The result artifact is
`5fbdf5d26918e57214d70997795b2b41fad3219b8821730a08b0430f890c1d0c`, bound to Raiko revision
`21ac9f900d880bb1167b3ca5d2b95be9afe72cd4` and four recorded source-file hashes.

In that revision, `controlled_blockhash_bytecode` in
`bin/guest-launcher/src/controlled_workload.rs` emits `NUMBER; PUSH offset; SUB; BLOCKHASH; POP`.
The pinned REVM41 SUB implementation subtracts the next stack value from the top value, so this
computes `(offset - NUMBER) mod 2^256`. At the controlled height 1166000, offsets 1 and 256 produce
huge future heights; reconstructing that source reaches BLOCKHASH's early-zero branch before any
host/database ancestor lookup. The fixture validator separately calculates the intended
`NUMBER - offset` and checks the retained header window. It does not check the emitted operand or
returned value, which is discarded. Opcode counts, gas-ledger agreement and fit/holdout residuals
can therefore pass while the intended ancestor-hit semantics are absent from the generated program.
Fit and holdout share this generator.

The four bound source files were independently matched to their Git blobs, and the 90 preserved
record hashes and numerical derivation were independently checked. Not all original GuestInputs
or complete old-binary/source bindings have been recovered. This is a source-bound semantic
reconstruction, not a claim of replaying all 90 historical executions and observing their runtime
operands. Original data and identities remain unchanged. Reth does not vendor
that original calibration material and generator; this distribution gap limits a standalone
audit of the inherited semantic labels from Reth alone.

The current 700-PGU coefficient is selected from the separate twelve development inputs and
validated with six prospective inputs below. Their producer emits `PUSH offset; NUMBER; SUB`,
checks the expected zero/nonzero result,
and has independent operand-level regression checks. Those 18 actual measurements do not rely
on the defective old generator. The inherited per-route `authority_sha256` value `5fbdf5…` records
parent provenance; it is not valid ancestor-hit coverage evidence. The active correction authority
is the sealed evidence identified by `successor.blockhash_evidence_path` and its SHA256, with the
frozen policy and exact successor replay. The complete model identity authenticates that derivation;
the generated runtime does not consume the old authority hash as a validity gate. This erratum
changes no coefficient, exporter, model identity or immutable fixture.

### Current controlled measurements

The sealed development cohort contains twelve whole-proposal SP1 SDK6.3 execute-only observations:
nearest valid ancestor `B-1`, farthest valid ancestor `B-256`, and expired `B-257`, each with
0, 64, 4096 or 262144 bulk queries. Every user program has one additional semantic query,
so its event count is `bulk_queries + 1`, including N0; this excludes Anchor execution. The fixed
64-site body retains the same unused code at N0. Native completion gates establish the exact target, opcode totals, successful
receipt, contiguous ancestor headers and absence of exceptions. No deployment is involved.

The parent has six complete-quote deficits and fifteen uncovered matched increments. Each same-mode
comparison uses an N0 or N64 reference. The frozen policy changes only the normal BLOCKHASH fixed
coefficient, selecting

```text
ceil_to_100(max(old_BLOCKHASH + (5/4 * actual_delta - old_node_delta) / extra_BLOCKHASH))
```

The exact maximum threshold is approximately 612.549665382938 PGU/event, hence 700 PGU/event.
All fifteen corrected matched node and complete-quote increments cover 125% of their observed actual
increments. The exporter's closure projection adds the positive per-event price increase for only
the user program's `bulk_queries + 1` events to the parent complete quote. It leaves Anchor and
other non-user work at their parent prices, so this is a conservative lower bound on the successor
complete quote, not its exact native value. Even that lower bound has zero complete-quote deficits
for these twelve observations. The sealed export records no successor native replay. Frozen evidence
retains the canonical input, native, parent quote and actual report hashes, and the independent raw-audit identity.
The policy was fixed before opening the six prospective validation observations; none of those observations
is used for this coefficient.

The synthetic fixture uses Raiko's checked-in `scripts/regression/shasta/Anchor.json` runtime.
Its embedded AST shows `anchorV4` calling `_validateBlock`, which calls `_calcAncestorsHash` once.
At this fixture's block height, the latter executes 255 BLOCKHASH queries for earlier ancestors
and one for the parent; `anchorV4` then queries the parent again when storing `blockHashes`.
That contributes 257 Anchor events per block, in addition to the user program's events.
The fixture's native opcode observation deliberately excludes Anchor, whereas node/prove pricing
includes it. Anchor has already queried all 256 valid ancestor heights before the user transaction,
so valid-target user queries are warm even before their semantic check. Expired queries return zero
before consulting the host or database and do not access or warm this cache. The shared Anchor work cancels
in the same-mode matched increments and does not change the frozen 700-PGU selection.

Subsequent same-input native repricing covered all twelve development inputs and six prospective
inputs. For these eighteen synthetic blocks, both node and prove operation-price changes correspond
exactly to `bulk_queries + 258` events: `bulk_queries + 1` user events plus 257 Anchor events.
All unrelated pricing terms, inputs, public outputs and Unzen observations remain unchanged;
the twelve development complete-quote deficits fall from six to zero. Native quotes alone do not
assign actual SP1 PGU to the six prospective inputs; their separate measurements are reported below.
The 257-event Anchor count belongs to this artifact and fixture
context, not every fork, block height or Anchor implementation. These later native results do not
change the sealed export's user-event-only projection, fit evidence or model identity.

### Subsequent validation under modelbbce

The six prospective inputs query the farthest valid or expired height with 0, 8192 or 196608 bulk
queries and an executed PUSH0/POP prelude, including N0. All six completed SP1 SDK6.3 / ELFb76
execute-only measurements under the previously frozen 700-PGU policy. Their complete quotes cover
all six actual observations and all six same-mode N0/N8192 increments. None was used to select or
adjust the tariff. The farthest/196608 case measures 266,827,920 actual PGU versus 156,273,289
node-known and 311,215,120 complete quoted PGU. The expired/196608 case measures 203,146,003
actual PGU versus 156,273,392 node-known and 311,215,222 complete quoted PGU. Literal actual/node
ratios retain proposal/prover-only work in the numerator; complete quotes and matched increments
remain separate metrics. Peak sampled process-tree PSS was 1,985,410,048 bytes, below the 6GiB guard.

Native repricing of the same retained 50 Mainnet proposals / 9600 blocks changes aggregate complete
quotes from 150,255,527,803 PGU under model2fb to 151,903,327,053 PGU under modelbbce: +1.0966646%
overall, with a maximum proposal increase of 1.6265893%. Complete-quote deficits remain zero in both
versions. The original SDK6.1 / ELF416f actual sum remains 69,980,978,094 PGU, so weighted
complete/actual changes from 2.147091x to 2.170637x. This is a historical sensitivity check against
retained observations, not fresh SDK6.3 actuals or strict current-program accuracy certification.
Historical GuestInputs were reverified from the immutable fixture archive before native replay;
all unrelated quote terms, Unzen observations, input hashes and public outputs remain unchanged.

These reports are later validation of the same sealed model, not a rewrite of its derivation record.
The export's `new_native_repricing_performed=False` and `historical_repricing_performed=False`
describe what had run when it was sealed; they are not live validation-status fields.
`prospective_actuals_used_for_fit=0` remains true after validation. The comparison quote used a
temporary override to reviewed upstream source; no committed production pin or guest artifact
changed, and no old SP1 workload run was repeated to obtain new native prices.

These are composite loop-workload increments, not isolated opcode proving costs. Cross-mode differences
can include semantic checks and target arithmetic. A constant-time source lookup does not establish its
numeric PGU cost. Literal whole-proposal actual/node ratios retain startup work and remain separate from
complete-quote closure and matched increments. This finite correction does not establish a universal bound,
production guest parity, capacity, throughput or a change to Unzen admission. Prior model, evidence and
validation records remain immutable; inherited historical claims are not reexecuted by this exporter.

## Reproduction and change policy

Verify the checked JSON address and generated Rust table with:

```bash
~/.venv/bin/python -m unittest discover -s crates/evm/src/zk_gas/pricing -p '*_test.py'
~/.venv/bin/python crates/evm/src/zk_gas/pricing/blockhash_successor.py --check
~/.venv/bin/python crates/evm/src/zk_gas/pricing/generate.py --check
```

Regenerate the reviewed current successor and its Rust table with:

```bash
~/.venv/bin/python crates/evm/src/zk_gas/pricing/blockhash_successor.py
~/.venv/bin/python crates/evm/src/zk_gas/pricing/generate.py
just fmt
```

A refinement requires versioned evidence and an explicitly reviewed successor derivation. Preserve
the existing immutable parent/evidence records. Reproduce the successor JSON with its transform,
then update `accepted-runtime-spec.sha256`, the identity line in this document and the pinned
assertions in `crates/evm/src/zk_gas/tests.rs` (identity, scale and route count). Regenerate and
validate the table; rerun targeted unit and replay checks. Changes to evaluator constants,
extraction, ownership, hard-coded domains, or selector bounds also change effective semantics even
when the JSON address does not change; they require explicit review, compatibility evidence, and a
deliberate identity/version decision.

Do not add a compatibility alias that lets reports from two model identities be completed as though
they were the same model. Documentation-only clarification may retain the identity only when it does
not change the artifact or executable semantics.
