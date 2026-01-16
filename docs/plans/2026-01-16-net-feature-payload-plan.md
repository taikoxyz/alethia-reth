# Net Feature Payload Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Gate payload/engine types behind a single `net` feature in `alethia-reth-primitives` and wire `alethia-reth-block` to enable it.

**Architecture:** Add a `net` feature that owns net-only dependencies and compile-time gates `engine` and `payload`. Keep defaults unchanged by enabling `net` by default.

**Tech Stack:** Rust 2024, Cargo features, workspace crates.

### Task 1: Add `net` feature and optionalize net-only deps in primitives

**Files:**
- Modify: `crates/primitives/Cargo.toml`

**Step 1: Write the failing check**

Run: `cargo check -p alethia-reth-primitives --no-default-features`

Expected: FAIL because net-only deps are still required.

**Step 2: Update features and dependencies**

Edit `crates/primitives/Cargo.toml`:
```toml
[features]
default = ["serde", "net"]
serde = [
    "dep:serde",
    "alloy-primitives/serde",
    "alloy-rpc-types-engine/serde",
    "alloy-rpc-types-eth/serde",
]
net = [
    "dep:alloy-primitives",
    "dep:alloy-rlp",
    "dep:alloy-rpc-types-engine",
    "dep:alloy-rpc-types-eth",
    "dep:alloy-serde",
    "dep:reth-chainspec",
    "dep:reth-engine-local",
    "dep:reth-ethereum",
    "dep:reth-ethereum-engine-primitives",
    "dep:reth-node-api",
    "dep:reth-payload-primitives",
    "dep:reth-primitives",
    "dep:reth-primitives-traits",
    "dep:serde_with",
    "dep:sha2",
    "dep:tracing",
]
```

Mark net-only deps optional:
```toml
alloy-primitives = { workspace = true, optional = true }
alloy-rlp = { workspace = true, optional = true }
alloy-rpc-types-engine = { workspace = true, optional = true }
alloy-rpc-types-eth = { workspace = true, optional = true }
alloy-serde = { workspace = true, optional = true }
reth-chainspec = { workspace = true, optional = true }
reth-engine-local = { workspace = true, optional = true }
reth-ethereum = { workspace = true, optional = true }
reth-ethereum-engine-primitives = { workspace = true, optional = true }
reth-node-api = { workspace = true, optional = true }
reth-payload-primitives = { workspace = true, optional = true }
reth-primitives = { workspace = true, optional = true }
reth-primitives-traits = { workspace = true, optional = true }
serde_with = { workspace = true, optional = true }
sha2 = { workspace = true, optional = true }
tracing = { workspace = true, optional = true }
```

**Step 3: Run the check to verify it passes**

Run: `cargo check -p alethia-reth-primitives --no-default-features`

Expected: PASS.

**Step 4: Commit**

```bash
git add crates/primitives/Cargo.toml
git commit -m "feat(primitives): add net feature gating"
```

### Task 2: Gate primitives modules behind `net`

**Files:**
- Modify: `crates/primitives/src/lib.rs`

**Step 1: Write the failing check**

Run: `cargo check -p alethia-reth-primitives --no-default-features`

Expected: FAIL because payload/engine modules are still compiled without `net`.

**Step 2: Gate modules**

Edit `crates/primitives/src/lib.rs`:
```rust
#[cfg(feature = "net")]
pub mod engine;
#[cfg(feature = "net")]
pub mod payload;
```

Keep `extra_data` always available.

**Step 3: Run tests**

Run: `cargo test -p alethia-reth-primitives`

Expected: PASS.

**Step 4: Commit**

```bash
git add crates/primitives/src/lib.rs
git commit -m "feat(primitives): gate payload and engine with net"
```

### Task 3: Wire block `net` feature to primitives `net`

**Files:**
- Modify: `crates/block/Cargo.toml`

**Step 1: Update feature wiring**

Edit `crates/block/Cargo.toml`:
```toml
net = [
    "dep:alethia-reth-primitives",
    "alethia-reth-primitives/net",
    "dep:reth-payload-primitives",
    "dep:reth-rpc-eth-api",
]
```

**Step 2: Run checks**

Run: `cargo check -p alethia-reth-block --no-default-features`
Expected: PASS.

Run: `cargo check -p alethia-reth-block --features net`
Expected: PASS.

**Step 3: Commit**

```bash
git add crates/block/Cargo.toml
git commit -m "feat(block): enable primitives net feature"
```

### Task 4: Final verification

**Step 1: Run workspace build**

Run: `cargo build`

Expected: PASS.

**Step 2: Optional targeted tests**

Run: `cargo test -p alethia-reth-primitives`
Expected: PASS.

**Step 3: Commit plan/design docs if needed**

```bash
git add docs/plans/2026-01-16-net-feature-payload-design.md docs/plans/2026-01-16-net-feature-payload-plan.md
git commit -m "docs: add net feature payload plan"
```
