# Proof-History Fail-Closed Anchor Fixes

## Context

Alethia-Reth proof-history storage is initialized from node state and then advanced by canonical notifications. Two anchor-related bugs need a narrow fix:

- Current-state initialization labels a persisted database snapshot with the provider's in-memory canonical tip.
- Reorg/revert handling can touch the retained earliest anchor even though the initial anchor has no unwindable changeset.

This design targets a small fail-closed patch. It does not add automatic proof-history wipe or rebuild behavior.

## Goals

- Ensure current-state proof-history initialization uses one consistent persisted database view for both copied state and the anchor label.
- Reject canonical reorg/revert notifications that would replace or unwind the retained earliest anchor.
- Ensure startup validation does not trust a canonical latest block while earliest is noncanonical.
- Keep the operator-facing recovery model explicit: wipe/reinitialize proof-history storage when the retained anchor is invalidated.

## Non-Goals

- No automatic deletion or rebuild of proof-history storage.
- No broad refactor of proof-history storage abstractions.
- No expensive current-state root recomputation.
- No changes to historical-window initialization beyond preserving its existing behavior.

## Design

### Current-State Initialization

`initialize_proof_history_storage` should derive the anchor from the same read-only database provider used to copy trie and hashed-state tables.

The function will:

1. Check whether proof-history storage still needs initialization.
2. Open `provider.database_provider_ro()?.disable_long_read_transaction_safety()`.
3. Read `db_provider.best_block_number()`.
4. Fetch `db_provider.sealed_header(best_number)?` and use `header.hash()` as the anchor hash.
5. Select the storage key adapter from `db_provider.cached_storage_settings().is_v2()`.
6. Move the DB transaction into `ProofHistoryInitializationJob`.
7. Call `run_with_adapter(anchor.number, anchor.hash)`.

The current `provider.chain_info()` value will not be used for this path because `BlockchainProvider::chain_info()` reflects the canonical in-memory state, while `database_provider_ro()` reads persisted DB tables.

### Startup Validation

`proof_history_startup_action` should validate earliest before returning `Ready`.

Existing behavior returns `Ready` when latest is canonical, even if earliest is noncanonical. The updated decision order should be:

1. Treat missing earliest/latest as `Uninitialized`.
2. Reject inverted bounds.
3. Require `canonical_earliest_hash == earliest_hash`; otherwise return an error.
4. If latest is within canonical best and `canonical_latest_hash == latest_hash`, return `Ready`.
5. Otherwise return `UnwindToEarliest`.

This keeps retained storage usable only when its base anchor is canonical.

### Reorg and Revert Handling

`handle_notification` should read both proof-history bounds once:

- `earliest_stored`
- `latest_stored`

It should pass both bounds into reorg/revert handlers.

Before mutating storage, `handle_chain_reorged` and `handle_chain_reverted` should fail when `old.first().number() <= earliest_stored`.

This condition means the notification would replace or unwind the retained anchor itself. The sidecar cannot unwind that anchor because initialization stores it as unversioned initial state without a per-block changeset.

The error should be explicit, for example:

```text
proof-history reorg touches retained earliest block {earliest}; wipe proof-history storage and restart initialization
```

A reorg whose first replaced block is `earliest + 1` remains allowed. In that case the common ancestor is the retained anchor, so storage can unwind and append from a canonical base.

## Error Handling

The fail-closed path should return an `eyre::Report` from the sidecar handler. This matches existing sidecar error propagation and avoids adding a partial recovery state.

The message should include:

- earliest stored block number and hash,
- first old block number and hash,
- whether the failing notification was a reorg or revert,
- a clear recovery instruction to wipe proof-history storage.

## Testing

Add targeted unit coverage around decision logic and storage initialization:

- Current-state initialization with a provider whose in-memory `chain_info().best_number` is ahead of `database_provider_ro().best_block_number()`. Assert proof-history earliest/latest use the persisted DB best block.
- `proof_history_startup_action` with canonical latest but noncanonical earliest. Assert it errors.
- Reorg where `old.first().number() == earliest_stored`. Assert the handler refuses before calling storage mutation.
- Revert where `old.first().number() == earliest_stored`. Assert the handler refuses before calling storage mutation.
- Reorg where `old.first().number() == earliest_stored + 1`. Assert the handler is still allowed.

If private method testing becomes awkward, extract small pure helpers for:

- startup decision ordering,
- anchor-touch detection.

Those helpers should be documented because repository policy requires docs on non-test Rust symbols.

## Verification

Run:

```sh
cargo test -p alethia-reth-node proof_history
just clippy
```

`just clippy` is required before completion because this repository enforces documentation coverage for production Rust symbols.
