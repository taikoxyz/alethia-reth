# Hoodi Unzen Fork Time Design

## Goal

Set the embedded Taiko Hoodi Unzen hardfork activation to Thursday, June 18, 2026
at 13:00:00 UTC. The corresponding Unix timestamp is `1781787600`.

## Scope

This change applies only to the Hoodi chainspec hardfork table in
`crates/chainspec/src/hardfork.rs`. It does not change Mainnet, Devnet, or
Masaya activation schedules, and it does not add a runtime override path.

## Current Behavior

`TAIKO_HOODI_HARDFORKS` currently activates Ontake and Pacaya at block `0`,
activates Shasta by timestamp, and leaves Unzen as `ForkCondition::Never`.
Because shared Ethereum Cancun, Prague, and Osaka fork conditions are derived
from the Taiko Unzen condition in `extend_with_shared_hardforks`, they also do
not activate on Hoodi today.

## Proposed Change

Replace Hoodi's Unzen activation with
`ForkCondition::Timestamp(1_781_787_600)`.

The existing hardfork extension path will then derive matching Cancun, Prague,
and Osaka timestamp activations from the Hoodi Unzen timestamp. This keeps the
Taiko hardfork and EVM fork flags aligned through the existing code path.

## Testing

Add a focused chainspec regression beside the Hoodi Shasta timestamp test. The
test should assert that Hoodi Unzen is timestamp-based and equals
`ForkCondition::Timestamp(1_781_787_600)`.

Run:

```sh
cargo test -p alethia-reth-chainspec hoodi
git diff --check
```

`just clippy` remains the full documentation and lint gate if a complete
pre-PR check is required.
