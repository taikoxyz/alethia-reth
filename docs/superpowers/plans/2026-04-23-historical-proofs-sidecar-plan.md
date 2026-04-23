# Historical Proofs Sidecar — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a bounded-history sidecar in alethia-reth that serves `debug_executionWitness` and `eth_getProof` for in-window historical blocks with sub-second latency, replacing the ~15s revert-based slow path.

**Architecture:** Two new crates (`proofs-trie`, `proofs-exex`) hold a separate MDBX env storing versioned trie nodes + leaves, populated live by an ExEx. Existing `cli`/`rpc` crates and the `bin/alethia-reth` binary are extended to expose CLI subcommands (`proofs init|prune|unwind`) and RPC overrides (`debug_executionWitness`, `eth_getProof`) behind a `--proofs-history` opt-in flag.

**Tech Stack:** Rust 1.93 / edition 2024, reth v2.1.0 git deps, MDBX via `reth-db`, jsonrpsee 0.26 for RPC, clap for CLI, `reth-exex` for the live-write pipeline.

**Reference source:** op-rs/op-reth vendored inside `ethereum-optimism/optimism` monorepo at `/Users/davidcai/Workspace/optimism/rust/op-reth/`. Tasks that say "port from op-reth <path>" refer to that local clone; op-reth pins reth v2.0.0, so API-adaptation work may be needed for v2.1.0 on some ports. See Task 0 for the up-front feasibility spike.

---

## File Structure

### New crates

- `crates/proofs-trie/` — MDBX sidecar storage, versioned cursors, proof generation
  - `Cargo.toml` — crate manifest
  - `src/lib.rs` — crate root, public re-exports
  - `src/api.rs` — public traits: `ProofsStore`, `ProofsProviderRO`, `ProofsProviderRw`, `ProofsInitProvider`
  - `src/error.rs` — error enum wrapping MDBX + decode errors
  - `src/db/mod.rs` — MDBX-backed impl root
  - `src/db/models/mod.rs`
  - `src/db/models/versioned_value.rs` — `VersionedValue<T>`, `MaybeDeleted<T>`
  - `src/db/models/keys.rs` — `StorageTrieKey`, `HashedStorageKey`, `ProofWindowKey`, `BlockNumberHash`
  - `src/db/models/change_set.rs` — `ChangeSet` bincode struct
  - `src/db/models/tables.rs` — MDBX table definitions (`AccountTrieHistory`, `StorageTrieHistory`, `HashedAccountHistory`, `HashedStorageHistory`, `BlockChangeSet`, `ProofWindow`, `SchemaVersion`)
  - `src/db/store.rs` — `MdbxProofsStorage` — owns the MDBX env; opens / closes / schema-version checks
  - `src/db/cursor.rs` — `BlockNumberVersionedCursor`, `MdbxTrieCursor`, account/storage cursor wrappers
  - `src/cursor_factory.rs` — factory bridging sidecar cursors to `reth-trie`'s `TrieCursor`/`TrieStorageCursor` traits
  - `src/provider.rs` — `ProofsStateProvider` — implements reth's `StateProvider` + `StorageRootProvider` traits
  - `src/proof.rs` — witness + EIP-1186 proof assembly
  - `src/initialize.rs` — main-DB → sidecar backfill (block-0 baseline), resumable via `InitialStateAnchor`
  - `src/live.rs` — `LiveTrieCollector` — consumed by the ExEx
  - `src/prune/mod.rs` — pruner task + batch logic
  - `src/metrics.rs` — Prometheus metrics

- `crates/proofs-exex/` — live ExEx writer
  - `Cargo.toml`
  - `src/lib.rs` — `ProofsExEx<Node, Storage>`, builder, run loop, reorg handling
  - `src/sync.rs` — batched catch-up task

### Extensions to existing crates

- `crates/cli/src/commands/proofs/mod.rs` — subcommand router
- `crates/cli/src/commands/proofs/init.rs`
- `crates/cli/src/commands/proofs/prune.rs`
- `crates/cli/src/commands/proofs/unwind.rs`
- `crates/cli/src/args/mod.rs` — new args module
- `crates/cli/src/args/proofs_history.rs` — `ProofsHistoryArgs` clap struct
- `crates/cli/src/lib.rs` — add `proofs_history: ProofsHistoryArgs` to `TaikoCliExtArgs`
- `crates/cli/src/command.rs` — register `Proofs` subcommand variant

- `crates/rpc/src/lib.rs` — re-export new `proofs` module
- `crates/rpc/src/proofs/mod.rs`
- `crates/rpc/src/proofs/state_factory.rs` — `ProofsStateProviderFactory<Eth, Storage>`
- `crates/rpc/src/proofs/state_provider.rs` — routing `StateProvider` wrapper
- `crates/rpc/src/proofs/debug.rs` — `debug_executionWitness` override
- `crates/rpc/src/proofs/eth.rs` — `eth_getProof` override

### Binary glue

- `bin/alethia-reth/src/proofs_history.rs` — `install_proofs_history` builder helper
- `bin/alethia-reth/src/main.rs` — call the helper

### Workspace-level

- `Cargo.toml` — add `crates/proofs-trie` + `crates/proofs-exex` to `[workspace.members]`; add `reth-trie`, `reth-exex`, `reth-exex-test-utils` to `[workspace.dependencies]`

---

## Working rules for this plan

- **Branch:** do all work on `feat/proofs-history-sidecar`. Single PR at the end.
- **Commits:** after every task's final step. Commit messages follow existing convention (e.g., `feat(proofs-trie): …`, `feat(proofs-exex): …`, `feat(cli): …`, `feat(rpc): …`).
- **Format + lint:** run `just fmt && just clippy` before every commit.
- **Tests:** run `just test` before every commit; targeted `cargo nextest run -p <crate>` during development.
- **Ported code:** tasks referencing op-reth source show the full list of per-file adaptations (renames, trait-bound changes). If you encounter reth v2.0 → v2.1 API breakage not listed, STOP and update this plan rather than guessing.

---

## Task 0: Feasibility spike — reth v2.0 → v2.1 delta

**Why:** op-reth pins reth v2.0.0; alethia-reth is on v2.1.0. We need to know up front which reth types used by `proofs-trie` / `proofs-exex` have breaking changes before committing to the full port.

**Files:**
- Create: scratch branch, no committed output (notes only)

- [ ] **Step 1: Check out a scratch branch**

```bash
git checkout -b spike/proofs-v21-feasibility
```

- [ ] **Step 2: Inventory reth types the ported code touches**

Grep op-reth for reth type usages we'll inherit:

```bash
grep -hrE "use reth_(trie|provider|exex|db|evm|storage_api|execution_types)::\{?[A-Z]" \
  /Users/davidcai/Workspace/optimism/rust/op-reth/crates/trie/src/ \
  /Users/davidcai/Workspace/optimism/rust/op-reth/crates/exex/src/ \
  | sort -u > /tmp/optreth-reth-imports.txt
wc -l /tmp/optreth-reth-imports.txt
```

Expected: ~40-60 unique lines.

- [ ] **Step 3: Cross-check each type against reth v2.1.0**

For each distinct type in `/tmp/optreth-reth-imports.txt`, check its definition in the v2.1.0 tag:

```bash
# e.g.
git -C /Users/davidcai/Workspace/optimism/rust/op-reth log --oneline -- \
  $(find / -path '*reth-v2.1*' ...)  # or clone reth v2.1.0 locally for grep
```

Practical shortcut: clone `paradigmxyz/reth` at `v2.1.0` into `/tmp/reth-v21` and grep. Write a short notes file at `/tmp/v21-delta.md` listing any type whose signature changed (e.g. added trait bounds, new associated types, renamed fields).

- [ ] **Step 4: Decide on plan**

If the delta list is empty or each item is a 1-line rename: proceed with this plan unchanged. If a type has structural changes (e.g. `HashedPostState` gained a required type parameter): come back to the spec/plan and document the v2.1 adaptations per file before continuing.

- [ ] **Step 5: Discard the spike branch**

```bash
git checkout main
git branch -D spike/proofs-v21-feasibility
git checkout -b feat/proofs-history-sidecar
```

No commit — this task produces only knowledge.

---

## Task 1: Workspace scaffolding for two new crates

**Files:**
- Modify: `Cargo.toml` (workspace root) — add crate members + workspace deps
- Create: `crates/proofs-trie/Cargo.toml`
- Create: `crates/proofs-trie/src/lib.rs`
- Create: `crates/proofs-exex/Cargo.toml`
- Create: `crates/proofs-exex/src/lib.rs`

- [ ] **Step 1: Add workspace members + new workspace deps**

Modify `Cargo.toml` — add to `[workspace.members]`:
```toml
    "crates/proofs-trie",
    "crates/proofs-exex",
```

Add to `[workspace.dependencies]` (keep alphabetical inside the `# reth` block):
```toml
reth-exex = { git = "https://github.com/paradigmxyz/reth", tag = "v2.1.0" }
reth-exex-test-utils = { git = "https://github.com/paradigmxyz/reth", tag = "v2.1.0" }
reth-trie = { git = "https://github.com/paradigmxyz/reth", tag = "v2.1.0", default-features = false }
```

- [ ] **Step 2: Create proofs-trie Cargo.toml**

