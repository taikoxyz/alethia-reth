# Claude Context for Alethia-Reth

This document keeps Claude aligned with the current multi-crate layout of the Alethia-Reth repository.

## Project Overview

Alethia-Reth is a Rust execution client for the Taiko protocol, built atop Paradigm's Reth. The workspace is split into focused crates (e.g., `crates/block`, `crates/evm`, `crates/rpc`) with the top-level orchestrator in `crates/node`. The packaged binary lives in `bin/alethia-reth`.

## Key Technologies

- Language: Rust (`1.93.1` toolchain via `rust-toolchain.toml` / `justfile`)
- Framework: Reth v2.0.0 APIs (`reth_node_builder`, `reth_rpc`, `reth_engine_primitives`, etc.)
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
4. New RPC or engine features often span multiple crates (primitives → payload → rpc). Follow the v2 crate boundaries and traits when wiring changes through the stack.
5. Treat secrets/config cautiously—use env vars or CLI flags rather than hardcoding.

## Custom Claude Commands

No repo-local Claude command set is currently checked in. Prefer the repository commands in `justfile` and the documented crate entry points above.
