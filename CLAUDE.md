# Claude Context for Alethia-Reth

This document keeps Claude aligned with the current multi-crate layout of the Alethia-Reth repository.

## Project Overview

Alethia-Reth is a Rust execution client for the Taiko protocol, built atop Paradigm's Reth. The workspace is split into focused crates (e.g., `crates/block`, `crates/evm`, `crates/rpc`) with the top-level orchestrator in `crates/node`. The packaged binary lives in `bin/alethia-reth`.

## Key Technologies

- Language: Rust (stable 1.88+ recommended)
- Framework: Reth 1.7.0 APIs (`reth_node_builder`, `reth_rpc`, etc.)
- Target protocol: Taiko rollup networks
- Build & dependency manager: Cargo + `just`

## Project Structure

```
/                      Workspace root, Dockerfile, justfile, docs
├── bin/alethia-reth   Binary crate producing `alethia-reth`
└── crates/
    ├── node           Public API surface (`TaikoNode`, add-ons)
    ├── block          Block execution/assembly
    ├── chainspec      Chain specs + genesis data
    ├── cli            CLI wrapper (`TaikoCli`)
    ├── consensus      Beacon consensus extensions
    ├── db             Taiko-specific tables & codecs
    ├── evm            EVM config, handlers, execution helpers
    ├── network        P2P network builder
    ├── payload        Payload builder service
    ├── primitives     Shared types (engine, payload attributes)
    └── rpc            Taiko RPC (eth / engine / auth)
```

## Development Guidelines

- **Formatting**: `just fmt` (installs toolchain, runs `rustfmt` + `cargo sort`).
- **Clippy**: `just clippy` (warnings treated as errors). Fix lints before committing.
- **Testing**: `just test` (runs `cargo nextest -v run --workspace --all-features`). Add targeted tests for new logic.
- **Build**: `cargo build --release` places binary at `target/release/alethia-reth`.

## Agent Tips

1. Update workspace paths when moving crates—Cargo manifests reference `../{crate}`.
2. When adding modules, re-export through the owning crate’s `lib.rs` if you expect consumers to use them.
3. Keep genesis fixtures in `crates/chainspec/src/genesis/`; avoid rewriting unless network specs change.
4. New RPC or engine features often span multiple crates (primitives → payload → rpc). Ensure imports follow the new module boundaries.
5. Treat secrets/config cautiously—use env vars or CLI flags rather than hardcoding.

## Custom Claude Commands

Custom helpers live in `.claude/commands/` (e.g., `improve-pr-desc.md`). Invoke them via `claude run` if the CLI harness supports it.
