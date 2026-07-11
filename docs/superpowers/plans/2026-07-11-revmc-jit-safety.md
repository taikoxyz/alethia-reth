# revmc JIT Safety Implementation Plan

> Execute this plan test-first in the PR #218 worktree. Do not push until every
> verification command and the final focused review pass.

**Goal:** Preserve opt-in JIT for canonical Engine execution while making revmc
execution equivalent to the interpreter, isolating all Ethereum RPC execution,
and guaranteeing non-blocking runtime controls.

**Architecture:** Vendor the compatible pinned revmc source and backport the two
upstream correctness/lifecycle fixes. Keep `TaikoEvmConfig` canonical and
JIT-capable, derive an interpreter-only copy for `EthApi`, and construct the RPC
API with that scoped copy while retaining Reth's complete configuration.

**Toolchain:** Rust 1.95, Cargo workspace, Reth `27bfdde`, revmc `4042c2e`, LLVM
22, `cargo nextest`, `just`.

---

## Task 1: Record the approved design and plan

**Files:**

- Add: `docs/superpowers/specs/2026-07-11-revmc-jit-safety-design.md`
- Add: `docs/superpowers/plans/2026-07-11-revmc-jit-safety.md`

1. Commit the approved design checkpoint.
2. Add this executable plan and verify `git diff --check`.
3. Commit the implementation plan separately.

## Task 2: Add red differential and routing regressions

**Files:**

- Modify: `crates/evm/src/zk_gas/tests.rs`
- Modify: `crates/block/src/config.rs`
- Modify: `crates/rpc/src/eth/builder.rs`

1. Replace `jit_pin_still_diverges_on_dynamic_gas_failure_order` with an equality
   assertion for the exact dynamic-gas bytecode from revmc #395.
2. Run the targeted EVM test and confirm it fails on the pinned compiler.
3. Add factory-introspection helpers or tests proving:
   - the canonical direct and executor factories are JIT-capable when configured;
   - an RPC-scoped copy disables JIT for both direct execution and the next-block
     builder while retaining the same chain specification/backend handle.
4. Run the targeted block/RPC tests and confirm the current mixed routing fails.

## Task 3: Add red runtime-control regressions

**Files:**

- Modify: `crates/evm/src/jit.rs`
- Later modify vendored revmc runtime tests/source.

1. Add tests for pause, resume, and clear before worker startup.
2. Add a saturated bounded-control-queue test that requires prompt typed errors.
3. Add a startup-state regression showing controls cannot observe an enabled but
   unavailable worker.
4. Run the targeted tests and confirm at least the queue/startup cases fail or
   cannot compile against the pinned API.

## Task 4: Vendor and patch the compatible revmc revision

**Files:**

- Add: `vendor/revmc/**`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

1. Import the unmodified `4042c2e` source mechanically, excluding `.git`, CI,
   fuzz corpora, build artifacts, and unrelated examples where Cargo permits.
2. Preserve `LICENSE-APACHE`, `LICENSE-MIT`, upstream README, and add a provenance
   note identifying the base revision and backported PRs.
3. Point the workspace `revmc` dependency to `vendor/revmc/crates/revmc` with the
   existing feature set.
4. Backport revmc #395's stack-section boundary and exact-error configuration.
5. Backport revmc #391's started-worker tracking and non-blocking pause/resume;
   apply the same contract to clear-all.
6. Expose a fallible control API so Alethia maps disabled, not-started, full, and
   disconnected states into actionable RPC errors.
7. Re-run the red tests and confirm they pass.

## Task 5: Split canonical and RPC execution configurations

**Files:**

- Modify: `crates/block/src/config.rs`
- Modify: `crates/rpc/src/eth/builder.rs`
- Modify dependency manifests only if required by public builder types.

1. Restore `TaikoEvmConfig` so its stored direct and executor factories are both
   JIT-capable for canonical Engine/payload consumers.
2. Add a documented `for_rpc`/`rpc_only` constructor that clones the config with
   JIT disabled in every EVM factory.
3. Build `EthApi` from `ctx.components` using the RPC-only EVM config.
4. Copy every setting currently applied by `EthApiCtx::eth_api_builder` so there
   is no configuration regression.
5. Run targeted config and RPC builder tests, then `cargo check` for the affected
   crates.

## Task 6: Integrate the fallible control contract

**Files:**

- Modify: `crates/evm/src/jit.rs`
- Modify: `crates/rpc/src/eth/jit.rs` if error mapping needs adjustment.

1. Remove the enabled-only wrapper that can race worker startup.
2. Call the vendored backend's fallible, non-blocking controls directly.
3. Keep trait methods documented and preserve stable RPC error messages.
4. Run all JIT backend and JIT RPC tests.

## Task 7: Remove obsolete safety caveats

**Files:**

- Modify: `README.md`
- Modify startup warning locations found with `rg`.

1. Remove statements that the known revmc divergence is intentionally accepted.
2. Retain clear documentation that JIT is experimental and opt-in.
3. Verify no production or test text still expects interpreter/JIT divergence.

## Task 8: Full verification and focused review

1. Run `just fmt` and inspect the resulting diff.
2. Run `just clippy` (mandatory documentation gate).
3. Run targeted JIT/EVM/block/RPC/CLI tests.
4. Run `just test`.
5. Run no-default-features and relevant release/build checks used by CI.
6. Run `git diff --check`, inspect `git status`, and review the complete diff
   against `origin/main`.
7. Request a focused reviewer to check correctness, RPC isolation, blocking,
   documentation, and vendored scope. Fix and re-verify any valid finding.

## Task 9: Commit and push to the existing PR

1. Stage only intended files and review the staged diff.
2. Commit using Conventional Commits with a concise imperative summary.
3. Verify the worktree is clean and the commit is based on the latest PR head.
4. Push `feat/reth-jit-support` directly to `origin`.
5. Confirm the remote branch points at the new commit and report commands run.
