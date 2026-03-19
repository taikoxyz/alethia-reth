//! Fork-scoped zk gas schedules for Taiko consensus.

/// Shared schedule types and fork selection helpers.
pub mod schedule;
/// Uzen-specific fixed zk gas schedule data.
pub mod uzen;

#[cfg(test)]
mod tests;
