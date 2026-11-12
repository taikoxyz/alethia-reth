# zkGas Shadow Pricing Model

## Status and scope

This document describes the SP1 Prover Gas Unit (PGU) Shadow pricing export compiled into
alethia-reth. The current runtime artifact is the ECMUL incremental-tariff **candidate**
`80160649d7d29a85879cd9524b35c271a4b8638f64de94eef91d016e56121cf8`, derived from immutable parent
`e8e574db50607857daadfd79f24e2cd6773177f6b46e4ee950f168379b4cab57`.
It changes only PC07's incremental coefficient from 29,901 to 0 PGU/native-gas; the complete
accepted constant/native slope and scalar structural slope are retained. The
[ECMUL section](#candidate-ecmul-incremental-tariff) records the finite validation gates.
The inherited [SHA256 floor section](#candidate-sha256-fixed-floor) records its earlier
PC02 constant refinement from 20,000,000 to 81,024 PGU.
The inherited [ordinary-tariff section](#candidate-ordinary-path-marginal-tariffs)
records the sealed marginal evidence and retained constraints. All unrelated policies and the
consensus Unzen schedule remain unchanged. Repricing and validation results belong to the model
identity that actually produced them; earlier results below describe their original checkpoints.

The inherited progressive storage read-through **candidate**
`eea6f1da87ee129d2d9f759ae6cef01cb8ac93988376a86efb290398e6db5da6`, derived from immutable
parent `3e335de20582fca48f84b3df1b2ecd8d8d7281de02442ab35a134e4635431d73`. It retains every
numeric tariff, discount and arithmetic domain, including384 normalized PGU per storage work
unit, and expands that component's transaction budget from2X toJ+2X. The existing public
`storage_transition_*` fields now include read-through queries as well as changed-slot forward
passes; there is no new selector or component charged alongside an unchanged2X component.
The [read-through candidate section](#candidate-progressive-storage-read-through-reserve) records
its formula, measured deficiency, fresh retained-input results and limitations. All86 retained
inputs/37groups were requoted once across9,642blocks: full estimates cover all33 original numeric
actuals,29 literal-node sums cover, and four lower-priority node deficits remain (maximum153,343,319PGU).
No measured residual exceeds2B in this finite cohort. The50-proposal/9600-block history changes
+0.041124% node/+0.023870% full over3e335; its absent numeric actuals and416f/SDK6.1 provenance make
this price sensitivity only, not currentb76 accuracy or an admission guarantee. Artificial
read-cache successors now quote about5.0–5.3x actual; these stress results do not establish an
ordinary-mainnet overquote ratio or a global workload bound.

The earlier3e335 checkpoint's82-input checks remain unchanged:29 original numeric actuals,
50 historical proposals/9600blocks, full estimates covering all29 actuals and four lower-priority
literal-node deficits (maximum153,346,391PGU). Its historical node/full additions over0222 were
+0.032639%/+0.018942%, older-program sensitivity rather than current-program accuracy. Subsequent
89-user read-cache stress evidence demonstrates a further2.890B literal-node gap at28.062Mgas;
the earlier finite cohort did not establish a universal bound. Earlier results retain their own
model identities. Consensus Unzen behavior is unchanged and the Shadow model still has no public
version selector.

The previous `af30` checkpoint added 128 normalized PGU per **transaction-local Account-journal**
work unit to immutable parent `69d2b812…`. For that checkpoint, all 21 retained full quotes covered
their original actuals, four small literal-node deficits remained, and the 50-proposal historical
panel changed by +0.2835% node / +0.1643% full. Those results describe `af30`, not the new cache
candidate. See the [Account-journal section](#empirical-account-journal-density-reserve) for its
original formulas and results. The new warm-first case demonstrates a further block-lifetime
population gap; its two opened actuals are now stress/calibration evidence rather than holdouts.
Earlier model-specific sections retain their original checkpoint identities. All these are finite
empirical results, not universal bounds or production SDK parity certification.

The original calibration calls itself `accepted_runtime_export`; the current successor purpose is
`block_local_empirical_ecmul_incremental_refinement`. These identify calibration and
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
| Runtime log | One `tracing` event per successful executor finish, target `zkgas`, for the committed prefix. `Incomplete` carries the model ID, status, EVM-local subtotal, and fixed/authorization/access-list-item/access-list-construction/access-list-address-pair/code-preparation/storage-journal/Account-journal/account-cache/storage-transition/operation/local-resource components; `Diagnostic` carries only the model ID and coarse status. |
| Prover outcomes | With the `prover` feature, `CommittedBlockExecutionOutcome` and `DerivedBlockExecutionOutcome` return `zk_gas_shadow_report`, `attempted_zk_gas_shadow_report`, and `attempted_transaction_count`. The reports are EVM-local and incomplete, not complete block quotes. |
| Exact block completion | Public evaluator API and tests exist, but this repository has no production caller. |
| Proposal-once pricing | Public evaluator API and tests exist, but this repository has no production caller. |
| Exact resources, block/parent MPT, and proposal selectors | No production producer is wired in this repository. The [companion Raiko tools](https://github.com/taikoxyz/raiko2/pull/296) consume these APIs for offline GuestInput replay and comparison; this does not wire them into Reth admission. |
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
- [`ecmul_incremental_successor.py`](../crates/evm/src/zk_gas/pricing/ecmul_incremental_successor.py)
  authenticates immutable parent e8 and the [retained ECMUL evidence](../crates/evm/src/zk_gas/pricing/successors/ecmul-incremental-v1.json),
  replays 33 whole-input actuals and 33 within-group contrasts, and changes only the PC07
  incremental coefficient. It preserves full accepted/native and scalar structural models.
- The historical [`sha256_floor_successor.py`](../crates/evm/src/zk_gas/pricing/sha256_floor_successor.py)
  authenticates immutable parentc087 and the [retained SHA256 evidence](../crates/evm/src/zk_gas/pricing/successors/sha256-floor-v1.json),
  recomputing every same-length/pattern count pair before changing only the accepted PC02 constant.
- The historical [`ordinary_tariff_successor.py`](../crates/evm/src/zk_gas/pricing/ordinary_tariff_successor.py)
  derives the inherited ordinary-tariff checkpoint from the immutable EEA snapshot and
  [sealed raw marginal facts and qualification summary](../crates/evm/src/zk_gas/pricing/successors/ordinary-tariffs-v1.json).
- [`storage_transition_successor.py`](../crates/evm/src/zk_gas/pricing/storage_transition_successor.py)
  derives the inherited read-through/forward candidate from immutable3e335 and the
  [opened read-cache evidence](../crates/evm/src/zk_gas/pricing/successors/storage-read-cache-v1.json).
  [`storage_transition.rs`](../crates/evm/src/zk_gas/pricing/storage_transition.rs) owns the physical
  finalized receipt and separate committed/attempted per-owner block unions.
- [`account_cache_successor.py`](../crates/evm/src/zk_gas/pricing/account_cache_successor.py)
  derives the inherited progressive block-cache component from immutableaf30 and the
  [sealed warm-first evidence](../crates/evm/src/zk_gas/pricing/successors/account-cache-v1.json).
  [`account_cache.rs`](../crates/evm/src/zk_gas/pricing/account_cache.rs) owns its independent
  receipt and virtual-prefix ledger.
- [`account_journal_successor.py`](../crates/evm/src/zk_gas/pricing/account_journal_successor.py)
  derives the inherited Account-journal checkpoint from immutable69 and the
  [sealed reference/successor evidence](../crates/evm/src/zk_gas/pricing/successors/account-journal-v1.json).
  It seals the Account policy, empirical residual margin and independent work domain while
  preserving every inherited numeric object.
  [`account_journal.rs`](../crates/evm/src/zk_gas/pricing/account_journal.rs) owns the observation ledger.
- [`access_list_address_pairs_successor.py`](../crates/evm/src/zk_gas/pricing/access_list_address_pairs_successor.py)
  derives the inherited raw address-item pair reserve from immutablec306 and its
  [three-row evidence](../crates/evm/src/zk_gas/pricing/successors/access-list-address-pairs-v1.json).
- [`access_list_other_pairs_successor.py`](../crates/evm/src/zk_gas/pricing/access_list_other_pairs_successor.py)
  derives the inherited different-low32 component from immutable389 and the
  [four whole-input actuals](../crates/evm/src/zk_gas/pricing/successors/access-list-other-pairs-v1.json).
  It retains384PGU low32 pricing and all other numeric objects, with a separate96PGU tariff,
  domain and conservative mixed-i128 arithmetic allowance. Evidence fixture publication status
  records the historical generation event, not live publication progress.
- [`access_list_construction_successor.py`](../crates/evm/src/zk_gas/pricing/access_list_construction_successor.py)
  adds the named raw-prefix supplement to immutable 923, authenticating the sealed
  [five-actual/eight-input evidence](../crates/evm/src/zk_gas/pricing/successors/access-list-construction-v1.json).
  It preserves every old numeric price and keeps unrealized/unmeasured inputs distinct from
  qualified concentrated inputs. Its finite projections are not successor-native measurements.
- [`transient_journal_successor.py`](../crates/evm/src/zk_gas/pricing/transient_journal_successor.py)
  extends the journal selector from immutable 5f to independent transient work, using the sealed
  [eight-actual evidence](../crates/evm/src/zk_gas/pricing/successors/transient-journal-v1.json).
  It preserves every inherited numeric pricing/arithmetic object and authenticates finite projections,
  retained deficits, conservative false positives and the distinct-key lifecycle policy.
- [`storage_journal_successor.py`](../crates/evm/src/zk_gas/pricing/storage_journal_successor.py)
  derives the inherited ordinary-storage journal reserve from immutable c893 and the sealed
  [three-row comparison evidence](../crates/evm/src/zk_gas/pricing/successors/storage-journal-comparison-v1.json).
  It authenticates the negative 256 candidate, exact residual margins, separate work bound and all
  unchanged inherited numeric prices.
- [`normal_dispatch_successor.py`](../crates/evm/src/zk_gas/pricing/normal_dispatch_successor.py)
  adds the inherited normal-dispatch reserve to immutable bbce using the sealed
  [60-row composite evidence](../crates/evm/src/zk_gas/pricing/successors/normal-dispatch-v1.json).
  Exact rational replay authenticates the parent, raw-derived counts and independent panel/audit
  identities, and rejects every unrelated numeric or provenance change.
- [`blockhash_successor.py`](../crates/evm/src/zk_gas/pricing/blockhash_successor.py)
  derives the inherited fixed BLOCKHASH unit from immutable model2fb and the sealed
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
  only the current SHA256 fixed-floor successor is regenerated by the commands below.
- [`accepted-runtime-spec.sha256`](../crates/evm/src/zk_gas/pricing/accepted-runtime-spec.sha256)
  pins the accepted successor separately from the generators, avoiding a self-reference cycle.

If prose conflicts with code, the executable evaluator and extractor control current behavior. The
JSON content address covers the canonical JSON fields; it does not cryptographically bind evaluator
constants, extraction rules, or ownership code. In particular, the blob/source domain and selector
bound are currently duplicated in Rust rather than generated from the JSON. Their agreement relies
on review: current tests do not compare those Rust definitions with the JSON fields. The artifact
hash alone does not enforce that agreement.
The Account-journal opcode-budget table is generated from the sealed `account_journal_policy`;
its extraction timing, physical-population source and scope ownership still require source review.

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

### Exceptional-instruction collection correction (November 12)

The collector now maps an actually observed `OpcodeNotFound` or `NotActivated` halt to the
existing `opcode:0xfe` INVALID feature-error route. For the pinned UNZEN/OSAKA instruction set,
the former covers 102 undefined byte values; the latter covers the four metadata-known but
inactive instructions SLOTNUM, DUPN, SWAPN and EXCHANGE. A failed child remains priced when its
caller catches the failure, and included or discarded outer scopes retain their existing
committed/attempted ownership. This is one error event, with no normal candidates or normal
dispatch reserve. It does not change EVM execution, legacy Unzen metering or the route tariffs.

This is an extraction-semantic correction, not a new numerical model: the runtime JSON, trace
schema and 174 sealed routes remain unchanged. Reproduction must bind the exact Reth source
revision and quote executable identity in addition to model `80160649…`. Old diagnostic reports
are retained as diagnostics; they must not be relabeled as new numeric quotes. Existing known-route
quotes retain their prior numeric behavior. The correction neither supports a new fork nor prices
an unrecognized instruction that executes successfully; such observations still require an
explicitly supported route and otherwise remain diagnostic.

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
  + access_list_construction_pair_tariff * raw_transaction_owner_low32_pairs
  + access_list_construction_other_pair_tariff * raw_transaction_owner_different_low32_pairs
  + access_list_address_pair_tariff * raw_transaction_address_item_pairs
  + code_preparation_event_tariff * nonempty_code_preparation_events
  + code_preparation_byte_tariff * prepared_code_bytes
  + storage_journal_work_tariff * (ordinary_journal_work_units + transient_journal_work_units)
  + account_journal_work_tariff * account_journal_work_units
  + account_cache_work_tariff * account_cache_work_units
  + storage_transition_work_tariff * storage_transition_work_units
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
gas, or transactions skipped without execution (zero-signer non-Anchor transactions and transactions
after zk gas exhaustion). Operation-route pricing omits the interpreter step whose legacy meter
charge first exceeds the budget; its independently captured ordinary/transient journal work and
potential Account-query budget are retained before that charge. For a truncated attempt, completed
steps and resources from the finalized journal
are retained. Completed precompile bodies remain observed even when their meter charge triggers
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
- actual `OpcodeNotFound` and `NotActivated` halts emit the existing `opcode:0xfe` feature-error
  price; other unknown routes retain a `collector_unknown_route` diagnostic.

Candidates accumulate by route across the committed block, then use the route's composition. The
same aggregation rules apply independently to the attempted prefix:

```text
Composition::Accepted:
    route_normal = accepted_absolute

Composition::Maximum:
    route_normal = max(accepted_absolute, incremental, structural)

route_dispatch = normal_dispatch_count * 64 PGU
route_total = route_normal + feature_error + route_dispatch
operation_price = sum(route_total for every committed route)
```

The maximum is applied after route-wide accumulation, not per event. `feature_error` remains a
separate accumulated field, although a single opcode event selects either its normal extraction or
its feature-error path. The separate mandatory `normal_dispatch_count` is one only for a normally
priced Opcode/ConfirmedSpawn event, otherwise zero. It includes Anchor and child execution, not just
user-code observations. Selected-but-undispatched wrappers and collector diagnostics add nothing;
precompile bodies do not add a second reserve after their caller's wrapper. Ordinary non-spawning
CALL-family steps that price successfully use their Opcode route and receive one reserve there.
Exceptional halts and successful steps with failed typed extraction use feature-error pricing and
receive no reserve; therefore this selector is not a count of every physical interpreter step.

`normal_dispatch_scaled` is an exposed subterm already included in `operations_scaled` and the
block total. Completion validates its sealed count/contribution; consumers must not add it again.
The count uses checked transactional accumulation and the existing selector bound of one billion.
Included REVERT retains its executed prefix; discarded/reset attempts appear only in attempted
snapshots. Candidate sums remain unchanged. Dispatch is added after their route maximum, so a
structural floor cannot swallow it. All arithmetic uses fixed-point units; there is no per-event
ceiling or second global discount.

### Candidate ECMUL incremental tariff

The current successor changes only `precompile:0x07.incremental_coefficient` to zero.
For each event `j`, let `g_j` be the actual consumed native gas and `W_j` the existing reduced-scalar
bit-length plus popcount selector. In PGU units, the route remains:

```text
A_j = max(20,000,000, 131.390224983920708487 * g_j)
S_j = 572,936 * W_j
PC07 = max(sum_j A_j, sum_j S_j)
```

The maximum is taken after summing across the block's route, including failures and reverted
work. It is not the sum of per-event maxima. Successful calls consume `g = 6000` and retain 20M
per-eventA. A failed off-curve call can burn its entire forwarded allowance: at `g = 200,000`,
A is 26,278,044.9967841416974 PGU. The accepted native slope first exceeds20M at
integer `g = 152,219`. Integer fixed-point arithmetic and final block/proposal ceilings are unchanged.

The [immutable evidence](../crates/evm/src/zk_gas/pricing/successors/ecmul-incremental-v1.json)
contains 33 opened current-program actuals: 18 successful calls at points G/2G with zero/two/dense
scalars, 6 intermediate scalar-work observations at W254/W255, 3 mixed inputs with 14 zero scalars
and one W507 scalar per repetition, and 6 caught off-curve failures at 6000/200,000 forwarded gas.
Every group retains its own 0/1/8 repetition controls. The mixture directly exercises the
route-aggregate crossover: at 15 calls, retained A = 300M and S = 290.478552M, rather than a sum of
per-call maxima. Caught failures check status 0, empty returndata and unchanged poisoned output,
while the parent transaction completes successfully.

The generator authenticates the e8 parent/evidence and replays raw
`floor(ExecutionReport.gas * 10 / 191)` endpoints. It removes the old route contribution in
scaled units, substitutes the new aggregate maximum, then applies each complete quote ceiling.
All 33 counterfactual full quotes cover actual, and all 33 within-group conditional increments
are covered by both node and full increments. Literal node deficits remain visible because
node quotes omit proposal-once startup. These contrasts include wrapper/scaffold work and
are not isolated per-operation cost measurements. No uniform extra margin or universal bound
is inferred. The former 179.406M floor for every successful call dominated small scalars even
though the scalar selector already protects heavy work; this finite evidence supports removing
that floor, not lowering the retained 572,936 structural coefficient.

The source manifest, research b76 ELF and SDK 6.3 execute-only identities are recorded separately
from the model's declared production ELF binding. These observations do not certify production
SDK/ELF parity, GPU proving time or an admission limit. Four point-3 actuals were reserved before
opening any fit actuals; they and fresh successor pressure/ordinary/natural requotes are external
validation, not inputs to this generated candidate. Their outcomes must be reported separately.

### Candidate SHA256 fixed floor

The inherited e8 successor changed only `precompile:0x02.accepted_absolute_model.constant`.
The accepted linear coefficient remains22.653487066193225602 PGU/native-gas, the incremental
coefficient remains21, and the structural coefficient remains42. Thus the effective PC02 quote
is `max(81024, 42 * native_gas)` PGU per call. Every other numeric tariff, route, wrapper,
memory charge, resource, ownership rule, arithmetic domain and consensus Unzen price is inherited
unchanged fromc087.

The [immutable evidence](../crates/evm/src/zk_gas/pricing/successors/sha256-floor-v1.json)
contains36 raw endpoint actuals and36 within-length/content-schedule count contrasts. They cover
64,4,096,1,048,576,1,269,664,1,269,665 and2,097,152 bytes; zero and`a5` schedules; and zero,
one and the retained full repetition count, with two user SHA calls per repetition. These are
already opened development observations. The four recent small-input regressions do not enter
the fit and are also already opened; none of these observations is an unopened holdout.

The [generator](../crates/evm/src/zk_gas/pricing/sha256_floor_successor.py) authenticates the saved
parent and evidence, normalizes actual PGU as`floor(raw_gas * 10 / 191)`, and rebuilds all count
pairs from endpoints. For each same-length/schedule pair it requires
`max(C, 42 * native_gas) >= (5/4) * max(0, delta_actual_pgu / extra_SHA_calls)`.
It subtracts no scaffold, loop, wrapper, memory or startup credit. It takes the greatest required
constant only among pairs whose margin exceeds the existing structural price, then rounds upward
to64 PGU. The binding zero-schedule4KB contrast has129,577 additional actual PGU across two
calls: the exact margin is80,985.625 PGU, yielding81,024 PGU. The unchanged42 slope already
covers the large-length contrasts. The generated floor is a finite empirical envelope, not an
isolated SHA cost measurement or a universal upper bound.

The evidence uses the retainedb76 ELF and CPU SDK6.3 gas estimator. Old/current observers match
on one retained common input; that finite parity check does not attest every observer output.
It establishes no production-program, GPU or proving-time parity. Fresh and historical repricing
are separate validation steps; the successor metadata records both as pending at generation.
Earlier sections' validation results retain their original model identities.

### Candidate ordinary-path marginal tariffs

The ordinary-tariff predecessor independently replays 46 authenticated same-program observations: 31 new fit
rows and 15 retained LOG4 rows. All use ELF `b76baf2e…`, SDK 6.3.0 and the `evm-gas-only` CPU
execute-only estimator. A common-input observation qualifies the earlier LOG4 observer: complete
execution report, raw gas, PGU, instruction/syscall counts and public output agree; only observer
executable/source/wall time and retained-event preset ordering differ. The portable qualification
summary retains original report and source hashes. Full raw inputs/execution files are outside the
Reth evidence. No reserved actual PGU entered fitting; the candidate was sealed before opening it.

Every pair with a nonzero event-count difference within the same operation/length/pattern is
recomputed from raw observations (34 pairs). Fixed prices are the positive maximum marginal PGU
multiplied by 5/4 and rounded upward to 64 PGU. LOG3/LOG4 obtain separate unrounded base maxima
from zero-length pairs. Their shared word maximum is
`max(0, (signed marginal PGU - corresponding unrounded base) / ceil(length/32))`, then the same
5/4 margin and 64-PGU rounding apply separately to bases and words. Every derived price covers
its pair increment with the 25% margin. Variable loop/operand/check costs stay in the increments;
scaffold observations receive no subtraction. STATICCALL includes one warmup at every N, so its
incremental event count is the N difference. Retained LOG4 has two users and 2N events.

Only the five accepted models change. CODESIZE retains exact raw gas 2, CODECOPY retains maximum
raw gas 12 and its original fallback above that envelope, and STATICCALL retains the spawn-memory
shape. LOG3/LOG4 use existing `log_piecewise` evaluation with zero topic/common-dispatch terms,
separate base constants and the same word coefficient for nonempty data and additional words.
Shared memory, independent 64-PGU normal dispatch, incremental/structural/feature-error terms,
maximum composition, all domains, ownership, fixed/startup costs, discount and Unzen remain exact.
In particular, the STATICCALL accepted coefficient does not replace its larger inherited 18,291-PGU
incremental candidate. These are conservative finite empirical prices, not universal bounds or
production SDK/GPU parity. Reserved and retained full-quote/node-block-sum comparisons remain
separate downstream checks; the opened historical cohort is development/regression evidence.

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
- `opcode:0x39` (CODECOPY) uses a flat 1,600 PGU accepted body inside its raw-gas envelope and switches
  to a raw-gas fallback above it. The transition is not monotone.
- LOG1 through LOG4 use the LOG-piecewise family. LOG0 remains a raw-gas linear floor; LOG3 and
  LOG4 use separately measured base prices and a common per-word price plus shared memory.
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
| `opcode:0x32` (ORIGIN), `opcode:0xfd` (normal REVERT) | 5,000,000 |
| `opcode:0x38` (CODESIZE, exact raw gas 2) | 2,048 |
| `opcode:0x39` (CODECOPY, raw gas at most 12) | 1,600 |
| `opcode:0xa3`, `opcode:0xa4` (LOG3/LOG4) | 45,888 / 64,064 base + 2,688 per data word + shared memory |
| `opcode:0x3b` (EXTCODESIZE) | 1,000,000 |
| `opcode:0xf1:confirmed_spawn_wrapper` (CALL) | 1,000,000 plus shared memory |
| `opcode:0xfa:confirmed_spawn_wrapper` (STATICCALL) | 9,280 plus shared memory; inherited 18,291 incremental candidate still participates in maximum |
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
- an actual `OpcodeNotFound` or `NotActivated` halt uses INVALID's authenticated feature-error
  price once, including failures in caught child frames;
- other unknown routes produce a collector diagnostic rather than silently gaining a fallback
  price; no successful future instruction is supported by exceptional-halt normalization;
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

The checked-in commands below reproduce the current successor JSON, acceptance record and Rust table without
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

### Historical bbce controlled measurements

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

## Candidate normal-dispatch reserve (bbce to c8936124)

The finite development panel comprises 24 ADD/MUL composite loops, 12 paired DUP/POP controls,
and 24 balanced stack loops, including every N0 control: 60 distinct measured proposal inputs and
60 matched increments. Their independent panel SHA256 is
`7c6f4ce7cfa98ddbcabbaeabdccb30b7088666ef22c96b80e3b946e68b65a7c3`; the separate exact-rational audit is
`69d3a6ab96c756035a4f0d369b2993894254122481f994bdda6b4a03b2560e72`.
The exporter retains their original canonical-input, native, parent-quote and SP1-report identities.

```text
R = ceil_to_multiple_of_8(max(0, max_pairs(
    (5/4 * actual_PGU_increment - parent_node_scaled_increment / 1e18)
    / extra_completed_user_opcodes))) = 64 PGU
```

The exact largest threshold is `5844273401172960045399 / 100100000000000000000`, approximately
58.3843496621 PGU/event. The count deltas cover the actual composite opcode streams, including
DUP/POP and loop scaffolding; they do not identify isolated dispatch, ADD, MUL, DUP or POP cost.
No claim is made that the original Taiko calibration lacked metering. No prospective actual is used
to select 64. Existing calibration authorities and validation records are inherited, not reexecuted.

User completed-opcode counts exclude Anchor. Candidate same-input native replay must independently
confirm that full-block normal-dispatch count differences equal these qualified user deltas before
interpreting the fitted margin. The exporter does not claim that replay has occurred. Literal
actual/node-block-plus-sum and actual/complete-proposal values, including startup/control deficits,
remain separate primary comparisons; matched increments are explanatory diagnostics only.
This candidate does not allocate proposal-only startup into mining, establish a universal bound,
change consensus Unzen, or activate admission. New empirical validation must preserve both
underpricing flags and original deficits instead of replacing them with projected success.

## Reproduction and change policy

Verify the checked JSON address and generated Rust table with:

```bash
env -u PYTHONOPTIMIZE ~/.venv/bin/python -S -B -m unittest discover -s crates/evm/src/zk_gas/pricing -p '*_test.py'
env -u PYTHONOPTIMIZE ~/.venv/bin/python -S -B crates/evm/src/zk_gas/pricing/ecmul_incremental_successor.py --check
env -u PYTHONOPTIMIZE ~/.venv/bin/python -S -B crates/evm/src/zk_gas/pricing/generate.py --check
```

Regenerate the reviewed current successor and its Rust table with:

```bash
env -u PYTHONOPTIMIZE ~/.venv/bin/python -S -B crates/evm/src/zk_gas/pricing/ecmul_incremental_successor.py
env -u PYTHONOPTIMIZE ~/.venv/bin/python -S -B crates/evm/src/zk_gas/pricing/generate.py
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

## Ordinary-storage journal observation and comparison reserve

`ZkGasBlockSnapshot::storage_journal` and `ZkGasShadowReport::storage_journal()` expose
`StorageJournalDiagnostics`. Its comparison work has an independent additive block-local tariff;
its older occupancy counters remain unpriced, and its five resource selectors are unchanged.
The shared plain/inspected runtime capture reads the length of the already-referenced storage
owner's transaction-local `JournaledAccount.storage` map. It adds no database or storage-key lookup.
`target_address` identifies the storage owner, including DELEGATECALL; code ownership is irrelevant.
Disabled collection adds no owner ledger. Declared access-list keys, finalized resource walks and
TLOAD/TSTORE do not emit these events.

A potential SLOAD/SSTORE event with valid identity and footprint contributes `n_before + 1` direct
occupancy units. These are conservative potential events, not actual guest probes, hash comparisons,
PGU or a proved PGU bound. Static, stipend and OOG failures may be counted before a physical lookup;
missing account/footprint metadata is explicitly incomplete. Stack operand underflow is known
pre-lookup failure and contributes zero journal work; the feature-error opcode tariff still applies.
Zero-valued slots occupy the map too.
Warmness is independent of occupancy: an access-list-warm uncached slot is vacant, while a slot
retained after frame rollback can be cold and occupied.

The potential rollback reserve counts one entry per cold potential access and one per potential
SSTORE change. Every SSTORE is conservatively treated as potentially changed, including noops and
failed writes. At transaction finalization, each owner's entry count is multiplied by that owner's
high-water upper `max(n_before + 1)`, then added across owners and transactions. This allows an early
entry to unwind after later map growth. It is a potential reserve even when no actual revert occurs.
Frame and included outer REVERT/HALT retain work. Rejected transactions enter only the attempted
summary; owner high-water and entry state are discarded at each commit/reset. An executed storage
step that itself crosses the legacy Unzen limit is included in attempted journal work even
though the existing priced operation route remains excluded. Both actual execution paths observe
that potential work exactly once before the legacy charge, independently of priced-step finalization.
Ordinary transaction finalization clears the physical journal map; persisted database values do not
carry its occupancy into the next transaction.

All count, conversion, sum and product arithmetic is checked. `incomplete` retains the first missing
metadata or overflow failure alongside partial values; consumers must not use an incomplete summary
as a complete statistic or bound. A true missing footprint or overflow now prevents publication of
a valid known subtotal with a typed `StorageJournal` diagnostic. These failures do not change EVM
execution, legacy gas, feature-error tariffs or legacy Unzen rejection timing.

The same ledger exposes a conservative low32 comparison envelope. At the start of each observing
transaction, physically cached journal slots seed their owner's prefix groups once. A captured
SLOAD/SSTORE whose existing cached-slot result is absent adds one potential materialization event;
repeated occupied accesses add only their lookup count. This reuses the pre-step cached-presence
fact and adds no storage-key lookup or full-U256 dedup set. Failed vacant attempts or clear/refill
may count the same key more than once; these are deliberately conservative event counts, not exact
distinct-ever-seen counts. Events are never removed when a frame reverts or an account clears.

For each owner, `N` is its initial-slot-plus-materialization event count, and `M` its largest group
sharing the raw slot's low 32 bits. `comparison_equivalents_high_water_upper` is the maximum owner
`C = ceil((N + 7*M)/8)`. The profiled little-endian U256 comparator reads the low 32 bits in its
first four memory bytes. A candidate with a different four-byte prefix inspects at most four byte
pairs, while one matching it is conservatively charged all 32: `U = 4*N + 28*M`, and `C = ceil(U/32)`.
The observer extracts `key.as_limbs()[0] as u32` numerically, without depending on the node's native
pointer layout. Its comparator interpretation is tied to the profiled guest representation and
must be rechecked when that representation or comparison implementation changes.
This envelope does not reconstruct or measure actual hash-bucket collisions. In particular, a
dispersed table whose raw keys share a low32 prefix is deliberately overestimated too.

At transaction finish the ledger adds `C * (A + N + J)` per owner into
`comparison_work_units_upper`, where `A` is its potential SLOAD/SSTORE count and `J` the existing
cold-plus-SSTORE potential rollback-entry reserve. It separately reports additive
`materialization_count_upper` and the largest `max_low32_group_upper`. The low32 counters use
ordered integer keys; regular warm repeats do not update those groups. Initial physical seeding is
Off-gated, reads only existing journal state, and neither performs a DB load nor substitutes the
declared access list or witness set for actual cached storage.

The bound is only on compared bytes in one hypothetical candidate scan testing each distinct key
at most once. It does not prove the number of scans, fixed comparator or probe overhead, growth or
rehash work, or a full PGU coefficient. Short-prefix hash concentration, lifecycle integration and
normal historical compatibility therefore remain separate verification requirements.

The historical successor `storage_journal_successor.py` authenticates parent `c8936124…`, the saved
three-row comparison evidence, and every unchanged inherited numeric price. It derives **384 PGU
per comparison work unit**, stored as `384 * 10^18` without another discount. The coefficient is the
maximum positive `(whole-input actual PGU - old literal node PGU) / work units` over concentrated
zero, long-prefix and short-prefix inputs, multiplied by 5/4 and rounded upward to a multiple of 64.
The abandoned 256 candidate underpriced the short-prefix actual by 101,266,793 PGU; that negative
observation remains sealed. The 384 candidate predicts 1,016,067,876 PGU for 831,917,069 actual PGU.
These b76/SDK6.3 controlled whole-input measurements do not certify production SDK parity or a
universal PGU upper bound. Deliberately dispersed inputs sharing a raw low32 prefix can be heavily
overquoted; the saved control projects about 33.3x actual under this conservative simple rule.

`storage_journal_work_units` and `storage_journal_scaled` are separate immutable known-breakdown
fields. The known subtotal adds their price once outside opcode-route maxima; exact resource/MPT
completion revalidates the raw units and price and retains that same contribution once. Direct
complete-block pricing follows the same rule. Proposal-once pricing adds no second journal term.
Committed node snapshots exclude rejected transactions; attempted snapshots retain their executed
journal work for proving consumers. Partial/incomplete observations are never interpreted as zero.

Only this component receives a dedicated **1,000,000,000,000 work-unit domain bound**. Exceeding it
returns typed `SelectorOutOfRange` for `storage_journal_work_units`; the existing ordinary selector
bound remains 1,000,000,000. The declared signed-i128 worst-case fixed-point envelope adds
`384 * 10^18 * 10^12` to the inherited coefficient/selector/term envelope. All runtime arithmetic
remains checked; the dedicated bound does not claim every valid future workload fits the domain.


## Transient-map reserve and combined reporting

Transient storage is a separate transaction-local `AddressMap<StorageKeyMap<U256>>`, not a
persistent storage resource or witness/MPT target. The current hook captures valid TLOAD/TSTORE
operands before execution, using `interpreter.input.target_address()` for storage ownership and
`peek(0)` for the key (`peek(1)` for the TSTORE value). DELEGATECALL and CALLCODE therefore use the
caller's storage owner. It reads the existing inner-map length; only a nonzero TSTORE candidate
checks whether the full key is physically absent. TLOAD adds neither a full-key seen lookup nor a
persistent resource observation. Disabled collection performs no transient capture or seeding.

The independent `TransientStorageJournalLedger` reuses the checked comparison arithmetic but gates
materializations through a transaction-local `BTreeSet<(Address, U256)>`. For each owner:

- `A` counts every valid-operand TLOAD/TSTORE attempt, including execution failures.
- `J` counts every potential TSTORE entry, including noops, static and OOG failures; there is no cold
  transient entry.
- `N` counts distinct full-U256 keys first potentially materialized by a vacant nonzero TSTORE, plus
  physically present initial keys. `M` is the largest raw-low32 group among these distinct keys.
- `C = ceil((N + 7*M)/8)` and `U_transient = sum(tx, owner) C*(A+N+J)`.

A TLOAD miss or absent-key TSTORE0 must not enter the seen set: a later nonzero store of that key
still grows N/M. Clear and frame/root rollback never remove seen keys. Initial physical transient
keys seed both seen and N/M once under the observing gate, without DB access. Normal REVM
`JournalInner::commit_tx`, `discard_tx` and `finalize` clear transient storage; finalizing the
observer charges its owner scopes before dropping them, so a new transaction starts independently.
The public context's manually preseeded transient state is also observed rather than assumed empty.
Repeated same-key `TSTORE1; TSTORE0` for R rounds has N=M=C=1, A=J=2R, and U=4R+1; it stays linear
under clear or caught revert. Persistent storage deliberately keeps its earlier conservative
materialization-event semantics and is unchanged.

`ZkGasBlockSnapshot::transient_storage_journal` and
`ZkGasShadowReport::transient_storage_journal()` expose the independent diagnostics.
`ZkGasBlockSnapshot::storage_journal_work_units()` returns the checked combined ordinary plus
transient units. The older `.storage_journal` field/getter remains **ordinary-only**; consumers must
use the combined method or the sealed known breakdown, rather than copy that field as the complete
priced selector. Both diagnostics must be complete. Missing metadata, overflow or a combined count
above 10^12 prevents a valid known subtotal; partial values never become a zero-price fallback.
Valid-operand attempts remain visible through legacy-limit crossing, root halt and caught child
revert. Operand underflow is known pre-lookup zero work and retains the existing feature-error tariff.

The serialized `storage_journal_work_units` is this combined selector. Its one
`storage_journal_scaled = 384 * 10^18 * combined_units` contribution appears once in the node known
subtotal and once in full block completion, retained rather than added again. Proposal-once adds
zero. Because the existing aggregate domain is shared, no additional i128 term or bound is added.
All operation/memory/payload prices and the single inherited global-discount treatment are unchanged.
This is Shadow-only and does not change Unzen execution, legacy limits or header difficulty.

The sealed evidence retains eight actuals from `unzen-transient-read-catalogs-v1` (ELF b76, SDK6.3),
one explicitly skipped high32/full actual, and nine native/5f-quote-qualified inputs. The immutable
baseline asset SHA256 is `86dbe6db4532fe08bc01ba2d8199346295a2da253946a139c7474fbdd63d509f`.
The same 384 coefficient is reused, not fitted on this panel. Baseline model 5f is the prior **new**
model, not Unzen. The following are formula projections, not fresh quotes from the changed runtime:

| Controlled sample | Actual PGU | Baseline node PGU | Added transient units | Projected node PGU |
| --- | ---: | ---: | ---: | ---: |
| default/zero | 165,384,479 | 35,425,248 | 787,456 | 337,808,352 |
| default/r64 | 168,650,021 | 71,148,121 | 2,884,608 | 1,178,837,593 |
| default/r512 | 191,508,491 | 321,208,226 | 17,564,672 | 7,066,042,274 |
| high32/zero | 190,167,021 | 35,425,248 | 787,456 | 337,808,352 |
| high32/r512 | 3,244,570,251 | 321,208,226 | 17,564,672 | 7,066,042,274 |
| low32/zero | 170,357,529 | 35,425,248 | 99,970 | 73,813,728 |
| low32/r512 | 698,710,846 | 321,208,226 | 2,229,890 | 1,177,485,986 |
| low32/full | 2,417,370,218 | 1,252,235,583 | 9,168,770 | 4,773,043,263 |

The motivating high32/r512 deficit was 2,923,362,025 node PGU at 3,860,903 block gas. Its projection
adds 6,744,834,048 PGU and is about 2.178x actual. The deliberately constructed default/r512 projects
about 36.9x actual; no normal-mainnet 2x claim follows from this panel. Low32/zero retains a
96,543,801 node-only deficit; its projected complete-proposal estimate covers actual. Finite own-input
hash/profile qualification does not prove all dynamic placements/comparisons, a universal PGU bound,
production SDK parity, or a full 30M-gas workload bound. Historical zero-transient replay and broader
compatibility remain separate checks, not implied by this source change or its model identity.

## Retained model389 access-list prefix supplement

REVM constructs a warm `HashSet<U256>` from each non-legacy transaction's raw access list before
executing its opcodes. Declared keys need not be read, written or materialized as persistent storage.
The physical journal ledgers therefore do not cover this construction, while the existing raw
address-item, signed-payload and unique-resource terms grow linearly. A concentrated generic-hash
family exposed a separate high-priority deficit even with an otherwise empty RETURN transaction.

The new observing path captures the full list once in the common plain/inspected transaction
bracket. A scratch `BTreeMap<(owner, low32), raw_count>` adds each previous group count when a key
is seen, then is dropped before EVM execution. The selector is:

```text
construction_pairs = sum(transaction, owner, numeric_low32) n * (n - 1) / 2
construction_scaled = 384 * 10^18 * construction_pairs
```

`low32` means the final four big-endian bytes of the signed B256 key, converted to numeric U256;
these are the first four comparison bytes in the measured little-endian U256 representation.
Repeated address items for one owner share a group within a transaction. Different owners and
transactions have independent groups, regardless of block-level resource deduplication. Raw
duplicate keys intentionally remain occurrences: a deliberately duplicated list may overquote
quadratically. There is no second full-U256 set and no rolling journal or checkpoint hook.

`AccessListConstructionDiagnostics` reports raw address items, raw storage keys, largest raw
low32 group, comparison pairs and any incompleteness. The transaction capture receipt is merged
into both prefixes when included, or attempted only when reset/rejected. An included REVERT keeps
the construction work. Missing nonempty capture, duplicate capture, item-count mismatch and
checked arithmetic/domain failures prevent a valid quote; partial diagnostics remain inspectable.
Zero-item scopes are known empty. Disabled collection does not allocate or traverse the new map.

The independent `access_list_construction_pairs` domain is capped at `10^12`; the ordinary
`10^9` selector domain and journal domain are unchanged. Known reports expose
`access_list_construction_pairs()` and `access_list_construction_scaled()`. Complete block reports
retain both fields, the runtime block log emits them, and completion rechecks their contribution.
This supplement is added exactly once outside opcode route maxima, resources and journal pricing.
Proposal-once contributes zero; the normalized coefficient receives no further discount.

The [sealed finite evidence](../crates/evm/src/zk_gas/pricing/successors/access-list-construction-v1.json)
retains eight native/parent-quote-qualified inputs, five actuals and three unmeasured controls.
G0 and family qualification belong to each input: the unrealized K7680 control cannot certify a
concentrated K7680 workload. All values below are whole-input measurements and formula projections,
not isolated comparison coefficients or fresh quotes from the changed runtime.

| Sample | Block gas | Actual PGU | Parent node PGU | Added PGU | Projected node PGU |
| --- | ---: | ---: | ---: | ---: | ---: |
| reference64, unrealized | 312,361 | 161,458,895 | 8,655,989 | 774,144 | 9,430,133 |
| own K1024, unrealized | 2,136,361 | 167,736,345 | 25,551,989 | 201,129,984 | 226,681,973 |
| aligned K1024, realized | 2,136,361 | 266,428,229 | 25,551,989 | 201,129,984 | 226,681,973 |
| large K7680, unrealized | 14,782,761 | 210,667,979 | 142,698,613 | 11,323,146,240 | 11,465,844,853 |
| aligned K7680, realized | 14,782,761 | 5,773,513,175 | 142,698,613 | 11,323,146,240 | 11,465,844,853 |

The realized K7680 node deficit is 5,630,814,562 PGU within the 30M-gas priority budget. Its parent
full proposal quote is 297,640,444 PGU; the projected successor is 11,620,786,684 PGU, approximately
2.013x actual, while the projected node is approximately 1.986x. The realized K1024 node retains a
39,746,256 PGU residual deficit, below the 2B priority threshold; its projected full proposal is
381,623,804 PGU and covers actual. No startup subtraction changes the literal node denominator.

The retained389 evidence above is an empirical long-prefix supplement. The subsequent
different-low32 observation and policy are recorded below. Neither term proves a universal
hash-table or PGU bound; control probing, resize and allocator costs remain distinct dimensions. The crafted unrealized K7680 overquote is deliberately
retained. Normal historical compatibility must be evaluated separately before claiming the prior
mainnet overestimate distribution is unchanged. The evidence uses the retained b76 ELF and SP1
SDK6.3 gas estimator; it does not establish production SDK/ELF parity or GPU proving time.


## Different-low32 access-list construction pairs

The current capture maintains two small transaction-local scratch count maps: previous raw
occurrences per declared owner and per(owner,numeric low32). Repeated owner items share that
scope; repeated identical keys remain occurrences. For each new occurrence, low-pair delta is
seen_bin and other-pair delta is seen_owner−seen_bin. All arithmetic is checked before updating
receipt/counters, including seen_bin<=seen_owner. No guest hash seed, key recipe, full-key dedup,
opcode, physical-storage target or cryptographic operation is introduced.

```text
P_low   = sum(tx,owner,low32) choose(raw_count,2)
P_owner = sum(tx,owner) choose(raw_count,2)
P_other = P_owner − P_low
construction_scaled = 10^18 * (384*P_low +96*P_other)
```

These are genuine unordered raw-occurrence pairs, never weighted work units. Diagnostics expose
comparison_pairs (unchanged low32), other_pairs and owner_pairs, with the checked identity
owner_pairs=comparison_pairs+other_pairs. Different owners and transactions do not cross-pair.
Finalized scopes merge additively; they never recompute choose2 from aggregate key counts.
All existing missing/duplicate capture, item mismatch and sticky failure rules remain. Off mode
still bypasses capture entirely. Included successful/REVERT/halt transactions contribute to the
committed node prefix; rejected candidates contribute only to attempted prover pricing.

The independent other-pair domain is10^12; the low32 domain stays10^12 and generic selectors
stay10^9. Public raw/scaled fields are access_list_construction_other_pairs and
access_list_construction_other_scaled. Sealed known reports retain both, completion independently
reprices them, Raiko transports them without zero defaults, and comparison checks the once-only
node/prove subtotal. The other component is zero at proposal_once and receives no second discount.
The block term count rises21→22 before route terms. The i128 declaration conservatively adds
96*10^18*10^12 to the parent's declaration; all inherited arithmetic bounds remain unchanged.

For the final two-user measured input, P_low=0 and P_other=58,974,720 from two independentK7680
scopes. Added reserve is5,661,573,120PGU; arithmetic-only projected node is5,869,373,955 and full
proposal6,024,315,785. These are not fresh quote results. The whole canonical actual remains
2,484,257,836PGU; 400M-prefix diagnostics only qualified the first observed map and do not
certify the other map or full comparison counts. The valid canonical reference is retained with
whole-input pressure unknown; its first observed local call was unrealized.

The finite actual ELF isb76/SDK6.3/observerdfa, separately recorded from the inherited calibration's
ba5ab program identity. No new actual or proving-time claim is made. Large ordinary well-distributed
lists also pay the conservative different-prefix reserve: K64 adds193,536PGU, K1024 adds50,282,496,
and K7680 adds2,830,786,560. Thus normal compatibility and historical sensitivity must be measured;
low32pairs0 never implies other_pairs0. The fixture tag fixture-unzen-access-list-short-pairs-v1
is planned at model generation, without implying it has already been published. Every raw actual,
old389 quote and deficit remains immutable while successor quotes are collected separately.


## Raw access-list address-item pairs

Outer warm-owner table construction also traverses address items whose storage-key list is empty.
The inherited inner-key receipts cannot represent this work: `owner_pairs` counts pairs of storage
keys within one declared owner, not pairs of address items. The independent receipt now retains
`raw_address_pairs`; it adds the previous raw item count for each incoming item, without another
map, hash predictor or deduplication. Duplicate addresses and empty-key items count equally.

```text
P_address = sum(tx) choose(raw_address_items_in_tx, 2)
address_scaled = 128 * 10^18 * P_address
```

Only already-finalized transaction pair counts are summed across a block. In particular, two lists
of6144 items produce37,742,592 pairs, rather than choose2(12,288). Included success, REVERT and halt
receipts retain this preexecution work in committed node pricing; rejected/discarded scopes remain
in attempted prover pricing. The existing Off gate bypasses the capture traversal and scratch
allocation. Missing/duplicate/count-mismatched captures, arithmetic overflow and domain failures
remain sticky diagnostics, never valid zero prices.

`access_list_address_pairs()` validates capture completeness, agreement with the captured raw item
metadata and the independent pair domain10^12. Known reports expose the raw selector and
`access_list_address_scaled()`; complete `PricingBreakdown` values and block logging retain both.
Completion independently reprices the raw selector and rechecks the sealed subtotal. The term is
added exactly once outside route maxima, physical resources, journals and the global discount;
proposal-once raw/scaled fields are zero. The fixed block term allowance increases22→23 and the
mixed-domain i128 declaration adds128*10^18*10^12. Every inherited price, including384 per same-low32
key pair,96 per different-low32 key pair and8192 per raw address item, remains unchanged.

Three frozen b76/SDK6.3 whole-input measurements motivate this empirical conservative reserve.
They share the inherited c306 model; no new actual measurements were requested to generate it.

| Sample | Charged block gas | Actual PGU | Parent node PGU | Parent full PGU | Address pairs | Added PGU |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Normal two-user raw list |228,567|161,241,694|7,855,879|162,797,709|12|1,536|
| First large reference |29,700,567|239,384,690|255,459,331|410,401,161|37,742,592|4,831,051,776|
| Sole same-shape successor |29,700,567|3,009,978,952|255,459,331|410,401,161|37,742,592|4,831,051,776|

The successor's parent-model literal node/full deficits are2,754,519,621/2,599,577,791PGU.
Subsequent fresh quotes from model69d2 produce node/full5,086,511,107/5,241,452,937PGU,
about1.69x/1.74x actual. These are retained quote outputs, separately validated after model generation.
The intentionally constructed reference receives the same quote, becoming a substantial overquote
that remains visible in the fixture. The normal fresh node/full quote is7,857,415/162,799,245PGU;
its remaining node deficit is153,384,279PGU. No startup subtraction modifies a denominator or actual.

Retained-input regression used65 fresh quotes:15 controlled inputs (12 b76/SDK6.3 actuals and3
explicitly unmeasured), plus50 historical proposals/9,600blocks. Every historical block/proposal
numeric component and public output is unchanged. All12 measured full quotes cover actual;
no measured <=30M-gas input retains a positive >2B-PGU node gap. Historical ELF416f/SDK6.1 results
are sensitivity checks, not current b76 accuracy certification. No canonical inputs were rebuilt
and no new SP1 actuals were run.
The128 tariff remains an empirical margin and does not establish a bound for untested compositions.

Address20 hashing uses its actual network-order bytes and the Fb20 path, distinct from generic
U256 hashing and fixed32 storage-journal hashing. Local own seed/shared-state, allocation shape
and comparator layout affect measured work. The reserve does not assume that low32 alone explains
outer owner cost or that old input hash parameters transfer to a new input. The successor's15
closed local calls confirm ownG and equal H within a400M diagnostic prefix. Whole-input pressure,
transaction identities and full comparison counts remain UNKNOWN; complete canonical PGU is
independently valid and cannot be attributed entirely to one observed map.

The deterministic successor archives immutable parentc306 and sealed three-row evidence in
`successors/access-list-address-pairs-v1.json`. Its fixture tag
`fixture-unzen-access-list-address-pairs-v1` has publication status at model generation `planned`,
without asserting a release already exists. Raw observations and parent quotes remain unchanged.
Historical sensitivity, fresh quotes, production SDK/ELF parity and normal-transaction admission
claims require separate evidence. The128PGU tariff is not a universal bound or an isolated fitted
per-comparison cost; the original384/96 coefficient domains and semantics are preserved.


### Empirical Account-journal density reserve

Model69's larger Account-query successor measured 5,073,040,910 PGU at 10,055,942 charged block
gas against literal node 2,380,285,007 / full proposal 2,535,524,054. Its reference actual
271,360,001 is retained, including deliberate overquotation. Fresh retained-input quotes now cover
both this successor and the older K512/R1400 approximately1.69x sample, closing that specific
sample's pricing TODO without closing every Account path. Whole-input pressure,
transaction identity and complete placements are not established by 63 closed local calls /46
candidate calls. No third input, new SP1 actual, guest, ELF, observer or SDK build is required.

The independent Account component uses the real finalized journal map, including physically cached
absent, untouched and frame-reverted Account entries; declared-but-unread AL owners are not that
population. Once per observing transaction, N is its physical entry count and M its largest group
sharing the first four **network-order Address20 bytes**. C=ceil((N+4M)/5) is a twenty-byte candidate
comparison proxy. It is not a guest hash/probe count or a whole-program computational upper bound.
A final high-water population intentionally overweights earlier smaller-table work; custom
inspectors that remove physical keys are outside the standard journal's append-only argument.

The sealed `account_journal_policy` assigns potential forward/unwind budget Q per executed opcode
attempt:2 for BALANCE/EXTCODESIZE/EXTCODECOPY/EXTCODEHASH/SELFBALANCE;4 for SLOAD/SSTORE;32 for
CREATE/CALL/CALLCODE/DELEGATECALL/CREATE2/STATICCALL/SELFDESTRUCT. All other opcodes, including
TLOAD/TSTORE, add0. The generated 256-entry table is the runtime source of truth. Early failures
and a legacy-rejected step retain their potential budget; ConfirmedSpawn does not count it again.
These are empirical allowances, not exact physical-query counts. There is no small-table threshold
and no surcharge on unrelated arithmetic. Frame rollback never subtracts already performed work.

`AccountJournalLedger` and `AccountJournalDiagnostics` retain Q, physical N/M, C, finalized work
U=C*(Q+N), completed receipt count and first sticky incompleteness. `PendingPricingStep` observes
Q in both execution paths before legacy rejection; `TaikoZkGasEvm::transact_raw` supplies exactly
one physical population after journal finalization and before propagating handler errors.
`ZkGasAccumulator` folds finalized per-TX units into committed/attempted prefixes, never a product
of block-wide sums. Missing, duplicate, late or overflowing captures fail closed. Known empty
population is explicit; no-transaction prefixes are complete zero. Off/suspended modes perform
neither new opcode capture nor prefix allocation/traversal.

The tariff is128 PGU per finalized work unit, independently bounded at10^12 and normalized once,
without a second discount. The sealed empirical derivation is four times the positive node deficit
divided by user-only lower basis137,365,504, rounded upward to a64-PGU multiple. This does not
calibrate an isolated comparison or prove universal/production accuracy. Raw
`account_journal_work_units` / normalized `account_journal_scaled` are separate from ordinary and
transient storage, all AL terms and their unchanged384/96/128 coefficients. Node uses committed
scopes, prove retains attempted scopes, and proposal-once units/price are zero. All immutable known,
exact and consumer subtotal checks include the new term exactly once.

The74 fresh quotes retain24 controlled inputs (21 original actuals and3 explicit nulls), plus50
historical proposals/9600blocks. Repricing performed0 native constructions,0 new SP1 actuals and0
same-input retries. All21 measured full-proposal quotes cover their original actuals. Four positive
literal-node gaps remain, maximum153,358,679 PGU; none exceeds2B. This is finite regression evidence,
not complete node coverage, normal-transaction admission or a universal bound.

| Retained case | Original actual PGU | af30 literal node PGU | af30 full proposal PGU |
| --- | ---: | ---: | ---: |
| N1024/R1024 reference |271,360,001|19,980,657,871|20,135,904,598|
| Sole N1024/R1024 successor |5,073,040,910|19,980,657,871|20,135,896,918|
| Original K512/long R1400 |5,952,586,609|15,387,637,502|15,542,727,812|

The two new inputs each capture137,502,913 Account units, adding17,600,372,864PGU at128/unit;
these are real captured selectors and fresh quotes, superseding the lower-basis projection.
Their large crafted overquotes, including the cheap unrealized reference, remain explicit.
For67 rows compared with parent69, every non-Account numeric component and public output is
unchanged. The other7 original Account baselines are5f, not69: their full5f→af30 change must not
be labelled an Account-only parent69 delta.

All50 historical proposals and9600blocks change. Literal node total92,561,066,464→92,823,492,832
(+0.2835%); full159,704,576,845→159,967,003,341(+0.1643%). Maximum proposal-node uplift is0.5106%,
maximum block-relative uplift0.8290%, and maximum absolute block uplift1,543,168PGU (these last
two maxima occur on different blocks). Historical416f/SDK6.1 observations establish model
sensitivity only, not currentb76 accuracy or production SDK certification. The full-versus-node increase differs
by128PGU because one retained discarded attempt adds one Account unit: block13,218,481/index76
has184 committed-node versus185 attempted-prove units. This is scope accounting, not rounding.

Validation includes independent269 actual-source behavior tests,33 fresh-ABI trace tests and125
default CLI integration tests. Review found and corrected installed-collector pollution during a
systemcall between finalized execution and commit: the complete plain/inspected collector is now
suspended and restored on Ok/Err without changing legacy metering. The population test now uses
a real inspector factory and counted callback; the earlier ignored toggle is not inspector
coverage. The E3 production build retains its own source identity; E4 only supplies a separate
test-source binding for required JSON fields, without relabelling E3 production artifacts.
Installed clippy-driver1.95 also passes the10-node host metadata closure with -Dwarnings; its3
Reth nodes additionally check missing_docs and missing_docs_in_private_items. Frozen host
features/env/dependencies apply; this is not full-workspace/all-features Cargo clippy.

External caller validation, authority processing, top-level frame and fee paths still have
unmeasured residual work; Q+N does not claim to count every such access. Native-gas frequency limits
do **not** prove these residuals below2B PGU. Source evidence and the immutable69 parent are sealed
by `account_journal_successor.py`; its planned fixture publication is a generation-time status.
The new data-only fixture uses `unzen-account-journal-density-v1`. The sealed generation-time
plan names `fixture-unzen-account-journal-v1`, which was already occupied by the older seven-input
5f fixture; that immutable release is not replaced or used as the new evidence download. The new
fixture's asset metadata and checksums identify its separate density release. Its publication is
still pending here; the sealed model/evidence and original quotes are not rewritten for a plan-name
correction. No guest, ELF, SDK or observer was
rebuilt, and full workspace Cargo/clippy or guest CI success is not claimed by these host checks.

### Candidate: progressive observed account-cache work

The warm-first finite panel exposed a block-lifetime gap after the Account-journal reserve:
86 user transactions each warm and query128 globally disjoint absent accounts. Both retained
inputs use29,669,999 charged block gas. With the immutable `af30c616…` model, both report
1,354,594,058 literal node PGU. The reference actual is338,631,603 PGU; the sole same-shape
successor actual is5,027,210,167 PGU. The successor's positive actual-minus-node gap is
3,672,616,109 PGU; its full-proposal estimate1,519,000,777 also underestimates actual.
These original columns remain `af30` results. Once used to develop this supplement, the two
opened actuals are calibration/stress evidence, not an unopened accuracy holdout.

The existing Account component finalizes its population separately per transaction. The backing
`State.cache.accounts` persists across transactions in the same block. A freshly finalized
Journal can therefore contain a small cohort while its first account loads query a much larger
State cache. This candidate adds one separate block-aware proxy; it preserves all old Account,
access-list, opcode, memory, resource, MPT and proposal-once tariffs and the existing discount.

For each transaction `t`, retain the actual finalized Journal account keys `J_t`. The committed
and attempted block prefixes each keep their own union `S_t` and first-four-network-byte group
counts. Define:

```text
N_t = |S_t|; M_t = largest first4 network-byte group in S_t
C_t = ceil((N_t + 4*M_t) / 5)
B_t = 2*|J_t| + persistent_storage_materialization_count_upper_t
account_cache_work_units = sum_t(C_t * B_t)
account_cache_scaled = account_cache_work_units * 128 * 10^18
```

The initial128 PGU/unit is an empirical conservative candidate, not a universal PGU upper bound.
Its independent unit domain is10^12. The2N budget allows one possible State basic lookup and one
possible commit entry per physical account; the persistent-storage materialization upper adds
possible State storage-owner lookups. It intentionally overcounts untouched commit entries.
Exact locked `revm-database43.0.0` source places the account-map entry in these paths; the sampled
guest's source links identify43.0.1, whose relevant entry paths agree. This is source reasoning,
not a claim that every actual PGU is attributable to this map or these operations.

The retained bounded prefix authenticates the selected shared hash state for Anchor's own
CacheAccount map, but closes no complete target query chain. Target placement, whole-table
pressure and the isolated cause of the full input delta remain unmeasured. Other affected
Address maps and input changes may contribute. The reserve combines population scale and
comparison density rather than claiming this particular conditional family is the sole cause.

A repeated key across transactions is charged again in B, although the union deduplicates it.
Repeated warm accesses within one Journal do not increase B; existing Account-journal opcode
work remains independent. Included REVERT/halt retains physical keys. A rejected mining
candidate contributes only to the attempted virtual population; it cannot inflate the committed
union through incidental producer-local DB caching. A fresh block accumulator resets both
populations. Disabled pricing does not collect or allocate these receipts or unions.

The source callback `complete_account_journal_population` now also retains a compact physical-key
receipt until both prefix merges complete. Missing/duplicate captures and checked arithmetic/domain
failures prevent publication of a trustworthy subtotal. `AccountCacheDiagnostics` is separate from
`AccountJournalDiagnostics`; known/full reports expose `account_cache_work_units` and
`account_cache_scaled`, and shadow reports expose `.account_cache()`. Completion revalidates its
sealed price and adds it exactly once; proposal-once contributes zero.

This remains an **observed-transaction cache proxy**. Honest stateless validation starts with a
fresh block State, but suspended preexecution system calls and direct DB consumers can load
unobserved keys, and filtered system results need not enumerate those keys. Arbitrary prefilled
host databases and custom inspectors deleting Journal keys are outside this population contract.
Capturing raw pre-system key membership before filtering is a follow-up if required; initial
population is not asserted to be exact or zero. Malformed-witness defense is a separate scope.

`account_cache_successor.py` deterministically generates the candidate from the immutable saved
`af30` parent and `successors/account-cache-v1.json`; its historical generator authenticates that successor
before generating the Rust table. Evidence retains input/native/quote/raw-actual identities.
Generation does not resample SP1. The generation-time candidate status remains in the immutable
evidence. The retained-input requote and50-proposal/9600-block price-sensitivity gates have since
completed as recorded below. That historical cohort has older416f/SDK6.1 input provenance and no
numeric actual PGU attached here; it cannot certify current-program PGU accuracy. A material change
to natural-workload conclusions requires reassessment rather than assuming this reserve is neutral.

Source regression covers progressive and repeated cross-transaction cohorts, rejected-candidate
isolation, warm within-transaction repeats, first4 byte order/dispersion/rounding, persistent
storage budget, missing/duplicate receipts, atomic overflow/domain failures, plain/inspected/Off,
STOP/REVERT/INVALID, known/full sealing and proposal-once zero. It does not establish a universal
workload bound. The historical-impact gate measures only the selected input cohort.

#### Completed finite validation of the supplement

Model `022232270da81c96dd9d7a12899d52ac011f46144d7f90d778a585aaeca5b7bd`
has now produced78 real local quotations of retained canonical inputs, without changing or
resampling their original SP1 observations. All inherited pricing components, selectors and
public outputs are unchanged from the actualaf30 baselines. Every total change is exactly the
separate128PGU/unit cache contribution; both ceiling paths and proposal-once zero are checked.

| Warm-first input | Original actual PGU | af30 literal node PGU | New literal node PGU | New full proposal PGU |
| --- | ---: | ---: | ---: | ---: |
| Fixed-G0 reference |338,631,603|1,354,594,058|17,441,782,794|17,606,176,969|
| Sole reference-G successor |5,027,210,167|1,354,594,058|17,441,782,794|17,606,189,513|

The demonstrated high-priority deficit is covered by the literal node price, about3.469x actual.
The artificial reference overquote, about51.507x, remains explicit rather than being discarded.
These opened cases are calibration/stress evidence, not normal historical admission samples.
Across25 measured controlled inputs, all full quotes cover actual. The same four positive
literal-node gaps remain, maximum153,352,535PGU, with no measured>2B residual. Three controlled
inputs stay explicitly unmeasured.

The separate50-proposal/9600-block historical sensitivity cohort changes as follows:

| Metric | af30 total PGU | New total PGU | Additional PGU | Relative change |
| --- | ---: | ---: | ---: | ---: |
| Literal sum of integer block-node prices |92,823,492,832|92,857,351,648|33,858,816|+0.03648%|
| Full proposal prices |159,967,003,341|160,000,862,669|33,859,328|+0.02117%|

All50 proposals/9600blocks change. The largest proposal-node uplift is0.07178%, and the largest
full-proposal uplift is0.02901%. The largest absolute block addition is48,128PGU; the largest
relative block uplift is0.11127%. Median andp95 block additions are both3,328PGU. The extra512PGU
in the full total belongs to attempted work; committed/attempted unions stay separate. Missing
historical actuals are not filled or treated as currentb76 precision measurements.

Validation comprises266 actual-source EVM tests,59 generator tests, independent source/lineage
review, seven independently authored behavior cases plus two focused source tests,35 trace and132
consumer CLI tests, five scoped host clippy targets, and independent78-row numeric/non-cache
verification. The strict EVM test lint still encounters one unchanged pre-existing style warning;
allowing only that lint passes the remaining checks. These cached-host checks do not claim a full
Cargo workspace/all-feature run, guest build or production SDK parity. Requoting replays GuestInput
natively; it performs no new canonical construction or SP1/SDK/guest/observer build or sample.

The original af30 inputs/reports remain in the separate
[data-only warm-first fixture](https://github.com/taikoxyz/raiko2-atlas/releases/tag/fixture-unzen-cache-state-warm-accounts-v1).
The proxy's pre-system/direct-DB population limits remain open, together with other block-long
containers and broader compositions. No mainnet admission rule is activated by this validation.


### Candidate: progressive storage-transition work

This section records the original3e335 checkpoint, whose2X policy and numerical results remain
raw historical evidence. The current J+2X policy is described in the following section.

The closed two-block prefix panel has an Anchor-only first block and 64 successful storage-update
transactions in the second. The immutable `0222` estimate is 998,599,458 literal node PGU for both
inputs. Its reference actual is 363,477,553 PGU; the sole successor actual is 3,550,141,992 PGU at
23,541,753 total charged gas. The successor's full estimate is 1,301,217,353 PGU. The positive gaps
are therefore 2,551,542,534 node and 2,248,924,639 full PGU. These original numbers remain `0222`
results. The two opened actuals become calibration/stress evidence once used for this candidate;
they are not held-out accuracy certification.

`CacheAccount.storage` and `TransitionAccount.storage` persist across transactions within a
block. Transaction-local Journal occupancy alone does not represent this population. In the
locked database source, changed entries flow through CacheAccount's changed-slot extension and
TransitionAccount's forward update. This independent term reserves those two forward passes;
it does not redefine the existing ordinary/transient journal term or alter SSTORE's opcode price.

For each finalized transaction t and storage owner o, capture all physical final `Account.storage`
keys J(o,t), each with its transaction-relative `is_changed()` flag. Let X(o,t) count the changed
entries. Each committed and attempted block prefix independently keeps a full-key union K(o).
Insert the whole incoming owner batch before pricing, with raw numeric low32 density:

```text
K(o) <- K(o) union J(o,t)
N(o) = |K(o)|; M(o) = largest same-low32 group in K(o)
C(o) = ceil((N(o) + 7*M(o)) / 8)
B(o,t) = 2*X(o,t)
storage_transition_work_units = sum_t,sum_o(C(o) * B(o,t))
storage_transition_scaled = storage_transition_work_units * 384 * 10^18
```

The independent work domain is 10^12. The 384 PGU/unit rate is an empirical conservative candidate,
using the existing storage-comparison reference without repricing any inherited component or
applying another discount. Different owners never share a population or group. Scalar diagnostics
report the largest owner C; computing C from total N and maximum M would be incorrect. Readonly
and final-noop entries enlarge the union for later writes, but X=0 adds no forward work. Repeated
changed keys across transactions contribute to X again without duplicating population. A
cross-transaction restoration to block-original value remains changed relative to that transaction;
a same-transaction restoration has X=0. Included REVERT/halt retains physical read population,
while restored writes are not counted as final changed entries.

The existing common finalization walk captures this receipt on plain and inspected handler paths,
before propagating handler errors, without extra database reads. Disabled and suspended collection
create no receipt or union. A rejected candidate advances only the attempted virtual population;
it cannot contaminate the committed population through incidental host cache state. A fresh block
collector resets both. Empty finalized maps supply an explicit empty receipt. Missing, duplicate,
malformed, overflow or domain failures are sticky and cannot publish a valid partial quote.
`StorageTransitionDiagnostics` is separate from logical unique resources and transaction-local
journal diagnostics. Known/full reports expose `storage_transition_work_units` and
`storage_transition_scaled`; shadow reports expose `.storage_transition()`. Completion validates
the sealed price, includes it once, and proposal-once contributes zero.

For the target owner alone in the observed 64-by-64 update panel, the formula yields 17,313,920
units, adding 6,648,545,280 PGU. Fresh model `3e335de2` host quotes observe 17,313,944 units for
each two-block input: 16 in the first block and 17,313,928 in the second. The extra 24 units belong
to other owners. The exact new component is therefore 6,648,554,496 PGU, added to node and full
equally; proposal-once contributes zero.

| Genuine-prefix input | Original actual SP1 PGU | New literal node PGU | New full proposal PGU |
| --- | ---: | ---: | ---: |
| Reference |363,477,553|7,647,153,954|7,950,601,817|
| Sole reference-G successor |3,550,141,992|7,647,153,954|7,949,771,849|

The demonstrated successor deficit is covered: node/actual is 2.15404x and full/actual 2.23928x.
The artificial reference's 21.03886x node overquote remains explicit. The two earlier single-block
update controls are covered too, at 7,642,552,232 node PGU and about20.2x their actuals. These are
deliberately unusual raw-key controls, not evidence of ordinary-transaction price precision.

All82 retained inputs were requoted once, with no new canonical construction or SP1 run. Across
9,634 blocks, every inherited component, selector, public output and raw Unzen value is unchanged;
only the new raw/scaled component, its exact additive totals and explicit model/executable
identities differ. All29 numeric full estimates cover their original actuals; 25 literal node
estimates cover. Four lower-priority node deficits remain, maximum153,346,391 PGU, with no
measured residual >2B PGU. Three controlled inputs remain explicitly unmeasured.

The separate50-proposal/9600-block historical sensitivity cohort changes as follows:

| Metric |0222 total PGU |3e335de2 total PGU | Additional PGU | Relative change |
| --- | ---: | ---: | ---: | ---: |
| Literal sum of integer block-node prices |92,857,351,648|92,887,659,232|30,307,584|+0.032639%|
| Full proposal prices |160,000,862,669|160,031,170,253|30,307,584|+0.018942%|

All50 proposals/9600blocks have positive additions. Maximum proposal-node uplift is0.066003%.
The highest relative block uplift is0.091275%; the largest absolute block addition is33,792PGU.
These two block extrema occur in different blocks. Median andp95 block additions are3,072PGU.
These small price changes preserve the earlier quote scale for this cohort;
its older416f/SDK6.1 metadata and absent numeric actuals remain price-sensitivity evidence only.
They do not certify currentb76 accuracy, an admission limit or a newly measured roughly-2x ratio.

Validation includes280 actual-source EVM tests,62 Python/model tests, independent complete source
review and nine independently authored behavior tests,37 trace/140 default-CLI tests, a separately
pinned genuine192-block replay and five strict production host clippy targets. The192-block input
retains its original identity and inherited goldens; the new component is1,622units/622,848PGU.
All82 numerical compositions were independently recomputed. The known unchanged EVM test-style
warning remains separate from strict production lint. These are cached host checks, without a
guest/SDK build, fresh SP1 actual or current-production program parity claim. The original input/
actual evidence is retained in the
[immutable genuine-prefix fixture](https://github.com/taikoxyz/raiko2-atlas/releases/tag/fixture-unzen-prefixed-storage-transitions-v1).

This is not a universal hash-map or PGU bound. Final bundle/revert construction remains present;
clone alone is not evidence of rehashing. Read-only/backend work, preseeded or suspended-system
storage not represented by observed final Journal keys, unusual lifecycle paths, and other hash
families remain follow-ups. The measured margin may cover later construction in these two inputs,
but the candidate does not separately price or prove a bound for every such operation.
In particular, read-filled block-long PlainStorage entry work can grow while X stays small; it is
the next candidate at that checkpoint; later read-cache measurements below confirm a further deficit. Dispersed
low32 keys with conditional full-hash collisions also need a separate short-comparison check.

At3e335, `storage_transition_successor.py` authenticated immutable `successors/parent-02223227.json`
and `successors/storage-transition-v1.json`, adding this separate policy/domain/tariff. Their bytes
remain immutable; current generation instead inherits the3e335 snapshot as described below. `generate.py` checks
that transform before generating Rust constants. No SP1 resampling is performed by generation.


### Candidate: progressive storage read-through reserve

Two closed finite panels use an Anchor-only first block and successful phase-advancing body reads
in the second. Body slots start at1, are read/checked once and never written; only phase0 changes.
The89-user point was chosen before measurements solely by EVM gas packing:
`floor((30M-2*1M)/312223)=89`. Both real Anchor costs remain in actual gas/workload; no startup
or prefix cost is subtracted. The old3e335 quotes and original SP1b76/SDK6.3 actuals are:

| Input | Actual SP1 PGU |3e335 literal node PGU |3e335 full PGU | Actual charged gas |
| --- | ---: | ---: | ---: | ---: |
|64×128 reference |430,578,358|1,643,693,938|1,961,030,144|20,256,761|
|64×128 sole fresh-G successor |2,954,417,885|1,643,693,938|1,960,992,560|20,256,761|
|89×128 reference |534,307,327|2,390,442,635|2,769,944,473|28,062,336|
|89×128 sole fresh-G successor |5,280,821,062|2,390,442,635|2,770,850,065|28,062,336|

The89 successor has measured positive gaps2,890,378,427 node /2,509,970,997 full PGU, exceeding the
>2B-at<=30Mgas priority screen. Its independently reconstructed245retained body locals have the
actual target map's ownG equal to the selected fresh reference word and fullH=0. This is local
hash-chain evidence, not proof of all11,392 placements, whole population or isolated map PGU.
All four actuals become opened stress/calibration evidence when used for this fix, not holdouts.
No new input, actual or seed search is requested by this model generation.

The missing scope is read-through work in block-long `CacheAccount.account.storage`. A Journal
Vacant load calls ordinary State::storage, whose PlainStorage entry inserts a successful value,
including0. Journal Occupied reads do not query this cache again. StorageWarmed rollback retains
keys and only restores coldness; StorageChanged restores values. The existing finalization walk
already captures these physical readonly keys before propagating handler errors. CacheAccount's
later changed-only commit filtering does not erase entries already cached by reads.

Keep the existing owner-separated all-J union, after-whole-batch C and public field names:

```text
K(o) <- K(o) union J(o,t)
C(o) = ceil((|K(o)| + 7*largest_same_low32_group(K(o))) / 8)
B(o,t) = |J(o,t)| + 2*X(o,t)
storage_transition_work_units = sum_t,sum_o(C(o) * B(o,t))
storage_transition_scaled = storage_transition_work_units * 384 * 10^18
```

J is the actual finalized physical key count, not SLOAD opcode count or declared access-list keys.
The J term supplies one successful ordinary read-through allowance per key/TX; repeated keys
across TXs are charged again without duplicating population. Within-TX warm/cold rollback keeps
the key. X retains the two potential changed-slot forward passes; same-TX noops/REVERTs have X0
but still pay J work. Committed/attempted isolation, batch atomicity, sticky missing/duplicate/
overflow errors, Disabled/suspended zero collection, fresh block reset and proposal-once0 remain.
Diagnostic query_budget validates the additive identity `physical_slot_receipts + 2*changed_slots`;
nonempty readonly receipts now have positive work. The raw/scaled public API and1e12 work domain
are unchanged. This replaces the previous budget; it does not add a second copy of2X or discount.

For the89 target owner only, J129/X1 and C_t=1+128t give sumC512729. Work changes from1,025,458
units to67,167,499, an extra66,142,041units or25,398,543,744PGU. Before requoting, that target-only
arithmetic predicted node>=27,788,986,379/full>=28,169,393,809. Fresh quotes add another6,144PGU
for other owners, obtaining27,788,992,523 node and28,169,399,953 full for the89 successor.
These are the actual new host quote totals, distinct from the retained SP1 measurement.

| Read-cache input | Retained actual SP1 PGU | EEA literal node PGU | EEA full PGU |
| --- | ---: | ---: | ---: |
|64×128 reference |430,578,358|14,835,335,026|15,152,671,232|
|64×128 sole fresh-G successor |2,954,417,885|14,835,335,026|15,152,633,648|
|89×128 reference |534,307,327|27,788,992,523|28,168,494,361|
|89×128 sole fresh-G successor |5,280,821,062|27,788,992,523|28,169,399,953|

The89 successor is now5.26225x node/5.33428x full actual; the64 successor is5.02141x/5.12881x.
The artificial89 reference's52.00938x node overquote remains explicit. The earlier prefix-update
successor (actual3,550,141,992) now quotes10,971,432,738 node/11,274,050,633 full,3.09042x/3.17566x.
These synthetic concentration families deliberately trade precision for a conservative price;
they do not imply an ordinary-mainnet relation changed from2x to5x. A comparable ordinary-workload
accuracy ratio was not measured by the history sensitivity cohort.

All86 retained inputs were quoted once in37groups, with zero native/SP1 runs, new inputs or
same-input retries. Across9,642blocks, all inherited components, selectors, public outputs,
proposal-once values and raw Unzen scores remain exact; only storage-transition units/contribution,
their additive totals and explicit model/executable identities change. Independent raw-number
recomputation covers every row/block. All33 numeric full estimates cover their original actuals;
29 literal-node estimates cover. The four node deficits remain below priority, maximum153,343,319PGU;
three controls remain explicitly unmeasured. Original program/SDK strata are retained rather than
presenting mixed-program observations as current-program accuracy certification. This finite
sample establishes no universal workload bound.

| Historical50-proposal sensitivity metric |3e335 total PGU | EEA total PGU | Additional PGU | Relative change |
| --- | ---: | ---: | ---: | ---: |
| Literal sum of integer block-node prices |92,887,659,232|92,925,858,400|38,199,168|+0.041124%|
| Full proposal prices |160,031,170,253|160,069,369,421|38,199,168|+0.023870%|

All50 historical proposals/9600blocks have positive additions. Maximum proposal-node uplift is
0.082066%; the maximum relative block uplift is0.102877% and maximum absolute block addition
36,096PGU (different block extrema). Median/p95 block additions are3,840PGU. The small score changes
do not materially change the earlier price scale for this cohort, but there are **no numeric
actuals** in these50 proposals:416f/SDK6.1 metadata remains price sensitivity only, never an
underpricing count or currentb76 precision certification. Material future ordinary-history impact
still requires discussion before endorsing a model change.

Host verification includes282 author EVM tests,62 Python/model tests, independent full source/model
review and12 independent behavior tests, then37trace/140default-CLI tests and five strict production
host clippy targets. The pinned genuine192-block replay changes its component from1,622 to3,699units,
an exact797,568PGU addition; inherited goldens remain intact. The fresh quote executable
`401a7e3d…` was built from Raiko200ccc8e-dirty/Reth5d6e281-dirty source, without relabeling old native
or actual artifacts as new builds. These are cached host gates, without a guest/SDK/ELF rebuild,
new SP1 measurement or production-program parity claim.

`storage_transition_successor.py` authenticates the inherited checkpoint from immutable `successors/parent-3e335de2.json`
and `successors/storage-read-cache-v1.json`; every inherited numeric pricing object, proposal-once
object, ownership list and arithmetic domain must remain exact. The declared384*1e12 domain was
already present in3e335 and is not added twice. Only work policy/provenance/model identity changes.
The evidence records `fixture-unzen-prefixed-storage-read-cache-v1` with an explicit
unpublished-at-model-generation state and null released tag; those generation-time fields stay
immutable, as do its generation-time repricing/validation flags; later host results are recorded
here. The four original inputs and their3e335 quotes/raw actuals were subsequently published
in the [immutable read-cache fixture](https://github.com/taikoxyz/raiko2-atlas/releases/tag/fixture-unzen-prefixed-storage-read-cache-v1)
at commit`2f92878202a9b80058c0d0fc60ec26c15d188081`. Independent genuine remote download/readback
verified the tag,9,672,438-byte asset digest, all inner identities and committed integrity commands;
the prior latest release stayed unchanged. Publication and later requotes do not rewrite model
identity or original samples. The original prefix-update fixture/evidence is not overwritten.

This remains an empirical comparison reserve, not a universal hash-probe, equality or PGU bound.
A backend error before Journal insertion is not a successful represented query; valid canonical
witnesses must resolve missing slots honestly. Custom preseeded caches/bundles, unobserved system
storage paths, alternate lifecycles, resize/clone/backend/bundle residuals remain qualified TODOs.
Symbolic long-equality zero-factor layouts and honest absent-zero body reads are separate future
boundaries; they are not newly measured here and do not alter either closed panel. In particular,
current shorter-comparison totals do not justify fitting a lower per-unit price before those
boundaries are tested. No claim of current production guest parity follows from host-source checks.
