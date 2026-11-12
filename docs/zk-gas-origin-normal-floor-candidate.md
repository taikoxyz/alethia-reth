# Offline ORIGIN normal-floor candidate

This is a development candidate, not the accepted runtime model. The accepted model remains
`80160649d7d29a85879cd9524b35c271a4b8638f64de94eef91d016e56121cf8`.
Only `opcode:0x32.accepted_absolute_model.coefficient` changes: the existing fixed model stores
PGU multiplied by `10^18`, and its normal coefficient changes from 5,000,000 to 2,176 PGU.
No schema extension is needed. Its original authority reference remains inherited; the successor
provenance identifies the new empirical coefficient and its derivation.

The feature-error coefficient and 5,000,000-PGU error minimum remain unchanged. Incremental,
structural, dispatch, memory, proposal completion, every other opcode/precompile, and Unzen prices
also remain unchanged. The 2,176-PGU value is this one normal floor, not the total transaction or
proposal quote. No runtime selector, new admission rule, guest change, or SDK upgrade is introduced.

## Retained development measurements

The three target inputs use a single successful top-level user call, empty calldata, a fixed
1,024-byte runtime and one block. The loop compares ORIGIN to CALLER and checks its completion count.
Native replay observes exactly 0, 1 or 8 ORIGIN instructions, no exceptional instruction, and one STOP.
Their user charged gas is `21,049 + 63*N`. Raw reports, requests, canonical inputs and public outputs
are independently hash-bound in `pricing/successors/origin-normal-floor-v1.json`.

| Target ORIGIN count | Raw execution-report gas | Actual PGU |
| --- | ---: | ---: |
| 0 | 3,075,728,992 | 161,032,931 |
| 1 | 3,075,761,225 | 161,034,619 |
| 8 | 3,075,991,355 | 161,046,667 |

All three actuals use research ELF
`b76baf2e0f3fa3fe6f8a20fe53dbf82f58601ed14b370dfc0f8d31db2069e034`,
SP1 SDK/executor/runner 6.3.0 and the same gas-estimator executable. Actual PGU is
`floor(raw_execution_report_gas * 10 / 191)`. These are model-development observations, not held-out
accuracy evidence or GPU proof-time measurements. Production-SDK equivalence has not been established.

Use all three same-target count pairs, with exact rational arithmetic:

| Pair | Additional PGU / additional ORIGIN |
| --- | ---: |
| 0 → 1 | 1,688 |
| 0 → 8 | 1,717 |
| 1 → 8 | 12,048 / 7 |

The policy is `ceil_to_64(5/4 * max(0, all_pair_slopes))`.
The maximum slope is `12,048/7`; the unrounded requirement is `15,060/7`; the smallest multiple
of 64 satisfying it is **2,176**. This includes loop/guard work in the marginal observation.
There is no subtraction of scaffold/CALLER, startup, dispatch, memory or any existing quote item.
The empirical 25% margin is a policy choice, not a mathematical bound for arbitrary programs.

## Reproduce and audit

Run from the repository root with an existing configured Python environment. Choose a fresh output
directory; the tool refuses to overwrite a nonempty directory or the accepted pricing directory.

```sh
PRICING=crates/evm/src/zk_gas/pricing
"$PYTHON_BIN" "$PRICING/origin_floor_successor.py" --output-dir "$CANDIDATE_DIR"
"$PYTHON_BIN" "$PRICING/origin_floor_successor.py" --check --output-dir "$CANDIDATE_DIR"
"$PYTHON_BIN" -m unittest discover -s "$PRICING" -p '*_test.py'
"$PYTHON_BIN" "$PRICING/generate.py" --check
```

The output contains `runtime-spec.json`, `candidate-runtime-spec.sha256` and `generated.rs`.
It does not write the accepted runtime JSON, acceptance record or compiled Rust table. The immutable
parent snapshot is byte-identical to accepted801. The candidate carries hashes of its replay script,
unchanged table generator, parent and evidence. Changing the algorithm requires a new candidate hash.
Readdressing a different price, feature-error rule, inherited route or provenance fails validation.

When the original measurement artifacts are available, verify their exact bytes and normative facts:

```sh
"$PYTHON_BIN" "$PRICING/origin_floor_successor.py" --check \
  --output-dir "$CANDIDATE_DIR" \
  --actual-root "$DEVELOPMENT_ACTUALS_DIR" --prepared-root "$PREPARED_CASES_DIR"
```

The roots are caller-supplied artifact locations. The retained evidence supplies relative filenames
and exact raw-report, request, native, input, runtime and program-manifest hashes. Source verification
checks the real receipt, completed/attempted instruction counts, actual PGU normalization, public
output and program identity. File and decompression reads are bounded; three samples are processed
one at a time. Replaying the retained facts alone does not reproduce a new SP1 execution.

The authoritative `generate.py` renderer is reused without editing its source. Accepted801's
provenance binds that source hash, so even adding a flag to it would invalidate its deterministic
replay. The candidate tool loads a private module instance and replaces only its validation callback
with the strict, fixed ORIGIN-successor validator; no arbitrary spec or skip-validation option is
provided. Normal `generate.py --check` continues to require accepted801.

## Remaining gates

The two synthetic n720 inputs were reserved and their actuals were not used in this candidate.
Seal the candidate hash before opening them. Then evaluate the candidate against those observations,
the fixed801 baseline comparison and relevant historical/normal-traffic requotes before deciding
whether to accept any new price. A failed reserved point must be reported as such rather than silently
retuning this candidate. CALL, caught normal REVERT and ECRECOVER are separate investigations.
