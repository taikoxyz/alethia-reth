# Repository Guidelines

## Project Structure & Module Organization
- Runtime code lives in `src/`, grouped by responsibility: `cli/` bootstraps configuration, `network/` drives peer discovery and gossip, `consensus/` and `payload/` handle block production, and `rpc/` exposes external APIs.
- Executables reside under `src/bin/`; audits and specs live in `src/audits/` and `chainspec/` respectively. Build artifacts land in `target/`.
- Place new functionality beside the owning module and re-export shared primitives through `src/lib.rs` to keep boundaries clear. Keep secrets out of the repo and prefer CLI flags or Docker env vars.

## Build, Test, and Development Commands
- `cargo check` – fast validation of the workspace without producing binaries.
- `cargo build --release` – produce optimized binaries in `target/release/` for deployment or benchmarking.
- `just fmt` – install the pinned toolchain, run `rustfmt`, and apply `cargo sort` to keep formatting consistent.
- `just clippy` – enforce lint cleanliness with warnings-as-errors across all features.
- `just test` – execute `cargo nextest -v run --workspace --all-features`; use `cargo test -p <crate> <name>` for focused cases.
- `docker build -t alethia-reth .` followed by `docker run -it --rm alethia-reth [OPTIONS]` to exercise container flows.

## Coding Style & Naming Conventions
- Follow Rust defaults: four-space indentation, `snake_case` for items, `CamelCase` for types/traits, and `SCREAMING_SNAKE_CASE` for constants.
- Let `rustfmt` decide layout; avoid manual alignment tweaks. Document public APIs and keep modules small and composable.
- Default to ASCII; only introduce Unicode when already present and justified. Run `typos` when touching docs.

## Testing Guidelines
- Unit tests belong in `#[cfg(test)]` modules next to the code; larger scenarios live in dedicated submodules under `src/`.
- Cover happy-path and failure cases, asserting on consensus-critical invariants. Prefer deterministic fixtures from `chainspec/` when possible.
- Before opening a PR, ensure `just test` passes locally and note any bespoke commands in the PR body.

## Commit & Pull Request Guidelines
- Use conventional commits (`type(scope): summary`) with imperative phrasing under 72 characters.
- Squash fixups before review. Reference related issues, summarize behavior changes, and attach logs or screenshots for external-facing updates.
- PRs should document the commands you ran (`just fmt`, `just clippy`, `just test`) so reviewers can mirror the checklist.
