# Repository Guidelines

## Project Structure & Module Organization
Rust sources live under `src/`, anchored by `src/lib.rs` which wires Taiko-specific primitives on top of Reth. Module directories map to runtime domains: `src/bin` hosts the `alethia-reth` entry point, `src/cli` defines flag parsing, `src/consensus`, `src/network`, and `src/payload` extend execution logic, while `src/rpc` exposes the `taiko_` and `taikoAuth_` namespaces. Chain parameters and fixtures live in `src/chainspec`. Audit notes and references reside in `src/audits`, and any shared utilities should be grouped with the subsystem they belong to. Assets generated during builds land in `target/`.

## Build, Test, and Development Commands
Use Cargo for all workflows. Typical iterations rely on `cargo check` for fast validation, `cargo build --release` to produce the shipping binary in `target/release/alethia-reth`, and `cargo run --release -- --help` to inspect runtime options. Lint the codebase with `cargo fmt --all` and `cargo clippy --all-targets --all-features`; both are enforced by the CI pipeline. Docker users can run `docker build -t alethia-reth .` before shipping container images.

## Coding Style & Naming Conventions
Follow standard Rust style with four-space indentation and wrap lines at 100 columns where practical. Types and traits use `PascalCase`, modules and functions use `snake_case`, constants use `SCREAMING_SNAKE_CASE`, and feature flags match Cargo manifest naming. Always run `cargo fmt --all` and `cargo clippy --fix --allow-dirty` prior to committing, and keep module-level documentation comments (`//!`) up to date so generated docs describe subsystem boundaries.

## Testing Guidelines
Author unit tests alongside implementation modules under `#[cfg(test)]` blocks, and prefer deterministic fixtures stored with the module. Run the full suite with `cargo test --all --locked`; target-specific subsets can be run with `cargo test -p alethia-reth <module>::<case>`. When adding integration tests, place them under `tests/` and mirror the naming of the subsystem under test. Aim to cover new RPC methods and consensus edge cases, documenting any intentionally skipped scenarios in the test module header.

## Commit & Pull Request Guidelines
The history follows Conventional Commits such as `feat(repo): bump reth dependency`. Use the `type(scope): summary` format, keep the subject under 72 characters, and reference GitHub issues in the body when relevant. Pull requests should describe the motivation, list the main changes, and enumerate the manual or automated checks run (e.g., `cargo test`, `cargo clippy`). Include configuration notes or screenshots whenever behavior impacts node operators. Request review from a domain owner before merging.