Write `crates/proofs-trie/Cargo.toml`:
```toml
[package]
name = "alethia-reth-proofs-trie"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[lib]
path = "src/lib.rs"

[dependencies]
# reth
reth-codecs = { workspace = true }
reth-db = { workspace = true, features = ["mdbx"] }
reth-db-api = { workspace = true }
reth-evm = { workspace = true, default-features = false }
reth-execution-errors = { workspace = true }
reth-primitives-traits = { workspace = true }
reth-provider = { workspace = true }
reth-revm = { workspace = true, default-features = false }
reth-storage-errors = { workspace = true, default-features = false }
reth-tasks = { workspace = true }
reth-trie = { workspace = true, features = ["serde"] }
reth-trie-common = { workspace = true, features = ["serde"] }
reth-trie-db = { workspace = true }

# alloy / ethereum
alloy-eips = { workspace = true }
alloy-primitives = { workspace = true }

# async + sync
parking_lot = { workspace = true }
tokio = { workspace = true, features = ["sync"] }

# codec
bincode = { workspace = true }
bytes = { workspace = true }
serde = { workspace = true }

# misc
auto_impl = { workspace = true }
derive_more = { workspace = true }
eyre = { workspace = true }
strum = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }

# metrics (always on for now)
metrics = { workspace = true }
reth-metrics = { workspace = true, features = ["common"] }

[dev-dependencies]
reth-db = { workspace = true, features = ["mdbx", "test-utils"] }
reth-provider = { workspace = true, features = ["test-utils"] }
reth-trie = { workspace = true, features = ["test-utils"] }
tempfile = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "test-util"] }
```

