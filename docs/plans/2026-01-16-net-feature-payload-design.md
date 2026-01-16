# Net Feature Payload Design

## Goal
Introduce a `net` feature in `alethia-reth-primitives` so all payload/engine types are gated behind a single feature. This lets downstream users enable one feature (`net`) to get the full payload surface, or disable it to keep guest/no-std builds minimal.

## Context
Today, `alethia-reth-primitives` always compiles payload/engine modules and pulls in `reth-*` and engine dependencies even when downstream disables default features. This creates conflicts for guest targets. We want `net` to be the sole switch for payload/engine, without requiring per-item feature wiring downstream.

## Approach
Add a new feature `net` in `crates/primitives/Cargo.toml` and make net-only dependencies optional. Gate `engine` and `payload` modules behind `cfg(feature = "net")` in `crates/primitives/src/lib.rs`. Update `alethia-reth-block`'s `net` feature to enable `alethia-reth-primitives/net`, since it depends on `TaikoExecutionData` behind the net gate. Defaults will include `net` to preserve current behavior; guest builds can opt out via `default-features = false` and omit `net`.

## Components
- `crates/primitives/Cargo.toml`: add `net` feature and optionalize net-only dependencies.
- `crates/primitives/src/lib.rs`: gate `engine` and `payload` modules with `cfg(feature = "net")`.
- `crates/block/Cargo.toml`: ensure `net` feature enables `alethia-reth-primitives/net`.

## Data Flow
No runtime behavior changes. The payload/engine types and their builders are compiled only when `net` is enabled. Extra data utilities remain always available.

## Error Handling
No new error paths. Compile-time gating should prevent net-only types from being referenced when `net` is disabled.

## Testing
- `cargo test -p alethia-reth-primitives` (default features).
- `cargo check -p alethia-reth-primitives --no-default-features` (net disabled).
- `cargo check -p alethia-reth-block --no-default-features` and `--features net` to verify net wiring.

## Compatibility
Default behavior remains unchanged (net enabled by default). Guest/no-std consumers can disable defaults and avoid net dependencies.
