# Proof History Backfill Window Only Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `--proofs-history.backfill-window-only` so a fresh sync can skip proof-history storage before `finalized - window`.

**Architecture:** Extend the CLI/config with a boolean flag, then move proof-history initialization into the ExEx runtime when delayed mode is enabled. Empty proof storage waits for a finalized-derived start block, acknowledges skipped ExEx notifications, initializes at the first eligible canonical state, and then uses the existing proof-history sync/prune flow.

**Tech Stack:** Rust, clap, reth provider traits, reth ExEx notifications, OP proofs MDBX storage, cargo nextest.

---

## File Structure

- Modify `crates/cli/src/lib.rs`: parse `--proofs-history.backfill-window-only` and cover CLI defaults.
- Modify `crates/cli/src/command.rs`: pass the parsed flag into `ProofHistoryConfig`.
- Modify `crates/node/src/proof_history.rs`: add config fields, finalized-window helper logic, delayed initialization state, and proof-history ExEx acknowledgement behavior.
- Test in `crates/cli/src/lib.rs` and `crates/node/src/proof_history.rs`: keep tests beside existing proof-history tests.

### Task 1: CLI And Config Surface

**Files:**
- Modify: `crates/cli/src/lib.rs`
- Modify: `crates/cli/src/command.rs`
- Modify: `crates/node/src/proof_history.rs`
- Test: `crates/cli/src/lib.rs`
- Test: `crates/node/src/proof_history.rs`

- [ ] **Step 1: Add failing CLI assertions**

Update `test_parse_proof_history_flags` in `crates/cli/src/lib.rs` to include the new flag and assertion:

```rust
let cli = TestCli::try_parse_from([
    "alethia-reth",
    "--proofs-history",
    "--proofs-history.storage-path",
    "/tmp/proofs-history",
    "--proofs-history.window",
    "256",
    "--proofs-history.prune-interval",
    "30s",
    "--proofs-history.verification-interval",
    "16",
    "--proofs-history.backfill-window-only",
])
.expect("proof-history args should parse");

assert!(cli.ext.proof_history.backfill_window_only);
```

Update `test_default_proof_history_config_is_disabled`:

```rust
assert!(!config.backfill_window_only);
```

- [ ] **Step 2: Run the CLI test and verify it fails**

Run:

```bash
cargo test -p alethia-reth-cli proof_history
```

Expected: compilation fails because `backfill_window_only` is missing from `TaikoProofHistoryArgs` and `ProofHistoryConfig`.

- [ ] **Step 3: Add the CLI and config fields**

In `crates/cli/src/lib.rs`, add this field to `TaikoProofHistoryArgs` after `window`:

```rust
/// Delay empty proof-history initialization until the finalized window start is reached.
#[arg(
    long = "proofs-history.backfill-window-only",
    default_value_t = false,
    help_heading = "Taiko Proof History"
)]
pub backfill_window_only: bool,
```

In `crates/node/src/proof_history.rs`, add this field to `ProofHistoryConfig` after `window`:

```rust
/// Whether empty proof-history storage waits until the finalized retention window starts.
pub backfill_window_only: bool,
```

Update `ProofHistoryConfig::disabled()`:

```rust
backfill_window_only: false,
```

In `crates/cli/src/command.rs`, populate the new config field:

```rust
backfill_window_only: self.proof_history.backfill_window_only,
```

- [ ] **Step 4: Run the CLI test and verify it passes**

Run:

```bash
cargo test -p alethia-reth-cli proof_history
```

Expected: both tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/lib.rs crates/cli/src/command.rs crates/node/src/proof_history.rs
git commit -m "feat(proof-history): add backfill window flag"
```

### Task 2: Pure Start-Block Helpers

**Files:**
- Modify: `crates/node/src/proof_history.rs`
- Test: `crates/node/src/proof_history.rs`

- [ ] **Step 1: Add failing helper tests**

Add these tests to the existing `#[cfg(test)] mod tests` in `crates/node/src/proof_history.rs`:

```rust
#[test]
fn proof_history_window_start_saturates_at_genesis() {
    assert_eq!(proof_history_window_start_block(100, 350_000), 0);
}

#[test]
fn proof_history_window_start_subtracts_window() {
    assert_eq!(proof_history_window_start_block(1_000_000, 350_000), 650_000);
}

#[test]
fn delayed_proof_history_initialization_waits_without_finalized_head() {
    assert_eq!(
        delayed_proof_history_start(None, 900_000, 350_000),
        DelayedProofHistoryStart::WaitForFinalized
    );
}

#[test]
fn delayed_proof_history_initialization_waits_until_local_execution_reaches_start() {
    assert_eq!(
        delayed_proof_history_start(Some(1_000_000), 649_999, 350_000),
        DelayedProofHistoryStart::WaitForExecution { start_block: 650_000 }
    );
}

#[test]
fn delayed_proof_history_initialization_starts_when_execution_reaches_window_start() {
    assert_eq!(
        delayed_proof_history_start(Some(1_000_000), 650_000, 350_000),
        DelayedProofHistoryStart::Ready { start_block: 650_000 }
    );
}
```

