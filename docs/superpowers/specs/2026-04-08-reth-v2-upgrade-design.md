# Reth v2.0.0 Upgrade Design

## Goal

Rebase this Taiko fork from upstream `reth` `v1.11.3` to `v2.0.0` while also aligning the related dependency families that share the same public type surface, especially `alloy-*` and `revm-*`.

The target success bar is:

- `just fmt` passes
- `just clippy` passes
- `just test` passes

The target integration model is:

- consume upstream `reth` from the GitHub `v2.0.0` tag
- use `/Users/davidcai/Workspace/reth` only as a local source reference during migration
- align local code with upstream `v2.0.0` APIs and crate boundaries instead of preserving `v1.11.3` compatibility shims

## Current Context

This repository pins a broad set of upstream `reth` crates directly in the workspace root `Cargo.toml`, all currently targeting `v1.11.3`. The fork has custom Taiko logic layered across:

- foundation crates: `primitives`, `chainspec`, `evm`
- execution path crates: `consensus`, `block`, `payload`
- node integration crates: `node-builder`, `node`, `network`, `cli`
- external edge crates: `db`, `rpc-types`, `rpc`, `bin/alethia-reth`

Direct `reth` coupling is concentrated in:

- node builder and addon wiring
- CLI/bootstrap code
- payload building and execution
- engine API validation and tree integration
- ETH/auth RPC glue
- database provider and table integration

Upstream `reth v2.0.0` keeps many existing crate names but introduces meaningful surface changes around payload building, RPC wiring, engine integration, and shared primitive/core traits. The migration should assume structural API churn, not just version churn.

## Chosen Strategy

Use a staged upstream-aligned migration on one branch.

This means:

1. update the workspace dependency matrix to `reth v2.0.0` and matching related dependency versions
2. port local crates in dependency order
3. reshape local integrations to follow upstream `v2.0.0` patterns where APIs moved or contracts changed
4. avoid building a long-lived compatibility layer that preserves `v1.11.3` usage patterns

This approach was chosen because it fits the requested outcome better than a compile-first adapter layer. It also reduces the risk of landing a nominal version bump that still diverges from upstream architecture in the most churn-heavy areas.

## Migration Architecture

Treat the upgrade as a workspace-wide rebase with four ordered layers.

### Layer 1: Dependency Baseline

Update the root workspace dependency matrix first:

- switch every upstream `reth` git tag from `v1.11.3` to `v2.0.0`
- align directly pinned ecosystem crates to the versions expected by upstream `reth v2.0.0`
- refresh `Cargo.lock`

The dependency alignment rule is:

- prefer the version family used by upstream `reth v2.0.0`
- only diverge when this fork has a demonstrated Taiko-specific requirement

This layer is expected to flush out the first compile failures caused by mixed-version type conflicts.

### Layer 2: Foundation Crates

Port the crates that define shared Taiko-specific types and execution glue:

- `crates/primitives`
- `crates/chainspec`
- `crates/evm`

These crates should absorb upstream trait and type signature changes before higher-level code is touched. Most other local crates depend on them either directly or indirectly.

### Layer 3: Execution and Node Integration

Port the main runtime integration crates:

- `crates/consensus`
- `crates/block`
- `crates/payload`
- `crates/node-builder`
- `crates/node`
- `crates/network`
- `crates/cli`

This is the highest-risk layer because upstream `v2.0.0` changes in this area are architectural:

- payload builder contracts
- engine validator contracts
- builder context types
- node addon wiring
- CLI launcher and bootstrap integration

### Layer 4: RPC and Storage Edge

Port the crates that depend on earlier layers and expose external surfaces:

- `crates/db`
- `crates/rpc-types`
- `crates/rpc`
- `bin/alethia-reth`

This layer should be intentionally last because it sits on top of engine, provider, and node APIs that move during the earlier phases.

## Workstreams

The implementation plan should track these workstreams explicitly.

### 1. Workspace Dependency Matrix

Update the workspace root `Cargo.toml` so the `reth` family points at upstream `v2.0.0`.

Then reconcile the directly pinned ecosystem crates that share the same type surface. Based on the upstream `v2.0.0` workspace manifest, the initial update set should include:

