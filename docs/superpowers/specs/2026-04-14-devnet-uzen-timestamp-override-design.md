# Devnet Uzen timestamp override rename

**Date:** 2026-04-14
**Status:** Approved design
**Scope:** `crates/cli` + `crates/chainspec`

## Problem

`alethia-reth` currently exposes a devnet-only runtime override named `--devnet-shasta-timestamp`, and the corresponding plumbing mutates the `Shasta` hardfork activation in the devnet chainspec.

That surface is stale for the current codebase:

- the devnet hardfork table already contains a real `Uzen` activation entry
- the requested operator-facing behavior should align with Uzen naming rather than legacy Shasta naming
- keeping a Uzen name while still overriding `Shasta` would create a misleading interface

The change should therefore update both the public surface and the underlying override target.

## Decision

Adopt a clean rename to a Uzen-specific devnet override with no backward-compatibility aliases.

The new operator-facing surface is:

- CLI flag: `--devnet-uzen-timestamp`
- Env var: `ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP`

The default remains `0`. A value of `0` means "use the embedded chainspec value" rather than forcing activation at timestamp zero.

## Change

### CLI surface

In `crates/cli/src/lib.rs`, rename the Taiko extension argument field from Shasta terminology to Uzen terminology and expose:

- `long = "devnet-uzen-timestamp"`
- `env = "ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP"`
- `default_value_t = 0u64`

The help text should describe the value as a devnet Uzen hardfork activation timestamp override and explicitly state that `0` preserves the embedded chainspec value.

The old `--devnet-shasta-timestamp` flag is removed rather than retained as an alias.

### Runtime wiring

In `crates/cli/src/command.rs`:

- rename the `TaikoNodeExtArgs` accessor from `devnet_shasta_timestamp()` to `devnet_uzen_timestamp()`
- rename the concrete `TaikoCliExtArgs` field access accordingly
- keep the override flow otherwise unchanged

In `crates/chainspec/src/spec.rs`:

- rename `clone_with_devnet_shasta_timestamp()` to `clone_with_devnet_uzen_timestamp()`
- keep the existing devnet-only guard based on `TAIKO_DEVNET_GENESIS_HASH`
- change the overridden hardfork from `TaikoHardfork::Shasta` to `TaikoHardfork::Uzen`

This preserves the current contract that the override is ignored for non-devnet chains while making the override semantics match the new Uzen name.

## Behavior

- `--devnet-uzen-timestamp 0` or unset env var: use the embedded devnet Uzen activation timestamp from the compiled chainspec.
- `--devnet-uzen-timestamp <non-zero>`: override the devnet Uzen activation timestamp at runtime.
- `ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP=<non-zero>` with no flag: apply the same runtime override through `clap` env parsing.
- non-devnet chains: ignore the override path exactly as today by returning `None` from the chainspec helper.
- invalid env-var or flag values: fail through normal `clap` parse errors.

## Testing

Update the existing unit coverage so the renamed surface is locked in at both the CLI and chainspec layers.

1. In `crates/chainspec/src/spec.rs`, replace the current Shasta-focused override test with a Uzen-focused one that verifies:
   - `clone_with_devnet_uzen_timestamp(42)` sets `TaikoHardfork::Uzen` to `ForkCondition::Timestamp(42)`
   - the helper still returns `None` for non-devnet chains
2. Add or update CLI parsing tests in `crates/cli` to verify:
   - `--devnet-uzen-timestamp 42` parses successfully
   - the default value is `0`
   - `ALETHIA_RETH_DEVNET_UZEN_TIMESTAMP=42` populates the parsed field when the flag is absent
3. Run `just clippy` as the required docs gate before completion.

## Risks

- **Semantic drift during rename.** The main failure mode is renaming the public API to Uzen while accidentally continuing to override `Shasta`. The chainspec unit test is the primary guard against this.
- **Operator migration break.** Removing the old flag and env var is a deliberate breaking change. This is acceptable because the requirement explicitly rejects a compatibility path.
- **Help-text ambiguity around zero.** If the docs do not say that `0` preserves the embedded value, operators may interpret it as "activate at timestamp 0". The CLI help text should state the zero behavior directly.

## Out of scope

- preserving `--devnet-shasta-timestamp` or any deprecated alias
- matching `taiko-geth` flag spelling exactly
- changing embedded genesis JSON or static hardfork tables
- introducing a generic hardfork override mechanism