- [ ] **Step 2: Run the node helper tests and verify they fail**

Run:

```bash
cargo test -p alethia-reth-node proof_history
```

Expected: compilation fails because `proof_history_window_start_block`, `delayed_proof_history_start`, and `DelayedProofHistoryStart` do not exist.

- [ ] **Step 3: Add helper types and functions**

Add the following near `proof_history_backfill_target` in `crates/node/src/proof_history.rs`:

```rust
/// Decision for delayed proof-history initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelayedProofHistoryStart {
    /// No finalized block has been observed yet.
    WaitForFinalized,
    /// Local execution has not reached the derived proof-history start block.
    WaitForExecution {
        /// First block where proof-history should initialize.
        start_block: u64,
    },
    /// Local execution has reached the derived proof-history start block.
    Ready {
        /// First block where proof-history should initialize.
        start_block: u64,
    },
}

/// Returns the first block retained by a finalized proof-history window.
const fn proof_history_window_start_block(finalized_block: u64, window: u64) -> u64 {
    finalized_block.saturating_sub(window)
}

/// Computes whether delayed proof-history initialization can start.
fn delayed_proof_history_start(
    finalized_block: Option<u64>,
    executed_head: u64,
    window: u64,
) -> DelayedProofHistoryStart {
    let Some(finalized_block) = finalized_block else {
        return DelayedProofHistoryStart::WaitForFinalized;
    };

    let start_block = proof_history_window_start_block(finalized_block, window);
    if executed_head < start_block {
        DelayedProofHistoryStart::WaitForExecution { start_block }
    } else {
        DelayedProofHistoryStart::Ready { start_block }
    }
}
```

- [ ] **Step 4: Run the node helper tests and verify they pass**

Run:

```bash
cargo test -p alethia-reth-node proof_history
```

Expected: all helper tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/node/src/proof_history.rs
git commit -m "feat(proof-history): derive finalized window start"
```

### Task 3: Runtime Delayed Initialization

**Files:**
- Modify: `crates/node/src/proof_history.rs`
- Test: `crates/node/src/proof_history.rs`

- [ ] **Step 1: Add a failing config test for the default field**

Extend `disabled_config_has_no_storage_path`:

```rust
assert!(!config.backfill_window_only);
```

Extend `enabled_config_requires_storage_path`:

```rust
let config = ProofHistoryConfig {
    enabled: true,
    backfill_window_only: true,
    ..ProofHistoryConfig::disabled()
};

assert!(config.required_storage_path().is_err());
assert!(config.backfill_window_only);
```

- [ ] **Step 2: Run the targeted node config tests**

Run:

```bash
cargo test -p alethia-reth-node proof_history
```

Expected: tests pass after Task 1. If they fail, fix the missing field propagation before continuing.

- [ ] **Step 3: Move initialization responsibility into `ProofHistoryExEx`**

Change `install_proof_history` so it no longer calls `initialize_proof_history_storage` before constructing the ExEx. Keep both the typed storage and raw storage clone:

```rust
let storage_for_exex = storage.clone();
let storage_for_init = Arc::clone(&mdbx);
let backfill_window_only = config.backfill_window_only;

.install_exex("proofs-history", async move |exex_context| {
    Ok(ProofHistoryExEx::new(
        exex_context,
        storage_for_exex,
        storage_for_init,
        config.window,
        config.prune_interval,
        config.verification_interval,
        backfill_window_only,
    )
    .run())
})
```

Update `ProofHistoryExEx`:

```rust
/// Raw proof-history storage used for initialization writes.
init_storage: Storage,
/// Whether initialization waits for the finalized retention window.
backfill_window_only: bool,
```

Update `ProofHistoryExEx::new` to accept and store `init_storage` and `backfill_window_only`.

- [ ] **Step 4: Add finalized-head reader**

Import the trait:

```rust
use reth_storage_api::{ChainStateBlockReader, StorageSettingsCache};
```

Add this helper near `initialize_proof_history_storage`:

```rust
/// Returns the latest finalized block number known to the local provider.
fn finalized_block_number<Provider>(provider: &Provider) -> eyre::Result<Option<u64>>
where
    Provider: DatabaseProviderFactory,
    Provider::Provider: ChainStateBlockReader,
{
    Ok(provider.database_provider_ro()?.last_finalized_block_number()?)
}
```

Extend trait bounds in `install_proof_history` and the ExEx impl that needs finalized reads:

```rust
<T::Provider as DatabaseProviderFactory>::Provider: ChainStateBlockReader + StorageSettingsCache,
```

and:

```rust
Node::Provider: DatabaseProviderFactory,
<Node::Provider as DatabaseProviderFactory>::Provider: ChainStateBlockReader + StorageSettingsCache,
```

- [ ] **Step 5: Add runtime initialization methods**

Add methods to `impl<Node, Storage, Primitives> ProofHistoryExEx<Node, Storage>`:

```rust
/// Initializes proof-history immediately or returns false when delayed mode must wait.
fn initialize_or_wait(&self) -> eyre::Result<bool> {
    if !proof_history_storage_needs_initialization(&self.init_storage)? {
        self.ensure_initialized()?;
        return Ok(true);
    }

    if !self.backfill_window_only {
        initialize_proof_history_storage(self.ctx.provider(), self.init_storage.clone())?;
        self.ensure_initialized()?;
        return Ok(true);
    }

    self.try_initialize_from_finalized_window()
}

