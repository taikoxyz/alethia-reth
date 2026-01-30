# Optimization Scan Design (2026-01-30)

## Goal
Find and apply low-risk, behavior-preserving optimizations similar to commits 0e2bde4 (error handling/unwrap removal) and f765312 (clone reduction/preallocation) across the repository. Focus on correctness-safe improvements with clear intent and no semantic change beyond safer error reporting.

## Scope
- **Robustness**: remove unsafe `unwrap/expect` in runtime paths; prefer explicit error propagation; avoid leaking internal errors in RPC; reduce `map_err(|_| ...)` that discards context unless intentionally redacting.
- **Performance**: reduce unnecessary `clone/to_vec/collect` in hot or repeated paths; preallocate `Vec` when size is known; prefer borrowing or `From<&T>` conversion paths.

## Non-goals
- No algorithmic changes, consensus changes, or behavioral changes to protocol logic.
- No micro-optimizations with unclear benefit or nontrivial refactors.
- No public API redesigns unless required to remove `unwrap` or obvious copies.

## Approach
1. **Static scan**: search for `unwrap/expect/panic!/todo!/unimplemented!` and cloning hotspots (`clone`, `to_vec`, `collect::<Vec<_>>()`, `Vec::new()` in loops).
2. **Candidate review**: shortlist only cases with clear, safe replacements (e.g., `ok_or_else`, `with_capacity`, borrowing input, `From<&T>` conversions).
3. **Patch set**: implement minimal diffs with localized impact; keep changes small per file.
4. **Verification**: run targeted checks (`cargo check` or crate-specific builds) and ensure no new warnings.

## Risk Management
- Avoid modifying consensus/execution semantics.
- Preserve external error surfaces unless the change is explicitly to redact internal details.
- Prefer additive helpers over invasive changes when mapping errors.

## Testing/Validation
- `cargo check` for workspace sanity.
- If a change touches a crate with existing tests, run relevant tests or `just test` if time permits.

