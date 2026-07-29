# Beacon Root Override Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve `eth_simulateV1`'s `blockOverrides.beaconRoot` in Unzen next-block execution while retaining the existing zero-root fallback and PR #227's JIT support.

**Architecture:** Extend `TaikoNextBlockEnvAttributes` with an optional parent beacon block root and propagate it through every constructor. Keep fork behavior centralized in `normalize_parent_beacon_block_root`, so pre-Unzen execution discards the value while Unzen preserves an override or falls back to `B256::ZERO`.

**Tech Stack:** Rust 1.95, Alloy RPC types, reth `ConfigureEvm`/`BuildPendingEnv`, Cargo nextest, rustfmt, clippy.

## Global Constraints

- Do not add or restore #228's `re-execute --jit` rejection; PR #227 intentionally supports that command.
- Preserve pre-Unzen `None`, Unzen override preservation, and Unzen zero-root fallback semantics.
- Update every `TaikoNextBlockEnvAttributes` constructor so the workspace remains type-correct.
- Every production Rust symbol and field added by this change must have a purpose/contract doc comment.
- Use a failing behavior test before modifying production code.

---

### Task 1: Propagate Beacon Root Overrides

**Files:**
- Modify: `crates/block/src/config.rs:280-297`
- Modify: `crates/block/src/config.rs:392-465`
- Test: `crates/block/src/config.rs:467-647`
- Modify: `crates/block/src/derived_block.rs:43-57`
- Modify: `crates/block/src/executor.rs:760-778`
- Modify: `crates/payload/src/builder/mod.rs:227-241`
- Modify: `crates/rpc/src/eth/auth/mod.rs:338-354`

**Interfaces:**
- Consumes: `BlockOverrides::beacon_root: Option<B256>`, `Header::parent_beacon_block_root: Option<B256>`, and the existing `normalize_parent_beacon_block_root(bool, Option<B256>) -> Option<B256>`.
- Produces: `TaikoNextBlockEnvAttributes::parent_beacon_block_root: Option<B256>` propagated into `TaikoBlockExecutionCtx::parent_beacon_block_root`.

- [ ] **Step 1: Add the focused failing regression test**

Move the existing `config_with_unzen_at` test helper from the nested `payload_ctx` module to the
outer test module:

```rust
fn config_with_unzen_at(timestamp: u64) -> TaikoEvmConfig {
    let mut chain_spec = (*TAIKO_DEVNET).as_ref().clone();
    chain_spec
        .inner
        .hardforks
        .insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(timestamp));
    TaikoEvmConfig::new(Arc::new(chain_spec))
}
```

Then add:

```rust
#[cfg(feature = "net")]
#[test]
fn pending_env_preserves_beacon_root_override_for_unzen_context() {
    let root = B256::repeat_byte(0x33);
    let parent = SealedHeader::seal_slow(Header {
        number: 1,
        timestamp: 1,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(1),
        ..Header::default()
    });
    let overrides =
        alloy_rpc_types_eth::BlockOverrides { beacon_root: Some(root), ..Default::default() };
    let attributes = TaikoNextBlockEnvAttributes::build_pending_env(&parent, Some(&overrides));
    let config = config_with_unzen_at(0);

    let ctx = config
        .context_for_next_block(&parent, attributes)
        .expect("pending Unzen context should build");

    assert_eq!(ctx.parent_beacon_block_root, Some(root));
}
```

- [ ] **Step 2: Run the regression test and verify RED**

Run:

```bash
cargo +1.95.0 test -p alethia-reth-block --all-features \
  pending_env_preserves_beacon_root_override_for_unzen_context -- --nocapture
```

Expected: the test compiles and fails because the current pending environment ignores
`BlockOverrides::beacon_root`, producing `Some(B256::ZERO)` instead of the supplied `0x33…33`
root.

- [ ] **Step 3: Add the minimal propagation implementation**

In `TaikoNextBlockEnvAttributes`, add:

