# Witness Range Benchmark Notes

Date: `2026-04-17`

## Scope

This note captures the deployed-node benchmark results for `taiko_executionWitnessRange` after the lazy historical revert cache change. It records:

- what was benchmarked
- what correctness was actually validated
- serial performance numbers for `8`, `32`, and `192` block windows

The goal is to separate confirmed observations from assumptions.

## Environment

- Node endpoint under test: `35.224.7.209:8545`
- Chain state during the measurements:
  - synchronized (`eth_syncing = false`)
  - head height was around `7233-7249` for the serial benchmark runs
- Tested RPC methods:
  - `debug_executionWitness`
  - `taiko_executionWitnessRange`

## Correctness Validation

### What Was Validated

Correctness was validated on the deployed node for the full `8` block historical window `4200..4207`.

For each block in that window:

- `debug_executionWitness(block)`
- `taiko_executionWitnessRange(4200, 4207)[block - 4200]`

were normalized and compared.

Normalization used:

- `state`: sorted
- `codes`: sorted
- `keys`: sorted
- `headers`: compared in returned order

All `8/8` blocks matched after normalization.

### Why Normalization Was Needed

The raw JSON payloads are not byte-for-byte stable because some arrays may be returned in a different order. Direct raw-response hashing is therefore not a reliable correctness test.

The normalized witness content matched exactly for all `8` tested blocks.

### What Was Not Yet Validated

The following were **not** exhaustively validated:

- every block in the `32` block benchmark window
- every block in the `192` block benchmark window
- split-range stitching semantics across multiple RPC calls

So the current correctness claim is:

> On the deployed node, `taiko_executionWitnessRange` produced the same normalized witness content as `debug_executionWitness` for every block in the tested `8` block historical window `4200..4207`.

## Serial Benchmark Results

All timings below are wall-clock measurements taken with serial requests.

### First-Block Comparison

| Window | Start Block | `debug_executionWitness(start)` | `taiko_executionWitnessRange(start, start)` | Delta |
| --- | ---: | ---: | ---: | ---: |
| `8` | `4200` | `3029 ms` | `3510 ms` | `+481 ms` |
| `32` | `4249` | `3069 ms` | `3597 ms` | `+528 ms` |
| `192` | `2249` | `3465 ms` | `3750 ms` | `+285 ms` |

Observation:

- the first block of a range stayed close to the upstream single-block path
- the lazy-cache rewrite removed the earlier regression where `range(start, start)` was dramatically slower than `debug_executionWitness(start)`

### Total Serial Time

| Window | Block Range | Serial Single-Block Total | Single Range Call | Split Range Result |
| --- | --- | ---: | ---: | --- |
| `8` | `4200..4207` | `24654 ms` | `15505 ms` | `4 + 4 = 17179 ms` |
| `32` | `4249..4280` | `94879 ms` | `56942 ms` | not benchmarked |
| `192` | `2249..2440` | `637886 ms` | failed after `300728 ms` | `3 x 64 = 294710 ms`; `6 x 32 = 295962 ms`; `2 x 96` did not complete reliably |

### `192` Block Split Details

#### `3 x 64`

| Range | Time |
| --- | ---: |
| `2249..2312` | `98067 ms` |
| `2313..2376` | `97275 ms` |
| `2377..2440` | `99368 ms` |
| Total | `294710 ms` |

#### `6 x 32`

| Range | Time |
| --- | ---: |
| `2249..2280` | `48301 ms` |
| `2281..2312` | `48598 ms` |
| `2313..2344` | `49155 ms` |
| `2345..2376` | `48635 ms` |
| `2377..2408` | `50417 ms` |
| `2409..2440` | `50059 ms` |
| Total | `295962 ms` |

#### `2 x 96`

| Range | Result |
| --- | --- |
| `2249..2344` | succeeded in `152290 ms` |
| `2345..2440` | connection reset after `185227 ms` |

## Failure Modes Observed

### Large Single Range

The single `192` block range did not return a witness result. It failed with:

```text
failed to read a value from a database table: read transaction has been timed out (-96000)
```

This is consistent with the current implementation holding one read transaction for the lifetime of a large range call.

### Weak-Node Memory Warning

The node also logged:

```text
WARN Attempt to calculate state root for an old block might result in OOM target=2313
```

This warning comes from the historical revert path inherited from upstream `reth`. It is a conservative warning, not proof of imminent OOM, but on a weak machine it matches the observed behavior: deep historical witness generation is expensive and large concurrent requests can destabilize the node.

## Takeaways

1. The lazy revert cache optimization is real for serial historical windows.
2. `8` and `32` block range calls clearly outperform serial single-block calls.
3. For the tested `192` block historical window, a single giant range is still too large for this node configuration.
4. Splitting the `192` block request into smaller serial chunks works well:
   - `3 x 64` and `6 x 32` both completed in about `4m 55s`
   - this is much better than the `10m 38s` serial single-block baseline
5. On this weak node, the practical limit is not witness correctness but node resource pressure:
   - read transaction lifetime
   - CPU contention
   - memory / page-cache pressure

## Current Safe Claim

The currently defensible claim is:

> `taiko_executionWitnessRange` is correct for the deployed-node `8` block validation window `4200..4207`, and it materially improves serial historical witness throughput on this node for moderate window sizes. For large windows like `192` blocks, request splitting is still required on weak hardware.
