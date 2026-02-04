# Shasta last-block scan optimization

Date: 2026-02-04

## Context
The Taiko auth RPC exposes `taiko_lastL1OriginByBatchID` and `taiko_lastBlockIdByBatchID`. When the batch-to-last-block mapping is missing, the RPC scans backwards from the head block to find the last block in a batch. The current scan in `alethia-reth` reads L1 origin data before decoding the Shasta proposal ID, which results in unnecessary database reads and longer scans when the target batch is not at the head.

The go-ethereum Taiko node (`taiko-geth`) introduced an optimization in commit `24e18fc77` to short-circuit scans when the proposal ID is already below the target and to avoid L1 origin reads when the proposal ID is above the target.

## Goals
- Align `alethia-reth` behavior with `taiko-geth` for batch scan logic.
- Reduce database reads during backward scans.
- Preserve existing error semantics (`NotFound`, `UncertainAtHead`, `LookbackExceeded`).

## Non-goals
- Changing the RPC surface or return types.
- Introducing new caching structures beyond the existing `BatchToLastBlock` table.
- Changing preconfirmation detection logic.

## Proposed change
In `find_last_block_number_by_batch_id` (crate `rpc`), reorder scan operations:

1. Keep anchor selector validation and lookback limits unchanged.
2. Decode `proposal_id` from the block extra data *before* reading L1 origin data.
3. Apply the following rules:
   - If `proposal_id < batch_id`, return `NotFound` immediately.
   - If `proposal_id > batch_id`, move to the previous block and continue scanning without any L1 origin lookup.
   - If `proposal_id == batch_id`, read the L1 origin and apply existing preconfirmation skipping logic. If the match is at the head without a mapping, return `UncertainAtHead`, otherwise return `Found`.

This matches the go implementation and relies on the same monotonicity assumption for proposal IDs with respect to block height.

## Error handling
- Failure to decode `proposal_id` retains the current behavior of stopping the scan and returning `NotFound`.
- Database read errors continue to be filtered via `is_missing_table_error` and otherwise return internal errors.

## Testing
Add a unit test that constructs a short chain where the head block has a `proposal_id` greater than the target `batch_id` and no `StoredL1Origin` entries. The expected outcome is `GethNotFound` without requiring any L1 origin data, proving the short-circuit path. Existing tests for `UncertainAtHead` and lookback limits remain unchanged.

## Rollout
This is a local RPC behavior alignment. No migrations are required. Expected impact is reduced DB reads and faster responses for non-head batch lookups.
