# Uzen zk-gas tx-selection skip fix

**Date:** 2026-04-13
**Status:** Approved design
**Scope:** `crates/block/src/tx_selection/mod.rs` + tests

## Problem

In Uzen blocks, the `txPoolContent` / `txPoolContentWithMinTip` RPC returns no transactions when a pending transaction with excessive zk gas appears early in the pool iteration order.

The root cause is in `select_and_execute_pool_transactions` at `crates/block/src/tx_selection/mod.rs:236-238`:

```rust
Err(err) if is_zk_gas_limit_exceeded(&err) => {
    trace!(target: "tx_selection", ?tx, "stopping selection after zk gas exhaustion");
    break;
}
```

The `break` aborts the entire selection loop on the first zk-gas-exceeded failure. When the offending tx is first in iteration order, the loop produces zero transactions even though many subsequent valid transactions from other senders would have fit.

The code has no per-tx zk-gas cap at pool admission (validation at `tx_selection/mod.rs:174-188` checks regular `gas_limit` and `da_size` only), so oversized zk-gas txs can reach the selection loop freely. zk-gas is only knowable post-execution in this codebase.

## Operational context

- The affected RPC is called on a ~2s cadence by the proposer.
- In practice, the proposer always passes `max_transactions_lists = 1`, so the "start a new list" branch (`tx_selection/mod.rs:209-231`) is never exercised in production. The parameter is still honored by the code; the fix must not assume `max_lists = 1`.
- With `max_lists = 1`, the loop's only natural termination conditions are (a) the single list fills from regular gas or DA, or (b) the pool iterator is exhausted. Failed (zk-gas-exceeded) txs do not contribute to the list's gas/DA totals (lines 264-267 run only on success), so a pool dominated by zk-bombs would be scanned end-to-end.

## Decision

Adopt the minimal fix: convert the `break` into a per-tx skip that mirrors how every other per-tx failure in this loop is already handled. Defer DoS hardening and pool-admission controls as follow-ups.

## Change

At `crates/block/src/tx_selection/mod.rs:236-238`, replace the `break` with `mark_invalid` + `continue`:

```rust
Err(err) if is_zk_gas_limit_exceeded(&err) => {
    trace!(target: "tx_selection", ?tx, "skipping transaction that exceeds zk gas limit");
    best_txs.mark_invalid(&pool_tx, &<InvalidPoolTransactionError variant>);
    continue;
}
```

This matches the existing patterns in the same function:
- gas-limit too large → `mark_invalid(ExceedsGasLimit)` + `continue` (line 176)
- DA-size too large → `mark_invalid(da_limit_error(...))` + `continue` (line 186)
- list-full at `max_lists` → `mark_invalid(limit_exceeded_error(...))` + `continue` (line 206)
- non-fatal validation error → `mark_invalid(Consensus(TxTypeNotSupported))` + `continue` (line 250)

## Semantics

- **Offending tx:** skipped for this selection call only. Not removed from the underlying mempool.
- **Same sender's descendants:** automatically skipped for this selection call via `best_txs.mark_invalid`, which propagates to nonce-dependent descendants inside the pool iterator (same mechanism used elsewhere in this loop).
- **Other senders:** unaffected; their txs continue to be evaluated normally for the rest of the loop.
- **Next RPC call (~2s later):** the mempool has not been mutated, so the same tx will be re-encountered and re-executed. This is an explicitly accepted trade-off. See [Risks](#risks).

## Error variant selection

The appropriate `InvalidPoolTransactionError` variant for zk-gas-exceeded is not immediately obvious. Candidates:

1. Reuse `ExceedsGasLimit(tx_gas_limit, allowed_gas_limit)` with synthetic arguments — semantically wrong (zk gas ≠ EVM gas) and misleading to log readers.
2. Add a small helper analogous to `da_limit_error` (e.g., `zk_gas_limit_error(...)`) producing a `Consensus` or `Other` variant with a descriptive message. Clearest for log readers.
3. Use `InvalidPoolTransactionError::Other` with a descriptive message inline.

Final choice deferred to implementation-plan stage, which will read the `InvalidPoolTransactionError` enum definition and pick the most accurate fit. Option 2 is the leading candidate.

## Tests

All tests go in the existing `#[cfg(test)] mod tests` block in `crates/block/src/tx_selection/mod.rs`. The implementation plan will identify the existing test scaffolding (mock pool + mock builder) and reuse it.

1. **Regression test — zk-bomb at head of iteration.**
   A pool with `[zk_bomb_from_A, valid_tx_from_B, valid_tx_from_C]`. After selection, the returned list must contain `valid_tx_from_B` and `valid_tx_from_C`. This is the test that specifically reproduces the reported bug.

2. **Sender descendants are skipped.**
   A pool with `[zk_bomb_from_A@nonce=n, valid_tx_from_A@nonce=n+1, valid_tx_from_B]`. Exactly one execution attempt should be made for sender A — the bomb at `nonce=n`. The `nonce=n+1` tx from A must not be executed, because `mark_invalid` on the bomb causes the pool iterator to treat A's queue as unusable for the rest of this selection call. `valid_tx_from_B` must appear in the result. The assertion on "exactly one execution attempt for A" is the behavior lock referenced in the Risks section.

3. **Interleaved bombs across multiple senders.**
   A pool with `[zk_bomb_from_A, valid_tx_from_B, zk_bomb_from_C, valid_tx_from_D]`. Result contains `valid_tx_from_B` and `valid_tx_from_D`. Verifies that one sender's zk-bomb does not skip unrelated senders.

## Out of scope (follow-up work)

Each is worth its own ticket after this fix lands:

- **Per-call skip counter.** Bound per-call EVM execution cost to guard against a flood of zk-bombs from many distinct senders. Needs a new field on `TxSelectionConfig`.
- **Pool eviction on zk-gas failure.** Remove the tx from the underlying mempool instead of only from the iterator. Eliminates the recurring 2s-cadence re-execution. Requires confidence that zk-gas-exceeded is a stable property of the tx and not contention-dependent.
- **Pool-admission zk-gas cap.** Reject oversized txs at submission instead of discovering them during selection. Blocked on the absence of a cheap zk-gas estimator.
- **Cross-call failure cache.** In-memory `HashSet<TxHash>` with an invalidation policy (e.g., parent-block-change or LRU). Compromise between eviction and doing nothing.

## Risks

- **Recurring re-execution cost.** The same zk-bomb is re-executed on every ~2s RPC tick until the sender replaces or cancels it. For a small number of bombs this is negligible. For a flood, the per-call cost is bounded by mempool size times single-tx EVM cost. Monitor and revisit if it shows up in profiling; the follow-up work above addresses it.
- **Error-variant choice.** A poorly chosen `InvalidPoolTransactionError` variant could mislead log readers but will not affect runtime behavior. Resolved during implementation planning.
- **Descendant propagation assumption.** The design relies on `best_txs.mark_invalid` causing the reth pool iterator to skip the offending sender's nonce-dependent descendants. This is the documented behavior of `BestTransactions` and is the same assumption made four other times in this function. Implementation plan should add an explicit assertion in test 2 to lock this in.

## Non-goals

- Changing RPC surface (no new params, no new methods).
- Changing `TxSelectionConfig` (no new fields in this change).
- Mutating the underlying mempool.
- Introducing any cross-call state.
