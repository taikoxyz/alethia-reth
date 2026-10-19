# ZKGas Pricing Model

## Status and scope

This document describes the accepted SP1 ZKGas pricing model compiled into alethia-reth. The current
runtime artifact has model identity
`93d04eddda3779393807808859eb36561d35aad0cdd8a8a3960ec3e279a93046` and was introduced by
[alethia-reth PR #253](https://github.com/taikoxyz/alethia-reth/pull/253).

The model is implemented but is not an admission or consensus rule. `Disabled` remains the default
runtime mode. `Shadow` collects selectors and evaluates the model without rejecting transactions or
blocks and without replacing the existing Unzen meter, block limit, or header-difficulty
commitment.

This is the human-readable semantic contract. The following checked-in artifacts are authoritative
for details that must remain machine-exact:

- [`runtime-spec.json`](../crates/evm/src/zk_gas/pricing/runtime-spec.json) is the canonical
  coefficient, provenance, domain, arithmetic, and validation artifact.
- [`model.rs`](../crates/evm/src/zk_gas/pricing/model.rs) defines the checked executable semantics.
- [`types.rs`](../crates/evm/src/zk_gas/pricing/types.rs) defines the typed selector and result
  contracts.
- [`accumulator.rs`](../crates/evm/src/zk_gas/pricing/accumulator.rs) defines transactional selector
  collection and unique-resource accounting.
- [`runtime.rs`](../crates/evm/src/zk_gas/runtime.rs) extracts typed operation inputs from revm.
- [`generated.rs`](../crates/evm/src/zk_gas/pricing/generated.rs) is derived from the canonical JSON
  and must not be edited by hand.

If this document conflicts with the canonical JSON or checked evaluator, the artifact and evaluator
control the current implementation. Such a conflict is a documentation bug and must be fixed before
the model is promoted to enforcement.

## Objective and non-goals

ZKGas is a host-computable workload meter. It is intended to identify execution that is cheap under
Ethereum gas but expensive for the configured ZK backend, so a later admission policy can bound the
work sent to a prover without running the prover first.

The pricing model is designed around these properties:

- deterministic integer evaluation for identical committed execution;
- explicit, non-overlapping ownership of operation, state, trie, payload, block, and proposal work;
- monotone charging of cost-bearing selectors;
- explicit conservative pricing or a typed failure for inputs outside the accepted model;
- transactional collection, so rejected candidate transactions do not pollute an admitted prefix;
- content-addressed coefficients and execution identity; and
- conservative safety taking priority over mean prediction error.

The accepted profile is calibrated against normalized SP1 `ExecutionReport::gas()` (PGU) for one
identified production proposal ELF and VK. It does not claim to price RISC0 cycles, wall-clock proof
latency, or another SP1 guest. A different backend, guest, cost-bearing trace schema, or coefficient
set requires independent calibration and a new model identity.

The current implementation deliberately does **not**:

- replace or modify the Unzen ZKGas schedule;
- choose or enforce a new per-block limit;
- enforce a cumulative proposal budget;
- guarantee that an admitted proposal finishes within a wall-clock deadline;
- expose an `Admission` mode; or
- treat a Shadow subtotal as a complete block quote before exact resources and MPT selectors are
  supplied.

## Model identity and units

The canonical artifact distinguishes two version numbers:

- `execution_identity.trace_schema_version = 9` identifies the calibrated feature/trace schema;
- top-level `schema_version = 1` identifies the JSON runtime-export format.

The artifact binds the production proposal ELF and VK hashes, calibration source hashes, validation
authority hashes, route table, tariffs, arithmetic policy, and supported domains. The canonical JSON
is content-addressed as `RUNTIME_SPEC_SHA256`; reports carry that identity so a caller cannot
silently complete a report with another model.

All coefficients are fixed-point PGU values with scale `10^18`. Evaluation uses checked signed
128-bit arithmetic. Selectors must be nonnegative and no larger than the sealed bound of
`1,000,000,000`; each scope is limited to at most 1,024 terms. Invalid selectors, excessive term
counts, negative components, and overflow are typed errors. The evaluator rounds upward only at a
block or proposal-once boundary:

```text
integer_pgu = ceil(total_scaled_pgu / 10^18)
```

There is no runtime-fit step and no mutable runtime discount. Any calibration normalization,
discount, reserve, envelope, or tariff decision is already incorporated into the sealed
coefficients.

## Scope decomposition and ownership

The model splits work into disjoint block-local and proposal-once scopes:

```text
proposal_scaled = proposal_once_scaled + sum(block_scaled)
proposal_reference_pgu = ceil(proposal_scaled / 10^18)

block_admission_pgu[i] = ceil(block_scaled[i] / 10^18)
```

The complete proposal reference applies one ceiling after combining all disjoint scaled components.
The per-block ceiling is a separate value for block-local admission and compatibility analysis;
`sum(block_admission_pgu)` is not the proposal reference price. Proposal startup and parent-state
work are not spread across blocks, and block costs are not averaged across a proposal.

The complete block-local fixed-point price is:

```text
block_price =
    block_base
  + started_non_anchor_transaction_tariff * started_non_anchor_transactions
  + native_value_transfer_tariff * native_value_transfers
  + operation_price
  + exact_unique_resource_price
  + exact_block_mpt_price
```

The proposal-once price is:

```text
proposal_once_price =
    proposal_startup
  + canonical_transaction_byte_tariff * canonical_transaction_bytes
  + nonempty_payload_transaction_tariff * nonempty_payload_transactions
  + true_unattempted_transaction_tariff * true_unattempted_transactions
  + extra_blob_tariff * (total_blob_count - 1)
  + extra_source_tariff * (source_count - 1)
  + exact_parent_mpt_price
```

The exact coefficient values live only in `runtime-spec.json` and generated Rust. Keeping the
numbers in one canonical artifact prevents a prose copy from drifting away from the evaluator.

### Anchor ownership

The Anchor transaction is not a second operation-pricing domain. Its executed opcodes,
precompiles, memory growth, state access, and confirmed spawn wrappers use the same routes as any
other included transaction. The Anchor does not receive the
`started_non_anchor_transaction_tariff`; its envelope and protocol orchestration are covered by the
block fixed term. This prevents both omission of Anchor execution and double charging it through a
separate whole-Anchor estimate.

### Transactional ownership

Selectors are staged per transaction and become block-visible only when that transaction is
included. Resetting a rejected or otherwise discarded candidate removes its in-flight operation and
resource selectors while preserving the previously committed prefix.

An included transaction that halts or reverts still consumed execution work, so its executed
operations remain priced. State and resource selectors reflect the collector's committed execution
contract rather than being inferred from the transaction's final status alone.

## Operation routes and composition

There are 174 authenticated operation routes in three namespaces:

- `opcode:0xNN` for ordinary interpreter steps;
- `precompile:0xNN` (including P256 at `0x100`) for direct precompile bodies; and
- `opcode:0xNN:confirmed_spawn_wrapper` for confirmed CALL/CREATE-family child-frame dispatch.

Each observed event is evaluated into up to four candidates:

- `accepted_absolute`: the calibrated absolute model for the typed event;
- `incremental`: a controlled per-event or per-selector candidate retained by some routes;
- `structural`: an independent structural floor, principally used by precompiles; and
- `feature_error`: a conservative route used when execution occurred but the accepted typed input
  is unavailable for that outcome.

Candidates are accumulated by authenticated route across the committed block. The route then uses
one of two sealed composition rules:

```text
accepted route:
    route_normal = accepted_absolute

maximum route:
    route_normal = max(accepted_absolute, incremental, structural)

route_total = route_normal + feature_error
operation_price = sum(route_total for every committed route)
```

The maximum is applied after each candidate has been accumulated for the route, not independently
to each event. The accepted artifact contains 113 accepted-only routes and 61 maximum-composition
routes.

The four candidates are alternatives only where the composition rule says so. `feature_error` is
additive because it represents executed events that could not safely use the accepted route; it is
not allowed to disappear behind another candidate.

### Accepted model families

The evaluator supports these typed families:

| Family | Cost-bearing selectors and behavior |
| --- | --- |
| Fixed | One constant contribution per event. |
| Fixed with memory | A fixed body plus the shared memory model; some routes require an exact raw-gas envelope. |
| Linear/floored linear | A checked linear function of interpreter gas, native precompile gas, event count, or declared work units, with optional constant and minimum floors. |
| Static raw gas | Common dispatch plus a coefficient multiplied by the opcode's exact interpreter raw gas. |
| Raw-gas affine | Common dispatch plus a constant and raw-gas term. |
| EXP | A conservative small-exponent bucket, otherwise a function of exponent-byte length and its square. |
| KECCAK256 | Common/fixed work, zero-length behavior, Keccak permutation count, and shared memory growth. |
| Memory access | Fixed opcode work plus shared memory growth for `MLOAD`, `MSTORE`, and `MSTORE8`. |
| Memory copy | Fixed work, copied words, and shared memory growth for `MCOPY`. |
| Copy | Validated byte/word relationship plus copied words and shared memory growth for calldata/returndata copying. |
| LOG | Topic count, zero/nonzero data, additional data words, and shared memory growth. |
| RETURN | Returned bytes/words and shared memory growth, with a conservative repeated-boundary extension outside the measured byte envelope. |
| Storage | Warm/cold `SLOAD` and warm/cold plus semantic-branch `SSTORE` pricing. |
| Declared context | Typed value/access classes for context operations whose cost shape is not represented by raw EVM gas alone. |

The exact family assigned to every route and all parameters are in `block_local.operation_routes` of
the canonical artifact.

### Shared memory model

Memory-bearing operation models use one common selector decomposition:

```text
memory_price =
    growth_event_tariff * growth_events
  + evm_gas_delta_tariff * memory_evm_gas_delta
  + boundary_4k_tariff * boundary_4k_events
```

Typed extractors also validate relationships such as `words = ceil(bytes / 32)` and the raw EVM gas
implied by LOG or copy inputs. A contradiction is an invalid model input, not a zero-cost event.

`RETURN` has a measured envelope through 65,536 returned bytes. Above that boundary, the evaluator
uses the maximum of a raw-gas-linear extension and repeated boundary-sized chunks. This makes the
extension monotone without claiming that an unmeasured huge return follows the in-envelope fit.

### Storage and context models

`SLOAD` is classified by warm/cold access. `SSTORE` is classified by warm/cold access and one of
`Clear`, `DirtyRewrite`, `Noop`, `Reset`, `RestoreOriginal`, or `Set`. Invalid combinations, such as
a cold dirty rewrite, are rejected rather than mapped to another branch.

The declared context model currently distinguishes fixed ADDRESS/CALLER work, zero/nonzero value
classes, zero/partial/full `CALLDATALOAD`, and `CALLDATASIZE`. These are explicit typed
approximations in the accepted artifact, not a generic inheritance from Ethereum gas.

### Precompiles and spawn wrappers

Direct precompiles are separate authenticated routes. Most use native precompile gas for their
accepted, incremental, or structural candidates. Routes may combine a constant and native-gas
linear term. The `0x07` route additionally uses declared semantic work units for its structural
floor because native gas alone did not provide the intended conservative boundary.

CALL/CREATE-family opcodes stage a possible spawn. The confirmed wrapper route is charged only when
frame dispatch confirms it. Child execution itself is priced by the ordinary operations executed in
the child and is not multiplied by forwarded gas.

## Block fixed, resource, and MPT terms

### Fixed block terms

The sealed block fixed vector contains:

- `block_base` for fixed per-block work, including block/Anchor orchestration not owned elsewhere;
- one tariff for each started non-Anchor transaction; and
- one tariff for each eligible native value transfer that did not enter executable code or a
  precompile route.

The block base is not an opcode estimate for the Anchor. Anchor operations remain operation-owned.

### Unique execution resources

The block prices five exact unique selectors:

- accounts accessed;
- storage slots accessed;
- storage slots written;
- nonempty code hashes materialized; and
- total bytes across those unique code hashes.

These are set cardinalities or unique byte totals across the committed block prefix, not sums of
per-transaction counts. Repeated access to the same resource does not recreate one-time
materialization work. The canonical artifact also records dependency ownership, such as the code
hash dimension's dependency on account observation.

The EVM-local collector may observe only a subset of the resources loaded elsewhere in block
processing. Completion therefore validates that each exact resource dimension is at least the
EVM-local observation and **replaces** the provisional EVM-local resource contribution with the
exact contribution. It does not add both.

### Sparse-trie work

Block and proposal-parent MPT work use seven exact selectors:

- path-node visits;
- materialization work units;
- hydration hash blocks;
- dirty encoded nodes;
- dirty hash blocks;
- structural branch/extension churn nodes; and
- dropped trie nodes.

Each scope is a dot product of these selectors and its sealed tariff vector. The block selector is
the exact absolute snapshot for the canonically aggregated block state. It is not an additive sum of
per-transaction path-depth guesses. Proposal-parent MPT work is proposal-once and must not be copied
into every block.

## Proposal payload and blob-source terms

Proposal-once payload pricing observes canonical signed EIP-2718 bytes, the number of transactions
with nonempty payloads, and transactions that were truly never attempted by proposal execution.
These terms cover proposal input work that is not owned by executed operation routes.

The current blob/source model accepts only the sealed BPO2 topology:

- between 1 and 11 sources;
- zero to ten forced-prefix sources, each with exactly one blob;
- one final normal source with between one and six blobs;
- between 1 and 16 blobs in total; and
- agreement between proposal blob hashes and provider blob, commitment, and proof counts.

Inside that domain, the base one-blob/one-source topology incurs no separate blob/source increment;
the model charges extra blobs and extra sources. Inputs outside the domain are rejected; the
evaluator does not extrapolate the accepted topology silently.

## Conservative and failure behavior

Conservative pricing is part of the model, not an error metric to optimize away. The calibration
uses three broad strategies:

1. controlled measured models for frequent, repeatable, parameter-dependent, or high-damage work;
2. conservative envelopes when the relevant dimensions are understood but a finer decomposition
   does not improve the safety boundary; and
3. explicit aggressive tariffs or failure for unresolved work whose reachable domain cannot safely
   use the measured model.

At runtime:

- an authenticated `feature_error` model prices executed outcomes whose exact accepted-model inputs
  are unavailable;
- an unknown route follows `unknown_route_policy = reject`;
- missing, contradictory, or out-of-range selectors are typed errors;
- arithmetic overflow and negative components are errors; and
- a diagnostic prevents a Shadow subtotal from being presented as a trustworthy complete quote.

Because the only active modes are `Disabled` and non-enforcing `Shadow`, these pricing failures do
not currently reject EVM execution. They become bounded-cardinality Shadow diagnostics. Any future
Admission mode must define how these typed failures fail closed before it can enforce a limit.

## Shadow runtime contract

Runtime mode is selected with either:

```text
--zkgas-mode disabled|shadow
ALETHIA_RETH_ZKGAS_MODE=disabled|shadow
```

`Disabled` is the default. It installs no new selector collector and preserves the existing Unzen
execution, admission, and difficulty paths.

`Shadow` installs a node-wide observer on every executor, including sync and RPC simulation paths.
It never rejects a transaction or block and never changes receipts, gas, state, or assembled
headers. It can add substantial CPU overhead on opcode-heavy execution and is intended for
diagnostics and reference comparison rather than default operation. Shadow collection is not gated
on Unzen activation; when selected, it also observes pre-Unzen execution without changing its
canonical outcome.

An alethia-reth block executor can publish only:

- an `Incomplete` report containing block fixed, operation, and EVM-local resource components; or
- a typed `Diagnostic` explaining why even that subtotal is not trustworthy.

The report remains incomplete because the EVM runtime does not own exact block-end resources or the
absolute MPT snapshot. An external caller that owns those values may call
`complete_block_shadow_report`. Completion authenticates the model identity and known subtotal,
checks that exact resources dominate the EVM-local subset, prices exact resources and MPT, and
returns the complete block breakdown. It cannot convert a diagnostic from another model into a
valid quote.

`price_proposal_once` separately evaluates proposal payload, blob/source, startup, and parent-MPT
inputs. The caller is responsible for composing its scaled value exactly once with every complete
block's scaled value, then applying one proposal-level ceiling. Per-block integer values remain the
separate block-admission view.

## Calibration and evidence

The artifact embeds hashes and paths for six calibration/evaluation sources, including the
estimator, runtime candidate, payload binding, natural holdout, final schema-9 holdout, and opened
regression result. These source artifacts live in the raiko2 opcode-gas experiment lineage; their
content hashes, rather than mutable paths, bind the runtime export.

Alethia-reth vendors the accepted runtime artifact, generator, and executable evaluator, not the
full calibration corpus or the 56 MB estimator source. This repository can reproduce and verify the
integer export from `runtime-spec.json`; recomputing the coefficients from raw measurements requires
the matching raiko2 calibration checkout identified by the embedded source paths and hashes.

The accepted runtime artifact records:

- 461 controlled fit rows replayed;
- 1,152 historical blocks replayed;
- six historical proposals replayed;
- zero underquoted controlled rows in the declared matched domain;
- zero underquoted historical blocks in that replay; and
- no integer-export rounding uplift in the recorded block or proposal replay.

These results validate the declared artifact and frozen evidence only. They are not a mathematical
claim about every future block, a proof-latency benchmark, or a capacity guarantee. Shadow
comparison against current SP1 workloads remains necessary before selecting an admission limit.

## Capacity and future admission

Model selection and limit selection are separate decisions. This model produces PGU estimates; it
does not encode the hardware throughput, proposal interval, utilization reserve, queueing policy, or
acceptable false-reject rate needed to choose a limit.

The current implementation is block-local. It can bound a single transaction or block only after
the exact candidate-prefix completion inputs are available. It does not constrain the sum of every
block in a proposal. A future cumulative proposal budget may compose the same calibrated terms, but
that is a separate policy and runtime change.

Before adding Admission, a reviewed change must at least provide:

- exact candidate-prefix resource and MPT selectors on the production construction path;
- explicit limit and capacity assumptions;
- fail-closed handling for every typed diagnostic;
- candidate rollback/replacement semantics;
- historical compatibility and adversarial saturation results; and
- a versioned activation and rollback plan that does not reinterpret existing Unzen difficulty.

## Reproduction and change policy

Check the generator and canonical artifact with:

```bash
python3 -m unittest crates/evm/src/zk_gas/pricing/generate_test.py
python3 crates/evm/src/zk_gas/pricing/generate.py --check
```

Regenerate `generated.rs` only through `generate.py`. A change to a coefficient, domain, route,
ownership rule, arithmetic policy, source authority, production guest identity, or cost-bearing
selector requires a regenerated artifact with a new content address and fresh compatibility
evidence. Do not add a compatibility alias that lets reports from two model identities be completed
as though they were the same model.

Documentation-only clarification may retain the model identity only when it does not change the
canonical artifact or executable semantics.