- `alloy-consensus`
- `alloy-eips`
- `alloy-json-rpc`
- `alloy-network`
- `alloy-rpc-client`
- `alloy-rpc-types-engine`
- `alloy-rpc-types-eth`
- `alloy-rpc-types-trace`
- `alloy-rpc-types-txpool`
- `alloy-serde`
- `alloy-signer`
- `alloy-signer-local`
- `alloy-evm`
- `revm-database-interface`

Additional direct version edits should be driven by actual compile conflicts after the initial rebase.

### 2. Upstream API Surface Remap

Build a migration table from current imports to their `v2.0.0` homes using `/Users/davidcai/Workspace/reth` as the source reference.

The remap should focus on:

- umbrella `reth` imports used in local CLI, node, payload, RPC, network, and DB code
- payload builder interfaces and builder primitives
- engine API validator and tree validator wiring
- RPC ETH conversion and helper APIs
- shared primitive and trait imports affected by upstream core/primitives changes

This table is a planning artifact, not a compatibility layer. Its purpose is to make the code edits targeted and upstream-aligned.

### 3. Taiko Foundation Port

Update Taiko-specific foundation code so it compiles cleanly against the new upstream contracts:

- payload attribute and engine types
- chainspec and hardfork extensions
- custom EVM config, factory, handler, and spec code
- shared Taiko primitive helpers

The rule for this workstream is to absorb signature churn here whenever possible instead of pushing adaptation upward into node or RPC crates.

### 4. Node and Payload Integration Port

Rework local runtime integration to match `reth v2.0.0` builder contracts and examples:

- payload builder integration
- executor builder integration
- consensus builder integration
- network builder integration
- addon and RPC registration integration
- CLI launcher/bootstrap code

The primary upstream references should be the `v2.0.0` node, payload, and example code paths under `/Users/davidcai/Workspace/reth`.

### 5. RPC/Auth and Storage Port

After execution-path code is stable, port:

- custom ETH/auth RPC modules
- engine API wrappers and validators
- DB model/provider integration
- binary entrypoint wiring

This workstream should not start early because its compile errors tend to reflect churn in lower layers rather than independent design issues.

## Error Handling and Migration Rules

### Hard Checkpoints

Use hard checkpoints instead of one final cleanup pass.

1. After the dependency bump, run a workspace compile check to expose the first break set.
2. After each migration layer, run targeted checks for the affected crates.
3. Only after all layers compile cleanly should the full repository gates run.

### Upstream Alignment Rule

If a local refactor begins preserving obsolete `v1.11.3` patterns through adapters or local wrappers without a Taiko-specific reason, stop and re-anchor on the upstream `v2.0.0` implementation.

### Scope Rule

Only perform refactors that directly serve the `reth v2.0.0` migration. Do not use this work to pursue unrelated cleanup.

### Documentation Rule

This repository requires Rust doc comments on production symbols. Any new or reshaped public API introduced during the migration must remain documented so that `just clippy` can serve as the docs gate.

## Verification Strategy

The implementation plan should define verification at three levels.

### Level 1: Dependency Checkpoint

Immediately after the dependency rebase:

- refresh the lockfile
- run a workspace `cargo check`

This confirms whether version alignment is coherent before crate-level edits begin.

### Level 2: Layer Checkpoints

After each migration layer, run targeted `cargo check -p ...` commands for the affected crates. This keeps the failure set bounded and prevents later layers from masking foundation issues.

### Level 3: Repository Gates

The final definition of done is:

- `just fmt`
- `just clippy`
- `just test`

These are mandatory and should only run after the crate graph is stable enough that failures are meaningful.

## Expected Deliverable Shape

The implementation plan produced from this design should include:

- a dependency-upgrade phase
- a crate-by-crate migration matrix in dependency order
- explicit upstream reference paths under `/Users/davidcai/Workspace/reth`
- verification commands for each phase
- final repository gates for formatting, clippy, and tests

The implementation plan should optimize for bounded failure domains, upstream alignment, and an end state that is easy to maintain against future upstream `reth` releases.
