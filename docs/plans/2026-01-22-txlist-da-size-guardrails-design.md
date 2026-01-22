# Txlist DA Size Guardrails (RLP + Zlib)

## Context
The current tx selection uses `tx_estimated_size_fjord_bytes` to gate DA size. That estimate is based on FastLZ and is applied per-transaction. The actual txlist bytes for Taiko are produced by RLP-encoding the list of raw tx bytes and then zlib-compressing the entire list. Because the estimator and codec are different and list-level compression is not additive, the current logic cannot guarantee that a produced txlist is under the DA size limit.

## Goals
- Guarantee that each built txlist does not exceed the configured DA size limit.
- Keep selection performance fast for the common case.
- Preserve nonce continuity when building multiple txlists by re-trying a tx at the start of the next list if it did not fit at the end of the previous list.
- Match the taiko-client-rs codec semantics exactly (RLP list + zlib with default compression).

## Non-goals
- Changing tx ordering or selection policy beyond size gating.
- Replacing the existing FastLZ estimator.
- Reworking the transaction pool iterator behavior.

## Recommended Approach
Use a two-phase gate:
1) **Fast path**: use the existing estimate to filter clearly-too-large cases and to admit txs when far from the DA limit.
2) **Near-boundary check**: when the estimated total is within a configurable headroom of the limit, compute the real compressed size via RLP list + zlib. Admit the tx only if the real compressed size is within the limit.

This preserves performance (real compression is rare) while providing a hard guarantee from the real-size check.

### Headroom
Headroom is only a trigger for the real-size check. It is not relied on for correctness. A default of `max(8 KiB, limit * 2%)` is recommended; it can be tuned later based on production data.

## Data Model
Maintain per-list state:
- `raw_txs: Vec<Vec<u8>>` containing `encoded_2718` bytes for each included tx.
- `est_total: u64` sum of `tx_estimated_size_fjord_bytes`.
- `gas_total: u64` (existing).
- `cached_real_size: Option<usize>` and `cached_count: usize` to avoid recompressing when the list has not changed since the last check.

## Algorithm (High Level)
- Iterate over best pool transactions.
- For each candidate tx:
  - Apply existing filters (locals, min_tip, gas limit, single-tx DA estimate).
  - Compute `est = tx_estimated_size_fjord_bytes(encoded)`.
  - If `est_total + est <= limit - headroom`, proceed with the existing execute-and-append flow.
  - If `est_total + est > limit - headroom`, run a real-size check on `raw_txs + encoded`.
    - If real size <= limit: execute and append.
    - If real size > limit: **start a new list and retry the same tx** (do not mark invalid and do not advance the iterator).
  - If a tx still exceeds the limit when it is the only tx in a list, mark it invalid and skip.

This guarantees that when a tx fails to fit at the end of list N, it is immediately attempted at the start of list N+1, preventing nonce gaps.

## Real-size Check
Use the same codec as taiko-client-rs:
- RLP list encoding via `alloy_rlp::encode_list::<_, Vec<u8>>`.
- zlib compression via `flate2::write::ZlibEncoder` with `Compression::default()`.
- The size check uses the length of the compressed bytes.

## Error Handling
- RLP or zlib errors should surface as fatal selection errors (propagate up), since they indicate a codec or memory issue rather than a valid tx failure.
- A tx that cannot fit even as a single-item list should be marked invalid and skipped.

## Testing
- Unit test for real-size check matching taiko-client-rs codec for known inputs.
- Selection flow test that forces a boundary case where the last tx would overflow, and verify it becomes the first tx of the next list (nonce continuity preserved).
- Test that a single oversized tx is rejected.
- Perf sanity test: ensure real compression is not invoked when far from the limit.

## Rollout and Tuning
- Start with the default headroom and record the rate of real-size checks and rejected txs.
- Adjust headroom if compression checks are too frequent or too rare.

