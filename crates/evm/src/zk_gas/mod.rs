//! Fork-scoped zk gas schedules for Taiko consensus.

/// Inspector-side Unzen opcode metering and shared error definitions.
pub mod adapter;
/// Checked zk gas accounting for a single Unzen block execution.
pub mod meter;
/// Production interpreter-side zk gas metering.
pub mod runtime;
/// Shared schedule types and fork selection helpers.
pub mod schedule;
/// Test-only zk gas trace export for the shared vector corpus.
#[cfg(any(test, feature = "zk-gas-trace"))]
pub mod trace;
/// Unzen-specific fixed zk gas schedule data.
pub mod unzen;

#[cfg(test)]
mod meter_vectors;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod vectors;
