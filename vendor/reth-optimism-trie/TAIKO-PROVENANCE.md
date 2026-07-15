# reth-optimism-trie provenance

This directory vendors the `reth-optimism-trie` crate from the Optimism monorepo at
commit `9b802fdb62c96a1cd70b2144ce89979d4e41f4ec`
(<https://github.com/ethereum-optimism/optimism>, path `rust/op-reth/crates/trie`).

It retains the crate's original MIT license; the upstream notice is vendored alongside the
sources as `LICENSE-MIT` (copied from `rust/op-reth/LICENSE-MIT` at the same commit).

## Why vendored

Alethia-Reth pins reth **v2.4.0**, which builds against the published
`reth-primitives-traits`/`reth-codecs` **0.5.x** crates. The Optimism monorepo pins reth
v2.3.0 (published crates 0.4.x). Cargo cannot unify a 0.4→0.5 split across a git
dependency, so consuming `reth-optimism-trie` as a git dependency would force Alethia's
reth pin to always match Optimism's. Vendoring decouples the two pins.

## Local changes

- `Cargo.toml`: rewritten against the Alethia workspace (explicit versions for externals
  the workspace does not declare; `publish = false`; upstream `[lints]` table dropped).
- Source adjustments required by the reth v2.3.0 → v2.4.0 API migration are kept minimal
  and are visible in this repository's history for this directory.

The crate is excluded from Alethia's `missing_docs` clippy gate (see `justfile`) to keep
the vendored source close to upstream.
