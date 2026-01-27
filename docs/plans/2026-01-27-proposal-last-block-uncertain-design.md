# Proposal Last Block Uncertain Design

## Goal
Mirror taiko-geth commit b6924ffa0 semantics in alethia-reth: when the batch-to-last-block DB mapping is missing and the backward scan finds the matching proposal only at the current head, the result is not deterministic and must return a specific RPC error instead of a block number.

## Context
The Taiko RPC methods `lastBlockIDByBatchID` and `lastL1OriginByBatchID` route through `resolve_last_block_number_by_batch_id`, which prefers the `BatchToLastBlock` DB mapping and falls back to a backward scan across recent Shasta blocks. Today the scan returns a block number even if the head block is the only match. Go now treats that case as uncertain, returning `ErrProposalLastBlockUncertain`.

## Design
1. Introduce a new Taiko RPC error variant (e.g. `TaikoApiError::ProposalLastBlockUncertain`) with message `proposal last block uncertain: BatchToLastBlockID missing and no newer proposal observed`. Map it to a new JSON-RPC error code `-32005` (sibling to existing `-32004 not found`).
2. Update the scan function to know the head block number. If the proposal matches at the head and there is no DB mapping, return a distinct "uncertain" outcome rather than a block number. If the proposal is found on any non-head block, return it as before. If no match, return not found.
3. Update `resolve_last_block_number_by_batch_id` to map the scan outcomes: return the new error for uncertain, preserve `GethNotFound` for no match, and return the block number when found.

## Error Handling
- DB mapping present: unchanged.
- Scan finds match at head: return `ProposalLastBlockUncertain` (JSON-RPC `-32005`).
- Scan finds match not at head: return block number.
- Scan finds no match: return `GethNotFound` (`-32004`).

## Testing
Add a unit test in `crates/rpc/src/eth/eth.rs` that constructs a minimal chain with a single head block containing an Anchor v4 transaction and extra data encoding the proposal id. Do not insert a `BatchToLastBlock` mapping. Assert that `resolve_last_block_number_by_batch_id` returns the new error (no block id). Use the reth test provider factory and `BlockchainProvider::with_latest` to set the head.
