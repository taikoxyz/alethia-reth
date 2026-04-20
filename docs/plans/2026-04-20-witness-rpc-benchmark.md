# Witness RPC Benchmark Script

Date: `2026-04-20`

## Goal

Provide a small repo-local script that benchmarks raw `taiko_executionWitnessRange`
against raw `debug_executionWitness` on the same contiguous historical window and
checks whether both APIs return the same normalized witness content per block.

The script is intentionally direct:

- it talks to the RPC endpoint with raw JSON-RPC over `curl`
- it does not go through `raiko2` or any provider abstraction
- it therefore cannot silently fall back from range witness to debug witness

## Files

- Script: `scripts/bench_witness_rpc.sh`
- Output artifacts: temporary JSON files under `/tmp/bench_witness_rpc.*`

## Dependencies

The script expects these tools on the host:

- `bash`
- `curl`
- `jq`
- `sha256sum`
- `awk`
- `mktemp`

## Default Benchmark Shape

The default parameters match the recent manual benchmark runs:

- `offset = 10000`
- `window = 192`
- `chunk_size = 32`
- `parallel = 6`

That means:

1. read the current head with `eth_blockNumber`
2. resolve `start = head - 10000`
3. benchmark `taiko_executionWitnessRange` over `192` blocks, split into `6`
   concurrent `32`-block requests
4. benchmark `debug_executionWitness` serially over the same `192` blocks
5. normalize each witness and compare the block-by-block hashes

## Usage

Default rolling window:

```bash
scripts/bench_witness_rpc.sh \
  --rpc http://<rpc-host>:8545
```

Explicit historical start:

```bash
scripts/bench_witness_rpc.sh \
  --rpc http://<rpc-host>:8545 \
  --start 2000 \
  --window 192 \
  --chunk-size 32 \
  --parallel 6
```

Keep raw responses for inspection:

```bash
scripts/bench_witness_rpc.sh \
  --rpc http://<rpc-host>:8545 \
  --start 2000 \
  --window 192 \
  --chunk-size 32 \
  --parallel 6 \
  --keep-tmp
```

Replace `http://<rpc-host>:8545` with your own witness-enabled RPC endpoint.

## Output

The script prints:

- resolved head / start / end
- per-chunk `taiko_executionWitnessRange` timings
- total range timing
- per-block `debug_executionWitness` timings
- total debug timing
- mismatch count after normalization
- overall speedup as `debug_total / range_total`

Example shape:

```text
resolved window: head=7030867 start=7020867 end=7021058 window=192 chunk=32 parallel=6
range 7020867..7020898 56343ms
...
range total: 56447ms
debug 7020867 2457ms
...
debug total: 508967ms
correctness mismatches: 0
range total ms: 56447
debug total ms: 508967
speedup: 9.02x
```

## Correctness Rule

Raw witness JSON is not byte-stable enough for direct hashing because some arrays
may be returned in a different order. The script therefore normalizes each witness
before hashing:

- `state`: sorted
- `codes`: sorted
- `keys`: sorted
- `headers`: kept in returned order

Each normalized witness is serialized with `jq -cS` and hashed with
`sha256sum`. A run only counts as correct if all block hashes match.

## Notes

- This is an endpoint benchmark, not a node-internal microbenchmark.
- A connection reset or proxy drop will fail the script; use `--keep-tmp` if you
  want the partial artifacts.
- If `taiko_executionWitnessRange` is not deployed on the target endpoint, the
  script fails immediately on the raw RPC error instead of hiding it behind a
  fallback path.
