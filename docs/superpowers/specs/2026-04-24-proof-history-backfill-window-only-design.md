# Proof History Backfill Window Only Design

## Goal

Add a `--proofs-history.backfill-window-only` mode that avoids proof-history writes from genesis during a fresh sync. The mode derives the desired proof-history start block from the node's finalized head and the configured proof-history retention window.

## CLI

Add a boolean flag:

```text
--proofs-history.backfill-window-only
```

The flag is only meaningful when `--proofs-history` is enabled. It uses the existing `--proofs-history.window` value.

## Start Block Selection

When proof-history storage is empty and `backfill-window-only` is enabled, the ExEx delays proof-history initialization until it can derive:

```text
start_block = finalized_block_number.saturating_sub(proofs_history.window)
```

The finalized block comes from the local provider's chain-state finalized block number, read through the database provider's `ChainStateBlockReader::last_finalized_block_number()`. If that value is `None`, the ExEx stays idle and acknowledges notifications so normal node sync is not blocked.

If `finalized_block_number <= proofs_history.window`, the derived start block is `0`, so the behavior is equivalent to the current genesis-start behavior.

## Runtime Flow

For empty proof-history storage:

1. If `backfill-window-only` is disabled, initialize immediately from the current canonical state, preserving current behavior.
2. If enabled and no finalized head is known, skip proof-history work and report the committed notification height as finished.
3. If enabled and local executed head is below `start_block`, skip proof-history work and report the committed notification height as finished.
4. Once local executed head reaches `start_block`, initialize proof-history storage from the current canonical state and start normal proof-history indexing from that block forward.

For non-empty proof-history storage, skip delayed initialization and continue from the stored proof-history range. This avoids changing restart behavior for nodes that already have a sidecar DB.

## ExEx Acknowledgement

Skipped blocks before initialization must still send `ExExEvent::FinishedHeight` for committed notifications. The ExEx must not hold back the node pipeline while waiting for the finalized-derived start block.

## Pruning

Pruning behavior remains unchanged. The existing `--proofs-history.window` value is still the retention window once proof-history data exists.

## RPC Behavior

Before proof-history storage is initialized, proof-history-backed `debug_executionWitness` and `eth_getProof` overrides return the same error used for an empty or missing proof-history range: `ProviderError::StateForNumberNotFound` mapped through the existing RPC error path. After initialization, only blocks inside the stored proof-history range are available.

## Logging

Log once when entering delayed initialization mode, including `window`, `executed_head`, and `finalized_block_number` when known. Log when the start block is derived and when storage initialization starts and completes.

## Tests

Add unit tests for:

- CLI parsing of `--proofs-history.backfill-window-only`.
- Start block derivation from finalized head and window.
- Empty storage with no finalized head remains uninitialized.
- Empty storage below derived start block acknowledges notifications without storing proof-history data.
- Empty storage at or above derived start block initializes and then processes forward.
- Non-empty storage ignores delayed initialization and resumes normally.