If any workspace dep here is missing from the root `Cargo.toml`, add it in the same step (likely: `bincode`, `bytes`, `parking_lot`, `strum`, `metrics`, `reth-metrics`, `reth-codecs`, `reth-execution-errors`, `tempfile`, `auto_impl` — check first with `grep -E '^(bincode|bytes|parking_lot|strum|metrics|reth-metrics|reth-codecs|reth-execution-errors|tempfile|auto_impl)\s*=' Cargo.toml`; add any missing ones pointing at the standard reth v2.1.0 tag where relevant, crates.io versions matching what reth itself uses for the rest — consult op-reth's top-level `Cargo.toml` at `/Users/davidcai/Workspace/optimism/rust/Cargo.toml` for the exact versions).

- [ ] **Step 3: Create proofs-trie stub lib.rs**

Write `crates/proofs-trie/src/lib.rs`:
```rust
//! Bounded-history proof sidecar storage for alethia-reth.
//!
//! This crate provides a versioned, append-only MDBX store of account and storage
//! trie data, enabling sub-second historical `eth_getProof` / `debug_executionWitness`
//! within a configurable retention window.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
```

- [ ] **Step 4: Create proofs-exex Cargo.toml and stub lib.rs**

Write `crates/proofs-exex/Cargo.toml`:
```toml
[package]
name = "alethia-reth-proofs-exex"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[lib]
path = "src/lib.rs"

[dependencies]
alethia-reth-proofs-trie = { path = "../proofs-trie" }

reth-execution-types = { workspace = true }
reth-exex = { workspace = true }
reth-node-api = { workspace = true }
reth-provider = { workspace = true }
reth-trie = { workspace = true }

alloy-consensus = { workspace = true }
alloy-eips = { workspace = true }

eyre = { workspace = true }
futures-util = { workspace = true }
tokio = { workspace = true, features = ["sync", "time"] }
tracing = { workspace = true }

[dev-dependencies]
reth-exex-test-utils = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt-multi-thread", "test-util"] }
tempfile = { workspace = true }
```

Write `crates/proofs-exex/src/lib.rs`:
```rust
//! Live execution-extension that writes canonical block state into the proofs sidecar.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
```

- [ ] **Step 5: Build + verify + commit**

Run:
```bash
just fmt && cargo check --workspace
```
Expected: workspace builds. No lints from the stub crates since they're empty.

```bash
git add Cargo.toml crates/proofs-trie crates/proofs-exex
git commit -m "feat(proofs): scaffold proofs-trie and proofs-exex crates"
```

---

## Task 2: Port data models — versioned values, keys, change set

**Files:**
- Create: `crates/proofs-trie/src/db/mod.rs`
- Create: `crates/proofs-trie/src/db/models/mod.rs`
- Create: `crates/proofs-trie/src/db/models/versioned_value.rs`
- Create: `crates/proofs-trie/src/db/models/keys.rs`
- Create: `crates/proofs-trie/src/db/models/change_set.rs`
- Create: `crates/proofs-trie/src/error.rs`

**Reference source:**
- `/Users/davidcai/Workspace/optimism/rust/op-reth/crates/trie/src/db/models/` (entire directory — `versioned_value.rs`, `keys.rs`, `change_set.rs`)

**Per-file adaptations:**
- Rename any `OpProofs*` types → `Proofs*`
- Update doc comments referencing "OP" → "Taiko" where applicable
- Keep all encoding logic byte-identical (we're adopting the same schema)

- [ ] **Step 1: Port `versioned_value.rs`**

Copy op-reth's `db/models/versioned_value.rs` content into `crates/proofs-trie/src/db/models/versioned_value.rs`. Apply renames noted above. This file defines `VersionedValue<T>` and `MaybeDeleted<T>` with custom `Compact` encode/decode implementing the block-number-prefixed versioning scheme documented in the spec section 2.

- [ ] **Step 2: Port `keys.rs`**

Copy op-reth's `db/models/keys.rs` to `crates/proofs-trie/src/db/models/keys.rs`. Defines:
- `StorageTrieKey` = `hashed_address (32B) ‖ StoredNibbles(path)`
- `HashedStorageKey` = `hashed_address (32B) ‖ hashed_slot (32B)` (fixed 64B)
- `ProofWindowKey` = `EarliestBlock | LatestBlock` (1-byte tag)
- `BlockNumberHash` = `block_number (8B BE) ‖ block_hash (32B)` (fixed 40B)

- [ ] **Step 3: Port `change_set.rs`**

Copy op-reth's `db/models/change_set.rs` to `crates/proofs-trie/src/db/models/change_set.rs`. Defines bincode-encoded `ChangeSet { account_trie_keys, storage_trie_keys, hashed_account_keys, hashed_storage_keys }`.

- [ ] **Step 4: Write module-wiring files**

Write `crates/proofs-trie/src/db/mod.rs`:
```rust
pub mod models;
```

Write `crates/proofs-trie/src/db/models/mod.rs`:
```rust
pub mod change_set;
pub mod keys;
pub mod versioned_value;

pub use change_set::*;
pub use keys::*;
pub use versioned_value::*;
```

Write `crates/proofs-trie/src/error.rs`:
```rust
use reth_db::DatabaseError;
use thiserror::Error;

/// Errors produced by the proofs-trie storage layer.
#[derive(Debug, Error)]
pub enum ProofsStorageError {
    /// MDBX / database-layer error.
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// Codec failure decoding a row.
    #[error("decode error: {0}")]
    Decode(String),
    /// Storage is not yet initialized (run `alethia-reth proofs init`).
    #[error("proofs storage not initialized")]
    NotInitialized,
    /// Schema version mismatch — the MDBX env was written by an incompatible sidecar version.
    #[error("schema version mismatch: expected {expected}, found {found}")]
    SchemaVersionMismatch { expected: u32, found: u32 },
}

/// Shorthand result type for the proofs-trie crate.
pub type ProofsStorageResult<T> = Result<T, ProofsStorageError>;
```

Wire both into `crates/proofs-trie/src/lib.rs`:
```rust
//! Bounded-history proof sidecar storage for alethia-reth.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod db;
pub mod error;

pub use error::{ProofsStorageError, ProofsStorageResult};
```

- [ ] **Step 5: Port encode/decode round-trip tests**

Each of the three op-reth model files ships with unit tests in `#[cfg(test)] mod tests`. Port those verbatim into the corresponding new file. These tests use `reth-codecs`' `Compact` trait to round-trip encode/decode each struct.

- [ ] **Step 6: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-trie
```
Expected: all model round-trip tests pass.

```bash
just fmt && just clippy -p alethia-reth-proofs-trie
git add Cargo.toml crates/proofs-trie/src/
git commit -m "feat(proofs-trie): port versioned value, keys, and change-set models"
```

---

## Task 3: Port table definitions + schema version check

**Files:**
- Create: `crates/proofs-trie/src/db/models/tables.rs`
- Modify: `crates/proofs-trie/src/db/models/mod.rs` (re-export `tables`)

**Reference source:**
- op-reth's `crates/trie/src/db/models/tables.rs` (or equivalent — locate via `grep -l AccountTrieHistory /Users/davidcai/Workspace/optimism/rust/op-reth/crates/trie/src/db/models/*.rs`)

- [ ] **Step 1: Port the seven tables**

Write `crates/proofs-trie/src/db/models/tables.rs` containing the reth-db `tables!` macro invocation for:
- `AccountTrieHistory` (DupSort) — `StoredNibbles → (u64 subkey, VersionedValue<BranchNodeCompact>)`
- `StorageTrieHistory` (DupSort) — `StorageTrieKey → (u64, VersionedValue<BranchNodeCompact>)`
- `HashedAccountHistory` (DupSort) — `B256 → (u64, VersionedValue<Account>)`
- `HashedStorageHistory` (DupSort) — `HashedStorageKey → (u64, VersionedValue<U256>)`
- `BlockChangeSet` — `u64 → ChangeSet` (bincode)
- `ProofWindow` — `ProofWindowKey → BlockNumberHash`
- `SchemaVersion` — `() → u32` *(NEW, not present in op-reth)*

The exact macro syntax is dictated by `reth-db`'s `tables!` macro; mirror the op-reth file verbatim except for the `SchemaVersion` addition and the `OpProofs` → `Proofs` renames.

- [ ] **Step 2: Add module re-export**

In `crates/proofs-trie/src/db/models/mod.rs`:
```rust
pub mod change_set;
pub mod keys;
pub mod tables;
pub mod versioned_value;

pub use change_set::*;
pub use keys::*;
pub use tables::*;
pub use versioned_value::*;
```

- [ ] **Step 3: Add schema-version constant**

Append to `crates/proofs-trie/src/db/models/tables.rs`:
```rust
/// Current proofs-trie on-disk schema version. Bump on any breaking change to
/// table keys/values, `VersionedValue` encoding, or `ChangeSet` encoding.
pub const PROOFS_TRIE_SCHEMA_VERSION: u32 = 1;
```

- [ ] **Step 4: Run + commit**

```bash
cargo check -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
```
Expected: compiles clean. No tests yet for this module (table macros are exercised via the store in Task 4).

```bash
git add crates/proofs-trie/src/
git commit -m "feat(proofs-trie): define MDBX tables + schema version constant"
```

---

## Task 4: Port `MdbxProofsStorage` — environment wrapper + schema-version gate

**Files:**
- Create: `crates/proofs-trie/src/db/store.rs`
- Modify: `crates/proofs-trie/src/db/mod.rs` (re-export)

**Reference source:** op-reth's `crates/trie/src/db/store.rs`

**Per-file adaptations:**
- Rename `MdbxOpProofsStorage` → `MdbxProofsStorage`, `OpProofsProvider*` → `ProofsProvider*`
- After `MdbxProofsStorage::new(path)` opens the env, add a schema-version check:
  - On fresh env (no `SchemaVersion` row): write `PROOFS_TRIE_SCHEMA_VERSION`
  - On existing env: read `SchemaVersion`; if not equal to `PROOFS_TRIE_SCHEMA_VERSION`, return `ProofsStorageError::SchemaVersionMismatch`
- Keep the existing provider-factory pattern (`provider_ro`, `provider_rw`, `initialization_provider`)

- [ ] **Step 1: Port the file with adaptations**

Write `crates/proofs-trie/src/db/store.rs` by porting op-reth's `db/store.rs` with the renames above plus the schema-version gate. The schema-version gate goes at the top of `MdbxProofsStorage::new()`:

```rust
// After opening the env, before returning:
{
    let tx = env.tx_mut()?;
    match tx.get::<SchemaVersion>(())? {
        None => {
            tx.put::<SchemaVersion>((), PROOFS_TRIE_SCHEMA_VERSION)?;
        }
        Some(found) if found == PROOFS_TRIE_SCHEMA_VERSION => {}
        Some(found) => {
            return Err(ProofsStorageError::SchemaVersionMismatch {
                expected: PROOFS_TRIE_SCHEMA_VERSION,
                found,
            });
        }
    }
    tx.commit()?;
}
```

- [ ] **Step 2: Add module re-export**

Update `crates/proofs-trie/src/db/mod.rs`:
```rust
pub mod models;
pub mod store;

pub use store::MdbxProofsStorage;
```

- [ ] **Step 3: Write schema-version gate test**

Add to the bottom of `crates/proofs-trie/src/db/store.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fresh_env_writes_schema_version() {
        let dir = TempDir::new().unwrap();
        let storage = MdbxProofsStorage::new(dir.path()).unwrap();
        drop(storage);

        // Reopen; must still work.
        let _reopened = MdbxProofsStorage::new(dir.path()).unwrap();
    }

    #[test]
    fn schema_mismatch_rejected() {
        let dir = TempDir::new().unwrap();
        {
            let storage = MdbxProofsStorage::new(dir.path()).unwrap();
            let env = storage.env();
            let tx = env.tx_mut().unwrap();
            tx.put::<SchemaVersion>((), 999).unwrap();
            tx.commit().unwrap();
        }
        let err = MdbxProofsStorage::new(dir.path()).unwrap_err();
        assert!(matches!(err, ProofsStorageError::SchemaVersionMismatch { expected: 1, found: 999 }));
    }
}
```

(Expose `env()` as `pub(crate)` accessor on `MdbxProofsStorage` to make this test reachable, if the ported struct doesn't already.)

- [ ] **Step 4: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
```
Expected: 2 store tests pass in addition to Task 2's model tests.

```bash
git add crates/proofs-trie/src/
git commit -m "feat(proofs-trie): port MdbxProofsStorage with schema-version gate"
```

---

## Task 5: Port cursor implementations

**Files:**
- Create: `crates/proofs-trie/src/db/cursor.rs`
- Modify: `crates/proofs-trie/src/db/mod.rs`

**Reference source:** op-reth's `crates/trie/src/db/cursor.rs` (~600 LoC — the largest single file in the port)

**Per-file adaptations:**
- Rename as before (no OP-specific types in this file; renames are cosmetic)

- [ ] **Step 1: Port the file verbatim**

Copy op-reth's `db/cursor.rs` to `crates/proofs-trie/src/db/cursor.rs`. Apply cosmetic renames. This file implements:
- `BlockNumberVersionedCursor<T, Cursor>` — low-level versioned lookup for a `DupSort` table
- `MdbxTrieCursor<T, Cursor>` — wraps `BlockNumberVersionedCursor` and implements reth-trie's `TrieCursor` trait for `AccountTrieHistory` and `TrieStorageCursor` for `StorageTrieHistory`
- `MdbxAccountCursor<Cursor>` / `MdbxStorageCursor<Cursor>` — leaf-value cursors
- Several free helpers for opening a named cursor type against a given `DbTx`

- [ ] **Step 2: Port the regression tests**

The op-reth file ships with a substantial `#[cfg(test)] mod tests` — port those tests verbatim. They validate:
- `seek_by_subkey` returns the highest entry ≤ target
- Tombstones are surfaced as `MaybeDeleted::None`
- `next()` after `seek()` advances correctly (including the regression test at line 1434 referenced in op-reth source)
- `TrieCursor` interface behaviors

- [ ] **Step 3: Add module re-export**

Update `crates/proofs-trie/src/db/mod.rs`:
```rust
pub mod cursor;
pub mod models;
pub mod store;

pub use cursor::*;
pub use store::MdbxProofsStorage;
```

- [ ] **Step 4: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
```
Expected: all ported cursor tests pass.

```bash
git add crates/proofs-trie/src/
git commit -m "feat(proofs-trie): port versioned MDBX cursor implementations"
```

---

## Task 6: Port public API traits

**Files:**
- Create: `crates/proofs-trie/src/api.rs`
- Modify: `crates/proofs-trie/src/lib.rs`

**Reference source:** op-reth's `crates/trie/src/api.rs`

**Per-file adaptations:**
- Rename `OpProofsStore` → `ProofsStore`, `OpProofsProviderRO` → `ProofsProviderRO`, etc.
- Keep trait signatures identical (we want the same surface for drop-in cursor/state-provider integration)

- [ ] **Step 1: Port api.rs**

Copy op-reth's `crates/trie/src/api.rs` to `crates/proofs-trie/src/api.rs` with renames. Defines:
- `ProofsStore` — factory trait (`provider_ro`, `provider_rw`, `initialization_provider`)
- `ProofsProviderRO` — read queries (`get_earliest_block_number`, `get_latest_block_number`, various gets)
- `ProofsProviderRw` — batch writes (`store_trie_updates`, `store_hashed_state`, `set_latest_block_number`, `set_earliest_block_number`, `replace_updates`, `commit`)
- `ProofsInitProvider` — `initial_state_anchor`, `create_anchor`, `update_anchor`, `set_earliest_block_number`, `commit`
- `InitialStateAnchor`, `InitialStateStatus` — resumable-init support types

- [ ] **Step 2: Export from lib.rs**

Update `crates/proofs-trie/src/lib.rs`:
```rust
//! Bounded-history proof sidecar storage for alethia-reth.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod api;
pub mod db;
pub mod error;

pub use api::*;
pub use db::MdbxProofsStorage;
pub use error::{ProofsStorageError, ProofsStorageResult};
```

- [ ] **Step 3: Run + commit**

```bash
cargo check -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
```
Expected: compiles.

```bash
git add crates/proofs-trie/src/
git commit -m "feat(proofs-trie): define ProofsStore trait family"
```

---

## Task 7a (v2.1 delta): Port `in_memory.rs` test utility

**Why added:** per Task 0 spike findings — `provider.rs` and the ExEx both use `InMemoryProofsStorage` in their tests. Must port before Task 8 (provider tests) and Task 13 (ExEx tests) can pass.

**Files:**
- Create: `crates/proofs-trie/src/in_memory.rs`
- Modify: `crates/proofs-trie/src/lib.rs` — re-export

**Reference source:** op-reth's `crates/trie/src/in_memory.rs` (~930 LoC)

**Per-file adaptations:**
- Renames (`OpProofs*` → `Proofs*`)
- **v2.1 delta:** `reth_provider::noop::NoopProvider` moved to `reth_provider::test_utils::NoopProvider` — fix this import
- Gate the module behind `#[cfg(any(test, feature = "test-utils"))]` if op-reth doesn't already; test-only code shouldn't bloat the prod build

- [ ] **Step 1: Port in_memory.rs**

Copy with renames and the NoopProvider import fix. Exposes `InMemoryProofsStorage` implementing `ProofsStore`, `ProofsProviderRO`, `ProofsProviderRw`, `ProofsInitProvider`.

- [ ] **Step 2: Port the file's own round-trip tests**

op-reth's `in_memory.rs` ships tests at the bottom of the file — port them.

- [ ] **Step 3: Add to lib.rs**

```rust
#[cfg(any(test, feature = "test-utils"))]
pub mod in_memory;
#[cfg(any(test, feature = "test-utils"))]
pub use in_memory::InMemoryProofsStorage;
```

Add a `test-utils` feature to `crates/proofs-trie/Cargo.toml`:
```toml
[features]
test-utils = []
```

- [ ] **Step 4: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
git add crates/proofs-trie/
git commit -m "feat(proofs-trie): port InMemoryProofsStorage test utility"
```

---

## Task 7: Implement `ProofsStore` + providers against `MdbxProofsStorage`

**Files:**
- Modify: `crates/proofs-trie/src/db/store.rs` — impl `ProofsStore`, `ProofsProviderRO`, `ProofsProviderRw`, `ProofsInitProvider`

**Reference source:** the `impl` blocks at the end of op-reth's `crates/trie/src/db/store.rs`

**Per-file adaptations:** identical to Task 4's (renames only).

- [ ] **Step 1: Port the impl blocks**

Append to `crates/proofs-trie/src/db/store.rs` (or keep in that file — it's where they live in op-reth) the impls of:
- `impl ProofsStore for Arc<MdbxProofsStorage>`
- `impl ProofsProviderRO for MdbxRoProvider<'_>` — where `MdbxRoProvider` is the type op-reth uses for RO tx handles
- `impl ProofsProviderRw for MdbxRwProvider<'_>`
- `impl ProofsInitProvider for MdbxInitProvider<'_>`

- [ ] **Step 2: Port the impl-block tests**

Port the op-reth tests that verify write-then-read round-trips: populate a few versioned rows via `ProofsProviderRw`, then read back via `ProofsProviderRO`, assert correctness including tombstone handling.

- [ ] **Step 3: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
```
Expected: RW round-trip tests pass.

```bash
git add crates/proofs-trie/src/
git commit -m "feat(proofs-trie): implement ProofsStore + RO/Rw/Init providers"
```

---

## Task 8: Port `cursor_factory`, `provider`, and `proof` — bridge to reth-trie

**Files:**
- Create: `crates/proofs-trie/src/cursor_factory.rs`
- Create: `crates/proofs-trie/src/provider.rs`
- Create: `crates/proofs-trie/src/proof.rs`
- Modify: `crates/proofs-trie/src/lib.rs`

**Reference source:**
- `/Users/davidcai/Workspace/optimism/rust/op-reth/crates/trie/src/cursor_factory.rs`
- `/Users/davidcai/Workspace/optimism/rust/op-reth/crates/trie/src/provider.rs`
- `/Users/davidcai/Workspace/optimism/rust/op-reth/crates/trie/src/proof.rs`

**Per-file adaptations:** renames only.

- [ ] **Step 1: Port `cursor_factory.rs`**

Copy op-reth's `cursor_factory.rs` to `crates/proofs-trie/src/cursor_factory.rs`. It provides a `TrieCursorFactory`/`HashedCursorFactory` impl using the sidecar cursors, so the standard reth-trie proof-generation plumbing can run unchanged against our store.

- [ ] **Step 2: Port `provider.rs`**

Copy op-reth's `provider.rs` to `crates/proofs-trie/src/provider.rs`. Provides `ProofsStateProvider` that implements reth's `StateProvider` + `StorageRootProvider` traits, backed by `ProofsProviderRO` cursors at a specific target block.

**v2.1 delta (required):** reth v2.1.0 added an `ExecutionWitnessMode` parameter to `StateProofProvider::witness`. At op-reth's `crates/trie/src/provider.rs:180` (roughly), the signature is:
```rust
fn witness(&self, input: HashedPostState) -> Result<Vec<Bytes>, _> { ... }
```
Update it to:
```rust
fn witness(
    &self,
    mode: reth_trie_common::ExecutionWitnessMode,
    input: HashedPostState,
) -> Result<Vec<Bytes>, _> { ... }
```
Forward `mode` into `TrieWitness::compute` if that method now takes it too, otherwise ignore (the trait just surfaces the mode for the caller to decide behavior). Cross-check against `/tmp/reth-v21/crates/trie/provider/src/lib.rs` for the exact current trait definition.

- [ ] **Step 3: Port `proof.rs`**

Copy op-reth's `proof.rs` to `crates/proofs-trie/src/proof.rs`. Contains helpers used by both the eth_getProof override and the debug_executionWitness override to assemble EIP-1186 proofs and witness objects.

- [ ] **Step 4: Export from lib.rs**

Update `crates/proofs-trie/src/lib.rs`:
```rust
//! Bounded-history proof sidecar storage for alethia-reth.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod api;
pub mod cursor_factory;
pub mod db;
pub mod error;
pub mod proof;
pub mod provider;

pub use api::*;
pub use db::MdbxProofsStorage;
pub use error::{ProofsStorageError, ProofsStorageResult};
pub use provider::ProofsStateProvider;
```

- [ ] **Step 5: Port the integration tests**

op-reth's `provider.rs` + `proof.rs` ship with tests that: write a synthetic block to the sidecar, build a `ProofsStateProvider` at that block, request a proof, verify the proof against the known state root. Port those.

- [ ] **Step 6: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
```
Expected: proof round-trip tests pass.

```bash
git add crates/proofs-trie/src/
git commit -m "feat(proofs-trie): bridge cursors + state provider + proof generation to reth-trie"
```

---

## Task 9: Port `initialize.rs` — backfill from main DB

**Files:**
- Create: `crates/proofs-trie/src/initialize.rs`
- Modify: `crates/proofs-trie/src/lib.rs`

**Reference source:** op-reth's `crates/trie/src/initialize.rs`

**Per-file adaptations:**
- Renames
- The main-DB source tables op-reth reads (`HashedAccounts`, `HashedStorages`, `AccountsTrie`, `StoragesTrie`) are reth core types — same names in alethia-reth's v2.1.0 reth deps. No Taiko-specific source-table changes.

- [ ] **Step 1: Port initialize.rs**

Port verbatim. Produces `Initializer::init(&main_db, &mut init_provider)` that:
1. Reads (or creates) `InitialStateAnchor`
2. Resumes from last-written key per table
3. Streams `HashedAccounts` → writes `HashedAccountHistory[block=0]`; same for `HashedStorages`, `AccountsTrie`, `StoragesTrie`
4. Commits `ProofWindow[EarliestBlock] = (0, genesis_hash)`
5. Marks anchor `Completed`

- [ ] **Step 2: Export from lib.rs**

Add `pub mod initialize;` and `pub use initialize::Initializer;`.

- [ ] **Step 3: Port init tests**

op-reth ships tests that: populate a synthetic main DB with a few accounts + slots + trie nodes, run `Initializer::init`, verify sidecar contains the expected baseline rows at block 0. Port them.

- [ ] **Step 4: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
```
Expected: init tests pass.

```bash
git add crates/proofs-trie/src/
git commit -m "feat(proofs-trie): port resumable main-DB → sidecar backfill"
```

---

## Task 10: Port `prune/` module

**Files:**
- Create: `crates/proofs-trie/src/prune/mod.rs` (and any sub-files op-reth has in its `prune/` dir)
- Modify: `crates/proofs-trie/src/lib.rs`

**Reference source:** op-reth's `crates/trie/src/prune/` (full directory)

- [ ] **Step 1: Port the prune module**

Copy each file from op-reth's `prune/` into `crates/proofs-trie/src/prune/`. Apply renames. Key public items ported:
- `ProofsStoragePrunerTask` (ported with `OpProofStoragePrunerTask` → `ProofsStoragePrunerTask`)
- `prune_batch(provider_rw, target_earliest_block, batch_size) -> eyre::Result<PruneStats>`
- Pruner reads `BlockChangeSet[N]` for each prunable block, deletes the referenced versioned rows, then deletes the changeset.

- [ ] **Step 2: Export from lib.rs**

Add `pub mod prune;` and `pub use prune::ProofsStoragePrunerTask;`.

- [ ] **Step 3: Port prune tests**

Integration tests that: populate sidecar with K blocks of history, run pruner with window=N<K, assert rows ≤ `K-N` are gone and rows > `K-N` remain.

- [ ] **Step 4: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
```
Expected: prune tests pass.

```bash
git add crates/proofs-trie/src/
git commit -m "feat(proofs-trie): port window-based pruner"
```

---

## Task 11: Port `live.rs` — `LiveTrieCollector`

**Files:**
- Create: `crates/proofs-trie/src/live.rs`
- Modify: `crates/proofs-trie/src/lib.rs`

**Reference source:** op-reth's `crates/trie/src/live.rs`

- [ ] **Step 1: Port live.rs**

Copy op-reth's `live.rs`. `LiveTrieCollector<Evm, Provider, Storage>` processes an `ExecutionOutcome` + `TrieUpdates` for one block and writes:
- Trie branches → `AccountTrieHistory` / `StorageTrieHistory`
- Leaf values → `HashedAccountHistory` / `HashedStorageHistory`
- Modified-keys union → `BlockChangeSet[block_number]`
- Updates `ProofWindow[LatestBlock]`

- [ ] **Step 2: Export from lib.rs**

Add `pub mod live;` and `pub use live::LiveTrieCollector;`.

- [ ] **Step 3: Port live tests**

op-reth's `live.rs` ships tests producing a synthetic block, invoking `LiveTrieCollector`, verifying that subsequent `ProofsProviderRO` reads surface those values. Port them.

- [ ] **Step 4: Port metrics.rs**

Copy op-reth's `crates/trie/src/metrics.rs` to `crates/proofs-trie/src/metrics.rs` with renames. Expose via `pub mod metrics;` in `lib.rs`.

- [ ] **Step 5: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-trie
just fmt && just clippy -p alethia-reth-proofs-trie
```
Expected: live + metrics tests pass.

```bash
git add crates/proofs-trie/src/
git commit -m "feat(proofs-trie): port LiveTrieCollector + metrics"
```

---

## Task 12: Port `ProofsExEx` — constructor, builder, ensure_initialized

**Files:**
- Modify: `crates/proofs-exex/src/lib.rs` (replace stub)

**Reference source:** op-reth's `crates/exex/src/lib.rs` (Lines 1–300 roughly — the struct, builder, `ensure_initialized`, and helpers)

**Per-file adaptations:**
- `OpProofsExEx` → `ProofsExEx`, `OpProofsExExBuilder` → `ProofsExExBuilder`
- Error message strings referencing `op-reth` → `alethia-reth`
- Tracing target strings `optimism::exex` → `alethia::exex::proofs`

- [ ] **Step 1: Port constants + builder + struct**

Replace `crates/proofs-exex/src/lib.rs` content with the first ~200 lines of op-reth's `exex/src/lib.rs`, adapted. Includes:
- `MAX_PRUNE_BLOCKS_STARTUP = 1000`
- `SYNC_BLOCKS_BATCH_SIZE = 50`
- `REAL_TIME_BLOCKS_THRESHOLD = 1024`
- `SYNC_IDLE_SLEEP_SECS = 5`
- `DEFAULT_PROOFS_HISTORY_WINDOW = 259_200` **(Taiko-specific, not op-reth's 1_296_000)** — matches the spec's 72h default at 1s block time
- `DEFAULT_PRUNE_INTERVAL = Duration::from_secs(15)`
- `DEFAULT_VERIFICATION_INTERVAL = 0`
- `ProofsExExBuilder<Node, Storage>` with `with_proofs_history_window`, `with_proofs_history_prune_interval`, `with_verification_interval` setters, and `build()`
- `ProofsExEx<Node, Storage>` struct and constructor

- [ ] **Step 2: Port `ensure_initialized`**

Adapt the function from op-reth. Critical safety check: on startup, if `target_earliest > earliest_block_number && blocks_to_prune > MAX_PRUNE_BLOCKS_STARTUP`, refuse to start with an error pointing to `alethia-reth proofs prune`. Also updates the earliest-block metric gauge.

- [ ] **Step 3: Run + commit**

```bash
cargo check -p alethia-reth-proofs-exex
just fmt && just clippy -p alethia-reth-proofs-exex
```
Expected: compiles, no clippy warnings (run loop not yet present — that's Task 13).

```bash
git add crates/proofs-exex/
git commit -m "feat(proofs-exex): port ProofsExEx builder + ensure_initialized"
```

---

## Task 13: Port `ProofsExEx` run loop + sync catchup

**Files:**
- Modify: `crates/proofs-exex/src/lib.rs`
- Create: `crates/proofs-exex/src/sync.rs`

**Reference source:** op-reth's `crates/exex/src/lib.rs` (`run()`, `spawn_sync_task`, `sync_loop`, `process_batch`, `handle_notification`) + sync-task helpers

- [ ] **Step 1: Port `run()` + spawn helpers**

Port the `run()` method and supporting helpers. Flow:
1. `ensure_initialized()`
2. `spawn_sync_task()` — returns a `watch::Sender<u64>` used later to signal the sync task
3. Spawn the pruner task via `ctx.task_executor().spawn_with_graceful_shutdown_signal(...)`, using `ProofsStoragePrunerTask::new(...)`
4. Construct a `LiveTrieCollector`
5. Loop `while let Some(notification) = ctx.notifications.try_next().await?` → `handle_notification(notification, &collector, &sync_target_tx)`

- [ ] **Step 2: Port `handle_notification`**

Translates `ExExNotification` (Committed | Reverted | Reorged) into sidecar writes via `LiveTrieCollector` + unwinds via `ProofsProviderRw::replace_updates`. CRITICAL: only emit `ctx.events.send(ExExEvent::FinishedHeight(...))` AFTER the sidecar commit has succeeded for that height — lose this invariant and reth may prune reth-side changesets we still need.

- [ ] **Step 3: Port the batched sync/catchup path**

Move the `sync_loop` + `process_batch` to `crates/proofs-exex/src/sync.rs` for clarity (op-reth keeps everything in lib.rs; we split because sync is a bounded concern). Wire up in `lib.rs` via `mod sync; pub(crate) use sync::*;`.

- [ ] **Step 4: Port the ExEx tests**

op-reth's `exex/src/lib.rs` ships `#[cfg(test)]` tests using `reth-exex-test-utils`. Port them, including:
- Commit + query round-trip
- Reorg unwind at varying depths
- Revert (no new chain) unwind
- FinishedHeight emission order
- Startup prune-threshold rejection

- [ ] **Step 5: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-exex
just fmt && just clippy -p alethia-reth-proofs-exex
```
Expected: ExEx tests pass.

```bash
git add crates/proofs-exex/
git commit -m "feat(proofs-exex): port run loop, reorg handling, and sync catchup"
```

---

## Task 14: Add `ProofsHistoryArgs` to CLI

**Files:**
- Create: `crates/cli/src/args/mod.rs`
- Create: `crates/cli/src/args/proofs_history.rs`
- Modify: `crates/cli/src/lib.rs` — extend `TaikoCliExtArgs`
- Modify: `crates/cli/Cargo.toml` — add `humantime-serde` or equivalent if not already there

- [ ] **Step 1: Define the args struct**

Write `crates/cli/src/args/proofs_history.rs`:
```rust
//! CLI flags for the historical-proofs sidecar.

use std::path::PathBuf;
use std::time::Duration;

use clap::Args;

/// Configuration for the historical-proofs sidecar. All flags are optional;
/// the sidecar is enabled by setting `--proofs-history`.
#[derive(Debug, Clone, Args)]
pub struct ProofsHistoryArgs {
    /// Enable the historical-proofs sidecar.
    #[arg(long = "proofs-history", env = "RETH_PROOFS_HISTORY")]
    pub enabled: bool,

    /// Path to the sidecar MDBX environment. Required when the sidecar is enabled.
    #[arg(
        long = "proofs-history.storage-path",
        env = "RETH_PROOFS_HISTORY_STORAGE_PATH"
    )]
    pub storage_path: Option<PathBuf>,

    /// Retention window in blocks. Default is 259_200 (72 hours at 1s block time).
    #[arg(
        long = "proofs-history.window",
        env = "RETH_PROOFS_HISTORY_WINDOW",
        default_value_t = 259_200
    )]
    pub window: u64,

    /// Interval between prune runs.
    #[arg(
        long = "proofs-history.prune-interval",
        env = "RETH_PROOFS_HISTORY_PRUNE_INTERVAL",
        value_parser = humantime::parse_duration,
        default_value = "15s"
    )]
    pub prune_interval: Duration,

    /// Maximum blocks processed per prune batch.
    #[arg(
        long = "proofs-history.prune-batch-size",
        env = "RETH_PROOFS_HISTORY_PRUNE_BATCH_SIZE",
        default_value_t = 10_000
    )]
    pub prune_batch_size: u64,

    /// Full block re-execution integrity-check interval (0 = disabled).
    #[arg(
        long = "proofs-history.verification-interval",
        env = "RETH_PROOFS_HISTORY_VERIFICATION_INTERVAL",
        default_value_t = 0
    )]
    pub verification_interval: u64,
}

impl ProofsHistoryArgs {
    /// Validate flag combinations. Run at startup before any allocation.
    pub fn validate(&self) -> eyre::Result<()> {
        if self.enabled && self.storage_path.is_none() {
            eyre::bail!(
                "--proofs-history.storage-path is required when --proofs-history is enabled"
            );
        }
        if self.enabled && self.window == 0 {
            eyre::bail!("--proofs-history.window must be greater than 0");
        }
        Ok(())
    }
}
```

- [ ] **Step 2: Write module root**

Write `crates/cli/src/args/mod.rs`:
```rust
pub mod proofs_history;

pub use proofs_history::ProofsHistoryArgs;
```

- [ ] **Step 3: Add humantime to Cargo.toml**

Modify `crates/cli/Cargo.toml` — add `humantime = { workspace = true }` under `[dependencies]`. If `humantime` is not in the root workspace deps, add it there too (version `"2"`).

- [ ] **Step 4: Extend `TaikoCliExtArgs`**

Modify `crates/cli/src/lib.rs`: add `pub mod args;` near the top module declarations, then add a field to `TaikoCliExtArgs`:
```rust
#[command(flatten)]
pub proofs_history: crate::args::ProofsHistoryArgs,
```

(Locate `TaikoCliExtArgs` — it's in `crates/cli/src/lib.rs`. If the struct has `#[derive(Args)]` already, the flatten works. If it's built differently, match whatever pattern existing flags use.)

- [ ] **Step 5: Unit-test `ProofsHistoryArgs::validate`**

Add at the bottom of `crates/cli/src/args/proofs_history.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_always_valid() {
        let args = ProofsHistoryArgs {
            enabled: false,
            storage_path: None,
            window: 0,
            prune_interval: Duration::from_secs(15),
            prune_batch_size: 0,
            verification_interval: 0,
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn enabled_requires_storage_path() {
        let args = ProofsHistoryArgs {
            enabled: true,
            storage_path: None,
            window: 1,
            prune_interval: Duration::from_secs(15),
            prune_batch_size: 10_000,
            verification_interval: 0,
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("storage-path"));
    }

    #[test]
    fn enabled_rejects_zero_window() {
        let args = ProofsHistoryArgs {
            enabled: true,
            storage_path: Some("/tmp/p".into()),
            window: 0,
            prune_interval: Duration::from_secs(15),
            prune_batch_size: 10_000,
            verification_interval: 0,
        };
        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("window"));
    }
}
```

- [ ] **Step 6: Run + commit**

```bash
cargo nextest run -p alethia-reth-cli
just fmt && just clippy -p alethia-reth-cli
```
Expected: validate tests pass; the overall CLI still compiles.

```bash
git add crates/cli/Cargo.toml crates/cli/src/
git commit -m "feat(cli): add ProofsHistoryArgs flag set"
```

(If adding `humantime` to the workspace deps, include root `Cargo.toml` in the commit.)

---

## Task 15: Add `proofs init|prune|unwind` subcommands

**Files:**
- Create: `crates/cli/src/commands/proofs/mod.rs`
- Create: `crates/cli/src/commands/proofs/init.rs`
- Create: `crates/cli/src/commands/proofs/prune.rs`
- Create: `crates/cli/src/commands/proofs/unwind.rs`
- Modify: `crates/cli/src/command.rs` — register `Proofs(ProofsCommand)` variant

**Reference source:** op-reth's `crates/cli/src/commands/op_proofs/{mod,init,prune,unwind}.rs`

**Per-file adaptations:**
- Chain-spec parameter: op-reth's commands are parameterized over `OpChainSpec`; we parameterize over `TaikoChainSpec` (follow how existing alethia-reth CLI commands do it — check `crates/cli/src/command.rs`).
- Datadir parsing: reuse `reth-cli-commands::common::EnvironmentArgs`-style helpers that existing alethia-reth commands use.

- [ ] **Step 1: Create `commands/proofs/mod.rs`**

```rust
//! `alethia-reth proofs …` subcommands.

pub mod init;
pub mod prune;
pub mod unwind;

use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum ProofsCommand {
    /// Seed the sidecar from the main reth DB's current state.
    Init(init::InitCommand),
    /// Prune blocks outside the retention window.
    Prune(prune::PruneCommand),
    /// Unwind sidecar state back to a target block.
    Unwind(unwind::UnwindCommand),
}

impl ProofsCommand {
    pub async fn execute<C>(self) -> eyre::Result<()>
    where
        C: alethia_reth_chainspec::TaikoChainSpecProvider,
    {
        match self {
            Self::Init(cmd) => cmd.execute::<C>().await,
            Self::Prune(cmd) => cmd.execute::<C>().await,
            Self::Unwind(cmd) => cmd.execute::<C>().await,
        }
    }
}
```

(The `TaikoChainSpecProvider` trait name here is a placeholder — use whatever existing CLI plumbing alethia-reth has. Cross-reference `crates/cli/src/command.rs` for the exact pattern.)

- [ ] **Step 2: Port `init.rs`**

Adapt op-reth's `commands/op_proofs/init.rs`. The command opens the main reth DB read-only, opens (or creates) the sidecar MDBX via `MdbxProofsStorage`, calls `Initializer::init(&main_db, &mut sidecar_init_provider)`.

- [ ] **Step 3: Port `prune.rs`**

Adapt op-reth's `prune.rs`. Opens sidecar RW, runs `ProofsStoragePrunerTask::prune_batch(&mut provider_rw, target_earliest, batch_size)` in a loop until no more blocks above the window.

- [ ] **Step 4: Port `unwind.rs`**

Adapt op-reth's `unwind.rs`. Opens sidecar RW, calls `provider_rw.replace_updates(target_block, vec![])` (or whichever primitive op-reth uses) to delete all rows with block > target.

- [ ] **Step 5: Register the subcommand**

Modify `crates/cli/src/command.rs`:
- Add `Proofs(commands::proofs::ProofsCommand)` variant to the `TaikoCommand` enum
- Add a `Proofs(cmd) => cmd.execute::<C>().await,` arm to the dispatch match

Add `pub mod proofs;` in `crates/cli/src/commands/mod.rs` (create if it doesn't already exist — check).

- [ ] **Step 6: Smoke-test each subcommand**

```bash
cargo run -p alethia-reth -- proofs --help
cargo run -p alethia-reth -- proofs init --help
cargo run -p alethia-reth -- proofs prune --help
cargo run -p alethia-reth -- proofs unwind --help
```
Expected: help text prints for each without errors.

- [ ] **Step 7: Run + commit**

```bash
just fmt && just clippy && cargo nextest run --workspace
```
Expected: everything still green.

```bash
git add crates/cli/src/
git commit -m "feat(cli): add proofs init|prune|unwind subcommands"
```

---

## Task 16: RPC — `ProofsStateProviderFactory` + `ProofsStateProvider`

**Files:**
- Create: `crates/rpc/src/proofs/mod.rs`
- Create: `crates/rpc/src/proofs/state_factory.rs`
- Create: `crates/rpc/src/proofs/state_provider.rs`
- Modify: `crates/rpc/src/lib.rs` — re-export
- Modify: `crates/rpc/Cargo.toml` — add `alethia-reth-proofs-trie` dep

- [ ] **Step 1: Add dep + module declaration**

Modify `crates/rpc/Cargo.toml`: add under `[dependencies]`:
```toml
alethia-reth-proofs-trie = { path = "../proofs-trie" }
```

Modify `crates/rpc/src/lib.rs`:
```rust
pub mod proofs;
```

Write `crates/rpc/src/proofs/mod.rs`:
```rust
//! RPC overrides backed by the proofs-history sidecar.

pub mod state_factory;
pub mod state_provider;

pub use state_factory::ProofsStateProviderFactory;
pub use state_provider::RoutingStateProvider;
```

- [ ] **Step 2: Port `ProofsStateProviderFactory`**

Reference: op-reth's `crates/rpc/src/state.rs`.

Write `crates/rpc/src/proofs/state_factory.rs` implementing a factory that, given a `BlockId`, returns a `RoutingStateProvider` wrapping either the sidecar's `ProofsStateProvider` (if historical and in-window) or the underlying `Eth::state_provider(...)` (otherwise).

Key logic to adapt (not direct port — op-reth's factory is OP-bounded; we use Taiko's trait bounds instead):

```rust
use alethia_reth_proofs_trie::{MdbxProofsStorage, ProofsStateProvider, ProofsStore};
use alloy_eips::BlockId;
use reth_rpc_eth_api::EthApi;
use std::sync::Arc;

/// Routes state-provider requests: historical in-window → sidecar; otherwise → native.
pub struct ProofsStateProviderFactory<Eth, Storage> {
    eth_api: Eth,
    storage: Arc<Storage>,
}

impl<Eth, Storage> ProofsStateProviderFactory<Eth, Storage>
where
    Eth: /* Taiko EthApi bounds — copy from existing rpc module */,
    Storage: ProofsStore + Clone + 'static,
{
    pub const fn new(eth_api: Eth, storage: Arc<Storage>) -> Self {
        Self { eth_api, storage }
    }

    /// Body ported from op-reth `crates/rpc/src/state.rs`. Algorithm:
    ///   1. Resolve `block` to a block number via `self.eth_api.block_number(block)`.
    ///   2. Read the sidecar's `earliest`/`latest` pointers via `self.storage.provider_ro()`.
    ///   3. If `earliest <= N <= latest`: return `Box::new(ProofsStateProvider::new(ro, N))`.
    ///   4. Otherwise: delegate to `self.eth_api.state_provider(block)`.
    /// Keep the Taiko-specific trait bounds — do NOT import `reth-optimism-*` types.
    pub async fn state_provider(
        &self,
        block: BlockId,
    ) -> Result<Box<dyn reth_storage_api::StateProvider>, /* see step 2 for error type */> {
        /* ported body goes here — copy from op-reth `state.rs:20-…`, adapt bounds */
    }
}

impl<Eth, Storage> Clone for ProofsStateProviderFactory<Eth, Storage>
where Eth: Clone,
{
    fn clone(&self) -> Self {
        Self { eth_api: self.eth_api.clone(), storage: Arc::clone(&self.storage) }
    }
}
```

Replace the `todo!` with the actual routing logic. Source: op-reth's `crates/rpc/src/state.rs:20-…`.

- [ ] **Step 3: Port `state_provider.rs` (routing wrapper)**

If op-reth uses a dedicated routing `StateProvider` wrapper, port it here. If the factory just returns `Box<dyn StateProvider>` directly (which is likely — inspect the op-reth source), keep `state_provider.rs` minimal as a type alias / re-export. Don't invent a wrapper that doesn't need to exist.

- [ ] **Step 4: Unit-test the routing decision**

Write the smallest possible test: mock an `Eth` + a sidecar `ProofsStore`, pre-populate the sidecar with earliest=100, latest=200, call `state_provider(BlockId::Number(150))` → assert it hit the sidecar path (not the native delegate).

- [ ] **Step 5: Run + commit**

```bash
cargo nextest run -p alethia-reth-rpc
just fmt && just clippy -p alethia-reth-rpc
```

```bash
git add crates/rpc/Cargo.toml crates/rpc/src/
git commit -m "feat(rpc): add ProofsStateProviderFactory routing between sidecar and native"
```

---

## Task 17: RPC — `debug_executionWitness` override

**Files:**
- Create: `crates/rpc/src/proofs/debug.rs`
- Modify: `crates/rpc/src/proofs/mod.rs`

**Reference source:** op-reth's `crates/rpc/src/debug.rs:253-316` (the `execution_witness` function). The algorithm is reth-native; the adaptation is the state-provider source and the Taiko trait bounds.

- [ ] **Step 1: Define the override trait**

Write `crates/rpc/src/proofs/debug.rs`:

```rust
//! `debug_executionWitness` override routed through the proofs sidecar.

use alloy_eips::BlockNumberOrTag;
use alloy_rpc_types_debug::ExecutionWitness;
use jsonrpsee_core::{RpcResult, async_trait};
use jsonrpsee_proc_macros::rpc;

use crate::proofs::ProofsStateProviderFactory;

#[cfg_attr(not(test), rpc(server, namespace = "debug"))]
#[cfg_attr(test, rpc(server, client, namespace = "debug"))]
pub trait DebugApiProofsOverride {
    #[method(name = "executionWitness")]
    async fn execution_witness(&self, block: BlockNumberOrTag) -> RpcResult<ExecutionWitness>;
}

pub struct ProofsDebugApi<Eth, Storage, Provider, EvmConfig> {
    factory: ProofsStateProviderFactory<Eth, Storage>,
    provider: Provider,
    eth_api: Eth,
    evm_config: EvmConfig,
    // Concurrency limit for CPU-heavy re-execution.
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
}

impl<Eth, Storage, Provider, EvmConfig> ProofsDebugApi<Eth, Storage, Provider, EvmConfig> {
    pub fn new(
        provider: Provider,
        eth_api: Eth,
        factory: ProofsStateProviderFactory<Eth, Storage>,
        evm_config: EvmConfig,
    ) -> Self {
        Self {
            factory,
            provider,
            eth_api,
            evm_config,
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(3)),
        }
    }
}

// impl DebugApiProofsOverrideServer for ProofsDebugApi { ... }  // Step 2
```

- [ ] **Step 2: Implement the handler**

Port the body of op-reth's `execution_witness` into the `impl DebugApiProofsOverrideServer for ProofsDebugApi` block. Replace the OP trait bounds with the Taiko equivalents (check what `crates/rpc/src/eth/eth.rs` uses for `TaikoExt` and mirror). The critical substitution is using `self.factory.state_provider(...)` instead of op-reth's `OpStateProviderFactory`.

If there is an out-of-window block case, return the explicit error per spec:
```rust
Err(internal_rpc_err(format!(
    "block {N} is before proofs-history earliest block {M}; historical proofs not available"
)))
```

- [ ] **Step 3: Export from `proofs/mod.rs`**

Add:
```rust
pub mod debug;
pub use debug::{DebugApiProofsOverrideServer, ProofsDebugApi};
```

- [ ] **Step 4: Run + commit**

```bash
cargo check -p alethia-reth-rpc
just fmt && just clippy -p alethia-reth-rpc
```

```bash
git add crates/rpc/src/
git commit -m "feat(rpc): add debug_executionWitness override routed through sidecar"
```

---

## Task 18: RPC — `eth_getProof` override

**Files:**
- Create: `crates/rpc/src/proofs/eth.rs`
- Modify: `crates/rpc/src/proofs/mod.rs`

**Reference source:** op-reth's `crates/rpc/src/eth/proofs.rs`

- [ ] **Step 1: Define + implement the override**

Write `crates/rpc/src/proofs/eth.rs`:

```rust
//! `eth_getProof` override routed through the proofs sidecar.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use jsonrpsee_core::{RpcResult, async_trait};
use jsonrpsee_proc_macros::rpc;

use crate::proofs::ProofsStateProviderFactory;

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiProofsOverride {
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        address: Address,
        storage_keys: Vec<B256>,
        block: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse>;
}

pub struct ProofsEthApi<Eth, Storage> {
    factory: ProofsStateProviderFactory<Eth, Storage>,
    eth_api: Eth,
}

impl<Eth, Storage> ProofsEthApi<Eth, Storage> {
    pub const fn new(eth_api: Eth, factory: ProofsStateProviderFactory<Eth, Storage>) -> Self {
        Self { factory, eth_api }
    }
}

// impl EthApiProofsOverrideServer for ProofsEthApi { ... }
//   - resolve block to BlockId (None → latest)
//   - factory.state_provider(block).proof(Default::default(), address, &storage_keys)
//   - convert to EIP1186AccountProofResponse
```

Port the handler body from op-reth's `eth/proofs.rs` — same algorithm, Taiko bounds.

- [ ] **Step 2: Export from `proofs/mod.rs`**

```rust
pub mod eth;
pub use eth::{EthApiProofsOverrideServer, ProofsEthApi};
```

- [ ] **Step 3: Run + commit**

```bash
cargo check -p alethia-reth-rpc
just fmt && just clippy -p alethia-reth-rpc
```

```bash
git add crates/rpc/src/
git commit -m "feat(rpc): add eth_getProof override routed through sidecar"
```

---

## Task 19: Binary-level `install_proofs_history` helper

**Files:**
- Create: `bin/alethia-reth/src/proofs_history.rs`
- Modify: `bin/alethia-reth/Cargo.toml` — add new crate deps

- [ ] **Step 1: Add bin deps**

Modify `bin/alethia-reth/Cargo.toml` — add to `[dependencies]`:
```toml
alethia-reth-proofs-exex = { path = "../../crates/proofs-exex" }
alethia-reth-proofs-trie = { path = "../../crates/proofs-trie" }
```

(Also add `tokio` with `time` feature, `futures-util`, and `eyre` if not already present — check first.)

- [ ] **Step 2: Write the helper**

Write `bin/alethia-reth/src/proofs_history.rs`:

```rust
//! Install the historical-proofs sidecar into the node builder when --proofs-history is on.

use std::sync::Arc;

use alethia_reth_cli::args::ProofsHistoryArgs;
use alethia_reth_proofs_exex::ProofsExEx;
use alethia_reth_proofs_trie::MdbxProofsStorage;
use alethia_reth_rpc::proofs::{
    DebugApiProofsOverrideServer, EthApiProofsOverrideServer, ProofsDebugApi, ProofsEthApi,
    ProofsStateProviderFactory,
};
use eyre::WrapErr;
use futures_util::FutureExt;
use tracing::info;

/// Install the proofs-history ExEx + RPC overrides into the NodeBuilder chain.
/// If `args.enabled` is false, returns `builder` unchanged.
pub async fn install_proofs_history<B>(
    builder: B,
    args: &ProofsHistoryArgs,
) -> eyre::Result<B>
where
    B: /* the in-progress NodeBuilder type — see main.rs for the concrete type */,
{
    if !args.enabled {
        return Ok(builder);
    }

    args.validate()?;
    let path = args.storage_path.as_ref().expect("validated above");
    let storage = Arc::new(
        MdbxProofsStorage::new(path)
            .wrap_err_with(|| format!("failed to open proofs-history sidecar at {path:?}"))?,
    );

    info!(
        target: "alethia::proofs-history",
        window = args.window,
        ?path,
        "proofs-history sidecar enabled; historical eth_getProof window controlled by \
         --proofs-history.window, superseding --rpc.eth-proof-window"
    );

    let exex_storage = Arc::clone(&storage);
    let exex_window = args.window;
    let exex_prune_interval = args.prune_interval;
    let exex_verification_interval = args.verification_interval;

    let rpc_storage = Arc::clone(&storage);
    let rpc_window = args.window;

    Ok(builder
        .install_exex("proofs-history", move |exex_context| {
            let storage = Arc::clone(&exex_storage);
            async move {
                Ok(ProofsExEx::builder(exex_context, storage)
                    .with_proofs_history_window(exex_window)
                    .with_proofs_history_prune_interval(exex_prune_interval)
                    .with_verification_interval(exex_verification_interval)
                    .build()
                    .run()
                    .boxed())
            }
        })
        .extend_rpc_modules(move |ctx| {
            let factory = ProofsStateProviderFactory::new(
                ctx.registry.eth_api().clone(),
                Arc::clone(&rpc_storage),
            );
            let debug_ext = ProofsDebugApi::new(
                ctx.node().provider().clone(),
                ctx.registry.eth_api().clone(),
                factory.clone(),
                ctx.node().evm_config().clone(),
            );
            let eth_ext = ProofsEthApi::new(ctx.registry.eth_api().clone(), factory);

            ctx.modules.replace_configured(debug_ext.into_rpc())?;
            ctx.modules.replace_configured(eth_ext.into_rpc())?;
            info!(
                target: "alethia::proofs-history",
                "proofs-history RPC overrides installed (debug_executionWitness, eth_getProof)"
            );
            Ok(())
        }))
}
```

The concrete `B` bound — match whatever NodeBuilder type `.node(TaikoNode)` returns in `main.rs`. If the bound is too complex to write, use an inherent impl on a newtype wrapper or make this a private function in `main.rs` rather than a generic helper.

- [ ] **Step 3: Run + commit**

```bash
cargo check -p alethia-reth
just fmt && just clippy -p alethia-reth
```
Expected: compiles.

```bash
git add bin/alethia-reth/Cargo.toml bin/alethia-reth/src/
git commit -m "feat(bin): add install_proofs_history builder helper"
```

---

## Task 20: Wire helper into `main.rs`

**Files:**
- Modify: `bin/alethia-reth/src/main.rs`

- [ ] **Step 1: Add module declaration + call**

Modify `bin/alethia-reth/src/main.rs`:
- Add `mod proofs_history;` near the top
- Extract the `ext_args` from the `TaikoCli::run` callback closure signature (it's currently `_ext_args`)
- Insert the helper call between `.node(TaikoNode)` and the existing `.extend_rpc_modules`:

```rust
if let Err(err) = TaikoCli::<TaikoChainSpecParser, TaikoCliExtArgs>::parse_args().run(
    async move |builder, ext_args| {  // rename _ext_args → ext_args
        info!(target: "reth::taiko::cli", "Launching Taiko node");
        let builder = builder.node(TaikoNode);
        let builder = crate::proofs_history::install_proofs_history(
            builder,
            &ext_args.proofs_history,
        )
        .await?;

        let handle = builder
            .extend_rpc_modules(move |ctx| {
                // ... existing taiko_ + taikoAuth_ RPC extensions, unchanged ...
            })
            .launch_with_debug_capabilities()
            .await?;

        handle.wait_for_node_exit().await
    },
) { … }
```

- [ ] **Step 2: Build + smoke test**

```bash
cargo build --release -p alethia-reth
./target/release/alethia-reth node --help | grep proofs-history
```
Expected: the new `--proofs-history*` flags appear in help output.

- [ ] **Step 3: Commit**

```bash
git add bin/alethia-reth/src/main.rs
git commit -m "feat(bin): wire proofs-history sidecar into node launcher"
```

---

## Task 21: End-to-end integration test

**Files:**
- Create: `crates/proofs-exex/tests/e2e.rs`

- [ ] **Step 1: Write the e2e test**

Write `crates/proofs-exex/tests/e2e.rs`:

```rust
//! End-to-end: drive a synthetic chain through the ExEx + verify sidecar can serve proofs.

use alethia_reth_proofs_exex::ProofsExEx;
use alethia_reth_proofs_trie::{MdbxProofsStorage, ProofsStateProvider};
use reth_exex_test_utils::test_exex_context;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread")]
async fn sidecar_serves_historical_proof_after_exex_ingest() {
    let sidecar_dir = TempDir::new().unwrap();
    let storage = Arc::new(MdbxProofsStorage::new(sidecar_dir.path()).unwrap());

    // 1. Seed sidecar with a block-0 baseline (simulating proofs init).
    {
        let provider = storage.initialization_provider().unwrap();
        // Write a minimal state: one account with one storage slot at block 0.
        // ... (use helpers from alethia-reth-proofs-trie test utils or construct inline)
        provider.commit().unwrap();
    }

    // 2. Drive the ExEx with one simulated canonical commit at block 1 that mutates the slot.
    let (ctx, handle) = test_exex_context().await.unwrap();
    let exex = ProofsExEx::builder(ctx, Arc::clone(&storage))
        .with_proofs_history_window(10_000)
        .build();

    let exex_task = tokio::spawn(async move { exex.run().await.unwrap() });

    handle.send_notification_chain_committed(/* block 1 with the slot update */).await.unwrap();
    handle.wait_for_finished_height(1).await.unwrap();

    // 3. Assert we can read the slot at block 0 (baseline) AND block 1 (post-update).
    let provider_0 = ProofsStateProvider::new(storage.provider_ro().unwrap(), 0);
    let provider_1 = ProofsStateProvider::new(storage.provider_ro().unwrap(), 1);

    let slot_at_0 = provider_0.storage(/* account */, /* slot */).unwrap();
    let slot_at_1 = provider_1.storage(/* account */, /* slot */).unwrap();

    assert_ne!(slot_at_0, slot_at_1, "sidecar must surface different values per block version");

    exex_task.abort();
}
```

Fill in the specific account/slot construction using helpers from `reth-trie`'s test utilities — match whatever pattern op-reth's e2e test in `crates/exex/src/lib.rs` tests uses. If the op-reth test already has this scaffolding, port that test directly rather than reinventing.

- [ ] **Step 2: Run + commit**

```bash
cargo nextest run -p alethia-reth-proofs-exex --test e2e
just fmt && just clippy -p alethia-reth-proofs-exex
```
Expected: the e2e test passes.

```bash
git add crates/proofs-exex/tests/
git commit -m "test(proofs-exex): end-to-end sidecar ingestion + query"
```

---

## Task 22: Full workspace verification + PR prep

**Files:** none (verification only)

- [ ] **Step 1: Full workspace build + test**

```bash
just fmt
just clippy
just test
```
Expected: all green.

- [ ] **Step 2: Binary smoke test**

```bash
cargo build --release -p alethia-reth
./target/release/alethia-reth --help | grep -c proofs
```
Expected: at least 1 line (the `proofs` subcommand in the top-level help).

- [ ] **Step 3: Check size budget**

```bash
ls -la target/release/alethia-reth
```
Expected: reasonable size vs. baseline (sidecar code adds maybe 1-2 MB to the binary).

- [ ] **Step 4: Review diff stat**

```bash
git log --oneline main..HEAD
git diff --stat main..HEAD
```
Expected: ~20 commits, ~3-4k lines added across `crates/proofs-trie/`, `crates/proofs-exex/`, `crates/cli/`, `crates/rpc/`, `bin/alethia-reth/`, plus `Cargo.toml` + `Cargo.lock`.

- [ ] **Step 5: Open the PR**

```bash
git push -u origin feat/proofs-history-sidecar
gh pr create \
  --title "feat: historical proofs sidecar (debug_executionWitness + eth_getProof)" \
  --body "$(cat <<'EOF'
## Summary
- Adds bounded-history sidecar storing versioned trie data in a separate MDBX env
- Populated live via new proofs-exex ExEx; bounded window default 259_200 blocks (72h at 1s block time)
- Overrides `debug_executionWitness` and `eth_getProof` to sub-second for in-window historical blocks
- New CLI subcommands: `alethia-reth proofs init|prune|unwind`
- Off by default — set `--proofs-history` to opt in

Design doc: `docs/superpowers/specs/2026-04-23-historical-proofs-sidecar-design.md`

## Test plan
- [ ] `just test` green on CI
- [ ] Run `alethia-reth proofs init` on a hoodi staging node; confirm sidecar baseline at block 0
- [ ] Restart node with `--proofs-history` enabled; confirm ExEx catches up within a few minutes
- [ ] Benchmark `debug_executionWitness` for an in-window historical block: verify p50 < 500ms
- [ ] Benchmark `eth_getProof` for an in-window historical block: verify p50 < 20ms
- [ ] Verify rollback path: restart without `--proofs-history`; node behaves as before the PR

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-review

Ran against the spec:

- **Section 1 (architecture):** Tasks 1–22 collectively implement the crate layout in the spec. ✓
- **Section 2 (storage):** Tasks 2–10 port the full storage layer including the spec-required `SchemaVersion` table. ✓
- **Section 3 (ExEx):** Tasks 12–13 port the builder, ensure_initialized, run loop, reorg handling, FinishedHeight invariant. ✓
- **Section 4 (CLI):** Tasks 14–15 add `ProofsHistoryArgs` and the three subcommands. ✓
- **Section 5 (RPC):** Tasks 16–18 add the state factory + two overrides. The "out-of-window explicit error" requirement is noted in Task 17. ✓
- **Section 6 (node wiring):** Tasks 19–20 add `install_proofs_history` and wire it into main.rs. ✓
- **Section 7 (configuration):** Task 14 implements all flags with correct defaults (window = 259_200). ✓
- **Section 8 (operational concerns):** The plan doesn't include a task for Grafana dashboards — that's tracked as follow-up work (noted in Task 22 PR description if desired). Metrics themselves are wired via Task 11 (metrics.rs) + the ExEx's existing metric calls.

**Gap flagged during review:** the spec's staged rollout (hoodi canary → broad mainnet) is ops work not code work. No task needed.

**Placeholder scan:** Task 16 Step 2's function body is marked `/* ported body goes here — copy from op-reth ... */` rather than inlined. This is deliberate — the body is a direct port and reproducing it here would double-duplicate the op-reth source without adding information. The exact file + line range is named. All "TODO/TBD/later" phrases absent elsewhere.

**Type consistency:** `ProofsStore`, `ProofsProviderRO`, `MdbxProofsStorage`, `ProofsExEx`, `ProofsStateProviderFactory`, `ProofsDebugApi`, `ProofsEthApi` — names consistent across tasks 6, 12, 16, 17, 18, 19.

---

## Risks / things to watch during execution

1. **Task 0 (spike) may surface reth v2.0 → v2.1 breaking changes.** If it does, update Tasks 2–13 with the per-file adaptations before executing them. Do not proceed blind.
2. **Task 16 Step 2's trait bounds** — the exact bounds for `Eth` in `ProofsStateProviderFactory` require inspection of how existing alethia-reth RPC code parameterizes eth_api. Budget extra time here.
3. **Task 19 Step 2's builder type** — reth's NodeBuilder generics are deep. If writing a fully-generic `install_proofs_history` proves painful, inline the logic into `main.rs` as a private function instead.
4. **op-reth README says "Under Construction."** The schema we're porting could change upstream. Our `SchemaVersion` table gives us a clean "bump-on-open-or-refuse" path if that happens.