/// Attempts delayed initialization from the finalized proof-history window.
fn try_initialize_from_finalized_window(&self) -> eyre::Result<bool> {
    let finalized = finalized_block_number(self.ctx.provider())?;
    let executed_head = self.ctx.provider().best_block_number()?;

    match delayed_proof_history_start(finalized, executed_head, self.proofs_history_window) {
        DelayedProofHistoryStart::WaitForFinalized => {
            debug!(
                target: "reth::taiko::proof_history",
                executed_head,
                window = self.proofs_history_window,
                "waiting for finalized head before proof-history initialization"
            );
            Ok(false)
        }
        DelayedProofHistoryStart::WaitForExecution { start_block } => {
            debug!(
                target: "reth::taiko::proof_history",
                executed_head,
                start_block,
                window = self.proofs_history_window,
                "waiting for local execution to reach proof-history window start"
            );
            Ok(false)
        }
        DelayedProofHistoryStart::Ready { start_block } => {
            info!(
                target: "reth::taiko::proof_history",
                executed_head,
                finalized_block_number = finalized,
                start_block,
                window = self.proofs_history_window,
                "initializing proof-history from finalized retention window"
            );
            initialize_proof_history_storage(self.ctx.provider(), self.init_storage.clone())?;
            self.ensure_initialized()?;
            Ok(true)
        }
    }
}

/// Acknowledges a skipped ExEx notification so pipeline progress is not blocked.
fn acknowledge_notification(&self, notification: &ExExNotification<Primitives>) -> eyre::Result<()> {
    if let Some(committed_chain) = notification.committed_chain() {
        self.ctx.events.send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
    }
    Ok(())
}
```

- [ ] **Step 6: Wire delayed initialization into `run`**

Replace the beginning of `run` with this structure:

```rust
let mut initialized = self.initialize_or_wait()?;
let mut sync_target_tx = initialized.then(|| self.spawn_sync_task());

if initialized {
    self.spawn_pruner_task();
}

let collector = LiveTrieCollector::new(
    self.ctx.evm_config().clone(),
    self.ctx.provider().clone(),
    &self.storage,
);

while let Some(notification) = self.ctx.notifications.try_next().await? {
    if !initialized {
        initialized = self.initialize_or_wait()?;
        if initialized {
            sync_target_tx = Some(self.spawn_sync_task());
            self.spawn_pruner_task();
        } else {
            self.acknowledge_notification(&notification)?;
            continue;
        }
    }

    let sync_target_tx = sync_target_tx
        .as_ref()
        .expect("proof-history sync target exists after initialization");
    self.handle_notification(notification, &collector, sync_target_tx)?;
}
```

Extract the existing prune spawning block into:

```rust
/// Spawns periodic pruning for initialized proof-history storage.
fn spawn_pruner_task(&self) {
    let prune_task = OpProofStoragePrunerTask::new(
        self.storage.clone(),
        self.ctx.provider().clone(),
        self.proofs_history_window,
        self.proofs_history_prune_interval,
    );
    self.ctx
        .task_executor()
        .spawn_with_graceful_shutdown_signal(|signal| Box::pin(prune_task.run(signal)));
}
```

- [ ] **Step 7: Run node tests and fix compile errors**

Run:

```bash
cargo test -p alethia-reth-node proof_history
```

Expected: all proof-history tests pass. Fix missing trait bounds, missing docs, and borrow errors before continuing.

- [ ] **Step 8: Commit**

```bash
git add crates/node/src/proof_history.rs
git commit -m "feat(proof-history): delay empty storage initialization"
```

### Task 4: Final Verification

**Files:**
- Modify: no source changes expected after this task unless verification fails.

- [ ] **Step 1: Run formatting**

Run:

```bash
just fmt
```

Expected: command exits 0.

- [ ] **Step 2: Run clippy fix**

Run:

```bash
just clippy-fix
```

Expected: command exits 0.

- [ ] **Step 3: Run full tests**

Run:

```bash
just test
```

Expected: all workspace tests pass.

- [ ] **Step 4: Inspect final diff**

Run:

```bash
git status --short
git diff --stat origin/main...HEAD
```

Expected: only proof-history CLI/config/runtime changes plus temporary superpowers docs are present.

- [ ] **Step 5: Commit verification fixes if any**

If formatting or clippy changed files, commit them:

```bash
git add crates/cli/src/lib.rs crates/cli/src/command.rs crates/node/src/proof_history.rs
git commit -m "chore: apply proof history verification fixes"
```

Expected: either a commit is created for mechanical fixes or no commit is needed because the tree is clean.
