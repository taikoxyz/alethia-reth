# Repository Guidelines

## Project Structure & Module Organization
The Rust crate is defined in `Cargo.toml`, with runtime logic under `src/`. Feature modules are grouped by responsibility: `cli/` handles startup configuration, `network/` peers and gossip, `consensus/` and `payload/` manage block production, and `rpc/` exposes external APIs. Executables live in `src/bin/`, while audits and spec fixtures sit under `src/audits/` and `chainspec/`. Build outputs land in `target/`. Place new code beside the module that owns it and route shared primitives through `src/lib.rs`.

## Build, Test, and Development Commands
Run `cargo build --release` for optimized binaries in `target/release/`. During iteration, prefer `cargo check` for faster feedback. `just clippy` enforces warnings-as-errors via `cargo clippy --all-features`. Use `just fmt` to install the pinned toolchain and apply `rustfmt` plus `cargo sort`. Tests run through `just test`, which proxies to `cargo nextest -v run --workspace --all-features`. Docker workflows use `docker build -t alethia-reth .` then `docker run -it --rm alethia-reth [OPTIONS]`.

## Coding Style & Naming Conventions
Follow Rust defaults: four-space indentation, `snake_case` for modules and functions, `CamelCase` for types and traits, and `SCREAMING_SNAKE_CASE` for constants. `rustfmt.toml` governs formatting; never hand-edit around it. Keep public APIs documented and prefer small, composable modules. Before pushing, run `just fmt` and `just clippy` so formatting, sorting, and linting remain clean. Spell-check commits with `typos` if touching documentation.

## Testing Guidelines
Unit tests should live in `#[cfg(test)] mod tests` blocks near the code under test. Scenario or integration checks belong in dedicated submodules within `src/` (follow the owning component). Every feature PR must cover happy path and failure cases; assert on consensus-critical invariants. Run `just test` before opening a PR, and add targeted `cargo test -p <crate> <name>` snippets to the PR body when special setup is required.

## Commit & Pull Request Guidelines
Commits follow a conventional pattern (`type(scope): summary`), as in `chore(repo): introduce justfile`. Keep messages imperative and under 72 characters. Squash fixups before review. Pull requests need a concise description, linked issue or context, and notes on testing (`just test`, etc.). Include configuration diffs, logs, or screenshots whenever behavior changes externally. Request review once CI is green and lint/test commands pass locally.

## Tooling & Environment
Formatting and unused-dependency checks rely on `rustup toolchain install nightly-2025-09-27`; ensure it is available before running `just fmt` or `just udeps`. Prefer the `just` recipes for reproducibility, but mirror the underlying `cargo` command in documentation so CI stays aligned. Keep `.env` secrets out of the repo and rely on CLI flags or Docker environment variables during local experiments.
