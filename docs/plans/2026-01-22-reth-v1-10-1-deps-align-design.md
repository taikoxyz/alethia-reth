# Reth v1.10.1 Dependency Alignment Design

**Goal:** Align alethia-reth dependency versions to upstream reth v1.10.1 with minimal risk by upgrading all reth git deps to v1.10.1 and syncing overlapping workspace dependencies to upstream versions.

**Architecture:**
- Two-step alignment: (1) upgrade all `reth*` git dependencies to tag `v1.10.1`; (2) align overlapping `workspace.dependencies` entries with upstream reth v1.10.1.
- Use upstream `Cargo.toml` (`/Users/davidcai/Workspace/reth` at tag `v1.10.1`) as the single source of truth for dependency versions/features.

**Scope & Constraints:**
- Only update dependencies that exist in both upstream and this repo’s `workspace.dependencies`.
- Do not add new dependencies that only exist upstream; do not remove local-only dependencies.
- Prefer exact version/feature parity; deviations must be explicitly noted.

**Data Sources:**
- Upstream: `/Users/davidcai/Workspace/reth` checked out at tag `v1.10.1`.
- Local: `/Users/davidcai/taiko/alethia-reth/Cargo.toml`.

**Implementation Summary:**
1. Extract upstream `workspace.dependencies` (versions + features).
2. Extract local `workspace.dependencies`.
3. Compute intersection and build a mapping table of version/feature changes.
4. Update local `Cargo.toml` to match upstream for those entries.
5. Update all `reth*` git dependency tags from `v1.10.0` to `v1.10.1`.
6. Regenerate `Cargo.lock` by running a minimal build/check.

**Validation:**
- Run `cargo check` (or `just clippy` if needed) to validate dependency resolution.
- Review `Cargo.lock` changes for unexpected upgrades outside the intended scope.

**Risks:**
- Upstream dependency graph may pull newer transitive versions. Mitigation: keep scope to overlapping direct deps and review `Cargo.lock` diff.
- Feature mismatches if local requires extra features not present upstream. Mitigation: explicitly preserve and document differences.
