//! Fork-scoped zk gas schedules for Taiko consensus.

/// Inspector-side Unzen opcode metering and shared error definitions.
pub mod adapter;
/// Canonical length-operand halts for Unzen execution on narrow-pointer targets.
#[cfg(target_pointer_width = "32")]
pub(crate) mod length_operands;
/// Checked zk gas accounting for a single Unzen block execution.
pub mod meter;
/// Production interpreter-side zk gas metering.
pub mod runtime;
/// Shared schedule types and fork selection helpers.
pub mod schedule;
/// Unzen-specific fixed zk gas schedule data.
pub mod unzen;

#[cfg(test)]
mod tests;
