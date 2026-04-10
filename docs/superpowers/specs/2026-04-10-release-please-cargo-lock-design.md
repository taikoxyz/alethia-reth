# Release-Please Cargo.lock Handling Design

## Summary

Add `Cargo.lock` update coverage to the existing root `release-please` package so release PRs bump local workspace crate versions in the lockfile alongside the workspace version in `Cargo.toml`.

## Context

This repository uses a single `release-please` package rooted at `.`. The current configuration updates `Cargo.toml` at `$.workspace.package.version`, but it does not update `Cargo.lock`.

The workspace contains these local packages in `Cargo.lock`:

- `alethia-reth-bin`
- `alethia-reth-block`
- `alethia-reth-chainspec`
- `alethia-reth-cli`
- `alethia-reth-consensus`
- `alethia-reth-db`
- `alethia-reth-evm`
- `alethia-reth-network`
- `alethia-reth-node`
- `alethia-reth-node-builder`
- `alethia-reth-payload`
- `alethia-reth-primitives`
- `alethia-reth-rpc`
- `alethia-reth-rpc-types`

Without explicit `extra-files` entries for `Cargo.lock`, a release bump can update the workspace version while leaving lockfile entries at the previous version.

## Goals

- Preserve the existing single-package release flow.
- Ensure release-please updates local workspace package versions in `Cargo.lock`.
- Avoid touching third-party lockfile entries from crates.io or git sources.

## Non-Goals

- Changing release-please to multi-package mode.
- Changing crate versioning strategy away from the workspace version.
- Modifying release automation outside `release-please-config.json`.

## Proposed Change

Keep the existing `packages["."]` configuration and extend its `extra-files` list.

The configuration will continue to update:

- `Cargo.toml` at `$.workspace.package.version`

It will additionally update `Cargo.lock` once per local workspace crate using TOML JSONPath selectors of the form:

`$.package[?(@.name == "<crate-name>" && !@.source)].version`

The `!@.source` guard scopes updates to local workspace packages, since registry and git dependencies in `Cargo.lock` include a `source` field while local workspace packages do not.

## File Changes

### `release-please-config.json`

Add `Cargo.lock` `extra-files` entries for:

- `alethia-reth-bin`
- `alethia-reth-block`
- `alethia-reth-chainspec`
- `alethia-reth-cli`
- `alethia-reth-consensus`
- `alethia-reth-db`
- `alethia-reth-evm`
- `alethia-reth-network`
- `alethia-reth-node`
- `alethia-reth-node-builder`
- `alethia-reth-payload`
- `alethia-reth-primitives`
- `alethia-reth-rpc`
- `alethia-reth-rpc-types`

No other repository files need to change.

## Data Flow

1. A future release-please run computes the next version for package `.`.
2. Release-please updates `Cargo.toml` workspace version.
3. Release-please applies each configured `Cargo.lock` JSONPath and updates the matched local package version entries.
4. The generated release PR includes both the workspace manifest bump and the aligned lockfile version bumps.

## Error Handling and Risks

- If release-please's TOML updater does not evaluate the `!@.source` predicate as expected, `Cargo.lock` entries may fail to update. This is the main compatibility risk.
- If a workspace crate is added later and not reflected in `release-please-config.json`, that crate's lockfile version would remain stale during releases.
- If a local package name ever collides with a third-party package name, the `!@.source` guard remains the intended protection against updating the wrong entry.

## Validation Plan

- Confirm each configured crate name exists in the current `Cargo.lock`.
- Confirm `release-please-config.json` remains valid JSON after the change.
- Rely on a future release PR to verify the updater behavior end-to-end; no runtime code paths are affected by this change.

## Alternatives Considered

### Split release-please into multiple packages

Rejected because it changes release semantics and adds maintenance overhead without solving a broader problem in this repository.

### Leave lockfile updates manual

Rejected because it preserves version skew between `Cargo.toml` and `Cargo.lock` during automated releases.
