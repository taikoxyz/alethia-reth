# Hoodi Shasta Timestamp Delay (24h)

## Summary
Shift the Taiko Hoodi Shasta hardfork activation by 24 hours (86,400 seconds), from
`1_770_210_000` to `1_770_296_400`. This moves the activation from
2026-02-04 13:00:00 UTC to 2026-02-05 13:00:00 UTC.

## Architecture
The Shasta activation for Hoodi is defined in `TAIKO_HOODI_HARDFORKS` in
`crates/chainspec/src/hardfork.rs` using a timestamp-based `ForkCondition`.
We update that constant only; no changes to genesis files, CLI flags, or other
networks.

## Components
- `crates/chainspec/src/hardfork.rs`: update Shasta timestamp for Hoodi.

## Data Flow
Runtime activation checks use `TaikoHardforks::is_shasta_active(timestamp)`, which
delegates to the Hoodi hardfork list. By shifting the timestamp, blocks whose
timestamps fall between the old and new activation times are treated as pre-Shasta.

## Error Handling
No error handling changes. This is a constant update.

## Testing
No existing tests assert Hoodi Shasta timestamps. Optional follow-up: add a unit
assertion for the Hoodi Shasta `ForkCondition` value to prevent regressions.
