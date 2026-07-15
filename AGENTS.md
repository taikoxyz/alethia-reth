# Repository Guidelines

## Project Structure & Module Organization
- Workspace root uses Cargo with `bin/` for the executable, `crates/` for libraries (`node`, `block`, `chainspec`, `cli`, `consensus`, `db`, `evm`, `network`, `payload`, `primitives`, `rpc`).
- Tests live beside implementation modules; integration flows run through `cargo nextest`.
- Docker assets and CI scripts share the root; build artifacts land in `target/`.

## Build, Test, and Development Commands
- `cargo check` – fast type-check of the entire workspace.
- `cargo build --release` – optimized binary at `target/release/alethia-reth`.
- `just fmt` – install toolchain, run `rustfmt`, and apply `cargo sort`.
- `just clippy` – lint with warnings treated as errors.
- `just test` – executes `cargo nextest -v run --workspace --all-features`.
- `cargo run -p alethia-reth-bin --release -- [args]` – launch the node locally.

## Coding Style & Naming Conventions
- Rust defaults: 4-space indentation, `snake_case` items, `CamelCase` types/traits, `SCREAMING_SNAKE_CASE` constants.
- Prefer module-per-file; public APIs go through `crates/node/src/lib.rs`.
- Run `just fmt` before committing; clippy must be clean (`just clippy`).

## Documentation Policy (Mandatory)

- Every non-test production Rust symbol must be documented with Rust doc comments (`//!` or `///`), including modules, structs/enums/traits, fields, constants/statics, type aliases, functions/methods, and associated items in `impl` blocks.
- Trait-implementation methods must also be documented (for example `Display::fmt`, `From::from`, `Default::default`, and `TryFrom::try_from`), even when rustdoc/clippy does not enforce them automatically.
- Comments must explain purpose and contract, not restate identifiers. Include units/invariants for fields and side effects or error semantics where relevant.
- Exclusions:
  - files under `tests/**`
  - `#[cfg(test)]` items and test-only helpers
  - examples
- The docs gate is required before completion: run `just clippy`.

## Testing Guidelines
- Unit tests reside in `#[cfg(test)]` modules next to code; broader flows can use `tests/` or crate-level fixtures.
- Use deterministic data; leverage `chainspec` fixtures in `crates/chainspec/src/genesis/` when possible.
- Execute `just test` prior to PRs; new features should include targeted test coverage.

## Commit & Pull Request Guidelines
- Follow Conventional Commits (`type(scope): summary`), imperative mood, ≤72 characters.
- PRs should summarize behavior changes, reference relevant issues, and note commands run (`just fmt`, `just clippy`, `just test`).
- Include logs or screenshots when altering externally visible behavior; ensure CI passes before requesting review.

## Security & Configuration Tips
- Keep secrets out of the repo; prefer environment variables or CLI flags.
- Review Dockerfile changes carefully—release images expect the `alethia-reth` binary in `/target/release`.
- When adjusting network or RPC crates, verify authentication hooks and engine APIs remain compatible with upstream `reth`.
