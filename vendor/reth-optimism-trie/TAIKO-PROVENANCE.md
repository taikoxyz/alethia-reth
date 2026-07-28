# reth-optimism-trie provenance

This directory vendors the `reth-optimism-trie` crate from the Optimism monorepo at
commit `4f21ce6b9574ffc3473dd3b622ec9f7bd0c72fad`
(<https://github.com/ethereum-optimism/optimism>, path `rust/op-reth/crates/trie`).

It retains the Reth contributors' original MIT license in `LICENSE-MIT`.

## Why vendored

Alethia-Reth pins Reth **v2.4.1** at
`8eb210175687c9f0c889a3b6795c16781d830e3a`, while the approved Optimism snapshot
uses a different Reth revision. Consuming `reth-optimism-trie` through Optimism's
workspace would introduce a second Reth type graph. Vendoring lets this crate inherit
Alethia-Reth's workspace dependencies so the build has one Reth revision.

## Local changes

- `Cargo.toml`: adapted to inherit Reth crates from the Alethia-Reth workspace, retain
  `serde-bincode-compat`, set `publish = false`, and declare external dependencies that
  the Alethia-Reth workspace does not provide.
- `LICENSE-MIT`: copied from the approved Optimism snapshot's `rust/op-reth` package.
- Source and test files are copied without local behavior changes from the approved
  Optimism commit.
