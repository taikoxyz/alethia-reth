# Witness Range Benchmark Notes

Date: `2026-04-17`

## Scope

This note captures the deployed-node benchmark results for `taiko_executionWitnessRange` after the lazy historical revert cache change. It records:

- what was benchmarked
- what correctness was actually validated
- serial performance numbers for `8`, `32`, and `192` block windows
- upgraded-node comparisons against the original `debug_executionWitness` baseline

The goal is to separate confirmed observations from assumptions.

## Environment

- Node endpoint under test: `35.224.7.209:8545`
- Chain state during the measurements:
  - synchronized (`eth_syncing = false`)
  - early serial benchmark runs were taken around `7233-7249`
  - later upgraded-node runs were taken around `7595-8381`
- Tested RPC methods:
  - `debug_executionWitness`
  - `taiko_executionWitnessRange`

## Correctness Validation

### What Was Validated

Correctness was validated on the deployed node for two full historical windows:

- `4200..4207`
- `6095..6286`

For each block in those windows:

- `debug_executionWitness(block)`
- the corresponding entry returned by `taiko_executionWitnessRange`

were normalized and compared.

Normalization used:

- `state`: sorted
- `codes`: sorted
- `keys`: sorted
- `headers`: compared in returned order

Validated results:

- `4200..4207`: `8/8` blocks matched after normalization
- `6095..6286`: `192/192` blocks matched after normalization

### Why Normalization Was Needed

The raw JSON payloads are not byte-for-byte stable because some arrays may be returned in a different order. Direct raw-response hashing is therefore not a reliable correctness test.

The normalized witness content matched exactly for all tested blocks in both validated windows.

### What Was Not Yet Validated

The following were **not** exhaustively validated:

- every block in the `32` block benchmark window
- split-range stitching semantics across multiple RPC calls

So the current correctness claim is:

> On the deployed node, `taiko_executionWitnessRange` produced the same normalized witness content as `debug_executionWitness` for every block in the tested windows `4200..4207` and `6095..6286`, including a full `192` block validation over `6095..6286`.

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

## Upgraded-Node Results

Later measurements were taken after the node configuration was upgraded substantially, especially on CPU.

These runs show two things clearly:

- hardware is a major factor for this workload
- on a stronger node, parallel `6x32` becomes practical and materially faster than both serial `6x32` and raw `debug_executionWitness`

### Old Historical Window `2249..2440`

This is the same deep historical window used in the earlier weak-node benchmark.

| Method | Total Time |
| --- | ---: |
| `debug_executionWitness` x `192` | `409207 ms` |
| serial `6 x 32` range | `109130 ms` |
| parallel `6 x 32` range | `28943 ms` |

Additional detail:

- first `debug_executionWitness(2249)` on the upgraded node: `2001 ms`
- all six parallel `32`-block range calls completed successfully

Relative speedup for this window on the upgraded node:

- serial `6x32` vs raw `debug`: about `3.75x`
- parallel `6x32` vs raw `debug`: about `14.1x`

### Newer Historical Window `6095..6286`

This `192`-block window was also measured on the upgraded node.

| Method | Total Time |
| --- | ---: |
| serial `6 x 32` range | `104945 ms` |
| parallel `6 x 32` range | `31623 ms` |

This matters because it shows that after the hardware upgrade, the older `2249..2440` window and the newer `6095..6286` window landed in a very similar performance range for `6x32`. That suggests the earlier "distance dominates everything" interpretation was overstated; once hardware is no longer severely constrained, the cost difference between those windows is much smaller.

### Fresh Parallel Run After Pod Restart

To check whether the parallel result only benefited from running the same window serially first, a fresh concurrent run was executed on an unseen window:

- window: `2000..2191`
- method: parallel `6 x 32`
- total time: `27003 ms`

After deleting and recreating the Kubernetes pod, the same fresh parallel run was repeated:

- window: `2000..2191`
- method: parallel `6 x 32`
- total time: `28477 ms`

Interpretation:

- the strong parallel result does **not** depend on running the same window serially first
- however, pod restart does not guarantee a true cold-cache experiment, because it resets process-local caches but typically does not clear host OS page cache

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
5. On the weak node, the practical limit was not witness correctness but node resource pressure:
   - read transaction lifetime
   - CPU contention
   - memory / page-cache pressure
6. On the upgraded node, the picture changes substantially:
   - serial `6x32` for deep historical `192`-block windows drops to about `1m45s`
   - parallel `6x32` drops further to about `29-32s`
   - raw `debug_executionWitness` on the same old window still takes about `6m49s`
7. The upgraded-node results suggest that hardware capacity was a major hidden variable in the earlier experiments. After the CPU upgrade, the difference between the old `2249..2440` window and the newer `6095..6286` window became much smaller.

## Current Safe Claim

The currently defensible claim is:

> `taiko_executionWitnessRange` is correct for the deployed-node validation windows `4200..4207` and `6095..6286`, including a full `192/192` block correctness check over `6095..6286`. It materially improves historical witness throughput, and on stronger hardware a `192`-block range split into `6x32` can be brought down to roughly `29-32s` with parallel requests on a single node.