```rust
/// Parent beacon block root used by the EIP-4788 system call.
pub parent_beacon_block_root: Option<B256>,
```

In `context_for_next_block`, replace the hard-coded `None` with:

```rust
parent_beacon_block_root: normalize_parent_beacon_block_root(
    is_unzen_active,
    ctx.parent_beacon_block_root,
),
```

In `BuildPendingEnv`, consume `block_overrides` and initialize:

```rust
parent_beacon_block_root: block_overrides.and_then(|overrides| overrides.beacon_root),
```

Update the remaining constructors with these exact sources:

```rust
// crates/block/src/derived_block.rs
parent_beacon_block_root: header.parent_beacon_block_root,

// crates/payload/src/builder/mod.rs
parent_beacon_block_root: attributes.parent_beacon_block_root,

// crates/block/src/executor.rs and crates/rpc/src/eth/auth/mod.rs
parent_beacon_block_root: None,
```

- [ ] **Step 4: Run the focused test and verify GREEN**

Run:

```bash
cargo +1.95.0 test -p alethia-reth-block --all-features \
  pending_env_preserves_beacon_root_override_for_unzen_context -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Run affected and workspace verification**

Run:

```bash
cargo +1.95.0 test -p alethia-reth-block --all-features
just fmt
just clippy
just test
git diff --check
```

Expected: every command exits zero with no test failures, formatting differences, clippy
warnings, missing-doc errors, or whitespace errors.

- [ ] **Step 6: Review the complete behavior diff**

Confirm:

```bash
git diff -- crates/block/src/config.rs crates/block/src/derived_block.rs \
  crates/block/src/executor.rs crates/payload/src/builder/mod.rs \
  crates/rpc/src/eth/auth/mod.rs
git diff -- crates/cli/src/lib.rs
```

Expected: the five beacon-root files contain only the planned propagation and regression test;
the CLI diff is empty.

- [ ] **Step 7: Commit the behavior change**

```bash
git add crates/block/src/config.rs crates/block/src/derived_block.rs \
  crates/block/src/executor.rs crates/payload/src/builder/mod.rs \
  crates/rpc/src/eth/auth/mod.rs
git commit -m "fix(evm): honor beacon root overrides"
```

Expected: one Conventional Commit containing the behavior fix and its regression test.

### Task 2: Independent Review and Publication

**Files:**
- Inspect: all files changed between PR #227's previous head and the behavior commit.
- Modify: only files required to resolve verified Critical or Important review findings.

**Interfaces:**
- Consumes: the committed beacon-root behavior from Task 1 and PR #227's existing source branch `feat/evm-revmc-jit`.
- Produces: an independently reviewed commit pushed to PR #227 without merging PR #228.

- [ ] **Step 1: Dispatch a clean-context code reviewer**

Give the reviewer the exact base SHA `b693f8ce97318a2911d2b2f9851466742c7e7e87`, the behavior commit SHA, the approved design, and the requirement that `crates/cli/src/lib.rs` remain unchanged.

- [ ] **Step 2: Resolve review findings**

Fix every verified Critical or Important finding using another red-green TDD cycle. Record Minor
findings as non-blocking unless they expose a correctness regression.

- [ ] **Step 3: Re-run verification after review changes**

Run:

```bash
just fmt
just clippy
just test
git diff --check b693f8ce97318a2911d2b2f9851466742c7e7e87..HEAD
git status --short --branch
```

Expected: all commands pass and the worktree is clean.

- [ ] **Step 4: Push to PR #227's source branch**

```bash
git push origin HEAD:feat/evm-revmc-jit
```

Expected: GitHub reports PR #227's head as the reviewed local `HEAD`.

- [ ] **Step 5: Verify the published PR**

Run:

```bash
gh pr view 227 --repo taikoxyz/alethia-reth \
  --json headRefOid,mergeable,isDraft,reviewDecision
gh pr checks 227 --repo taikoxyz/alethia-reth
```

Expected: `headRefOid` equals the pushed commit; checks are green or actively running with no
immediate configuration failure.
