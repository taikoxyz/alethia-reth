//! Single-dimension gas repricing primitives for a future Taiko fork.
//!
//! Pure building blocks — an opcode `static_gas` surcharge table ([`opcodes`]), an
//! input-scaled precompile reprice ([`precompiles`]), and a per-transaction intrinsic
//! surcharge ([`intrinsic`]). They are not yet wired into execution; a future fork gate
//! on [`TaikoSpecId`](crate::spec::TaikoSpecId) activates them. All numeric values in
//! [`schedule`] are non-consensus placeholders pending calibration.

/// Per-opcode / per-tx surcharge schedule types and the example placeholder schedule.
pub mod schedule;
/// Repriced instruction-table builder (additive opcode `static_gas` surcharge).
pub mod opcodes;
/// Input-scaled precompile reprice via `PrecompilesMap` overrides.
pub mod precompiles;
/// Per-transaction intrinsic gas surcharge helper.
pub mod intrinsic;
