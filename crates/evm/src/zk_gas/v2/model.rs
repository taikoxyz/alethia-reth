//! Handwritten checked arithmetic for the generated pricing model.

use super::{
    generated::{
        BLOB_SOURCE_TARIFFS, BLOCK_FIXED, FIXED_POINT_SCALE, MAXIMUM_TERMS_PER_SCOPE, MPT_TARIFFS,
        OPCODE_ROUTE_INDEX, PARENT_MPT_TARIFFS, PRECOMPILE_ROUTE_INDEX, PROPOSAL_STARTUP,
        RESOURCE_TARIFFS, ROUTES, SPAWN_ROUTE_INDEX, TRANSACTION_PAYLOAD_TARIFFS,
    },
    types::{
        AccessClass, BlockPricingInput, CalldataAccessClass, MemorySelectors, MptSelectors,
        OperationModelInput, OperationPricingInput, OperationRoute, PricingBreakdown,
        ProposalPricingInput, ResourceSelectors, SELECTOR_BOUND, StorageBranch, ValueClass,
        ZkGasV2Error,
    },
};

/// Generated route-composition mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Composition {
    /// Charge only the accepted absolute model.
    Accepted,
    /// Charge the maximum of route-aggregate incremental, absolute, and structural candidates.
    Maximum,
}

/// Generated selector reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selector {
    /// Interpreter-native opcode gas.
    InterpreterRawGas,
    /// Native precompile gas.
    NativeGas,
    /// One unit per event.
    Event,
    /// Typed route-specific work units.
    WorkUnits,
}

/// Typed feature shape required by generated fixed-price models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequiredInput {
    /// No feature payload is inspected.
    None,
    /// A static raw-gas payload is required.
    StaticRawGas,
    /// A typed LOG payload is required.
    Log,
}

/// Generated accepted absolute model family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelKind {
    /// Fixed event cost.
    Fixed,
    /// Fixed event cost with optional shared memory and raw-gas fallback.
    FixedWithMemory,
    /// Maximum of a constant/minimum and one linear selector.
    Linear,
    /// Common dispatch plus native-gas body.
    StaticRawGas,
    /// Common dispatch plus affine native-gas body.
    RawGasAffine,
    /// EXP small bucket or quadratic exponent-byte model.
    Exp,
    /// KECCAK permutation and shared-memory model.
    Keccak,
    /// Shared-memory access model.
    MemoryAccess,
    /// MCOPY word and shared-memory model.
    MemoryCopy,
    /// CALLDATACOPY/RETURNDATACOPY word and shared-memory model.
    Copy,
    /// Piecewise LOG topic/data-word and shared-memory model.
    LogPiecewise,
    /// RETURN word and shared-memory model with conservative out-of-envelope extension.
    Return,
    /// Stateful SLOAD/SSTORE branch model.
    Storage,
    /// Declared context-class model.
    Context,
}

/// Three shared memory coefficients in generated fixed-point units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MemoryCoefficients {
    /// Cost per growth event.
    pub(crate) growth_event: i128,
    /// Cost per native memory-expansion gas unit.
    pub(crate) evm_gas_delta: i128,
    /// Cost per crossed 4 KiB boundary.
    pub(crate) boundary_4k_event: i128,
}

/// Generated linear candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinearRule {
    /// Runtime selector consumed by the rule.
    pub(crate) selector: Selector,
    /// Fixed-point coefficient per selector unit.
    pub(crate) coefficient: i128,
    /// Constant lower bound.
    pub(crate) constant: i128,
    /// Explicit minimum lower bound.
    pub(crate) minimum: i128,
}

/// One generated operation route and all of its candidate rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RouteSpec {
    /// Typed runtime route.
    pub(crate) route: OperationRoute,
    /// Route-level candidate composition.
    pub(crate) composition: Composition,
    /// Accepted absolute family.
    pub(crate) model_kind: ModelKind,
    /// Family-specific fixed-point coefficients.
    pub(crate) coefficients: [i128; 12],
    /// Shared-memory coefficients, zero when unused.
    pub(crate) memory: MemoryCoefficients,
    /// Typed payload shape required by fixed models.
    pub(crate) required_input: RequiredInput,
    /// Exact raw-gas envelope, when declared.
    pub(crate) exact_raw_gas: Option<u64>,
    /// Maximum raw-gas envelope, when declared.
    pub(crate) maximum_raw_gas: Option<u64>,
    /// Out-of-envelope raw-gas linear floor.
    pub(crate) fallback_raw_gas_coefficient: Option<i128>,
    /// Maximum RETURN bytes in the measured envelope.
    pub(crate) maximum_returned_bytes: Option<u64>,
    /// RETURN extrapolation chunk size.
    pub(crate) return_boundary_chunk_bytes: Option<u64>,
    /// RETURN out-of-envelope raw-gas coefficient.
    pub(crate) return_raw_gas_coefficient: Option<i128>,
    /// Controlled incremental candidate.
    pub(crate) incremental: Option<LinearRule>,
    /// Structural candidate.
    pub(crate) structural: Option<LinearRule>,
    /// Explicit feature-error absolute model.
    pub(crate) feature_error: Option<LinearRule>,
}

/// Stateless evaluator for the accepted schema-9 pricing rules.
#[derive(Clone, Copy, Debug, Default)]
pub struct ZkGasV2Model;

impl ZkGasV2Model {
    /// Returns the number of authenticated operation routes compiled into the model.
    pub const fn route_count(self) -> usize {
        ROUTES.len()
    }

    /// Returns the generated model input for an opcode without a dedicated typed extractor.
    ///
    /// Runtime collection handles structured families (EXP, memory, copy, storage, context,
    /// LOG, and RETURN) explicitly. This method keeps the remaining `None` versus
    /// `StaticRawGas` decision tied to the authenticated generated route instead of duplicating
    /// a handwritten opcode table in the execution observer.
    pub(crate) fn default_opcode_model_input(
        self,
        opcode: u8,
        raw_gas: u64,
    ) -> Result<OperationModelInput, ZkGasV2Error> {
        let route = lookup_route(OperationRoute::Opcode(opcode))?;
        match route.model_kind {
            ModelKind::Fixed | ModelKind::Linear => Ok(OperationModelInput::None),
            ModelKind::FixedWithMemory => match route.required_input {
                RequiredInput::None => Ok(OperationModelInput::None),
                RequiredInput::StaticRawGas => Ok(OperationModelInput::StaticRawGas { raw_gas }),
                RequiredInput::Log => Err(ZkGasV2Error::MissingInput("log")),
            },
            ModelKind::StaticRawGas | ModelKind::RawGasAffine => {
                Ok(OperationModelInput::StaticRawGas { raw_gas })
            }
            ModelKind::Exp => Err(ZkGasV2Error::MissingInput("exp")),
            ModelKind::Keccak => Err(ZkGasV2Error::MissingInput("keccak")),
            ModelKind::MemoryAccess => Err(ZkGasV2Error::MissingInput("memory_access")),
            ModelKind::MemoryCopy => Err(ZkGasV2Error::MissingInput("memory_copy")),
            ModelKind::Copy => Err(ZkGasV2Error::MissingInput("copy")),
            ModelKind::LogPiecewise => Err(ZkGasV2Error::MissingInput("log")),
            ModelKind::Return => Err(ZkGasV2Error::MissingInput("return")),
            ModelKind::Storage => Err(ZkGasV2Error::MissingInput("storage")),
            ModelKind::Context => Err(ZkGasV2Error::MissingInput("declared_context")),
        }
    }

    /// Prices one operation without applying the block-boundary ceiling.
    pub fn price_operation(
        self,
        input: &OperationPricingInput,
    ) -> Result<PricingBreakdown, ZkGasV2Error> {
        let route = lookup_route(input.route)?;
        validate_operation_namespace(input)?;
        if let Some(raw_gas) = input.interpreter_raw_gas {
            bounded("interpreter_raw_gas", raw_gas)?;
        }
        if let Some(native_gas) = input.native_gas {
            bounded("native_gas", native_gas)?;
        }
        if input.feature_error {
            if input.model_input != OperationModelInput::None {
                return Err(ZkGasV2Error::InvalidInput("model_input"));
            }
            let rule =
                route.feature_error.ok_or(ZkGasV2Error::InvalidInput("feature_error_route"))?;
            if !matches!(input.route, OperationRoute::Opcode(_)) {
                return Err(ZkGasV2Error::InvalidInput("feature_error_route"));
            }
            let total = price_linear(rule, input)?;
            ensure_nonnegative(total, "feature_error")?;
            return Ok(PricingBreakdown {
                feature_error_scaled: total,
                total_scaled: total,
                ..Default::default()
            });
        }

        let accepted = price_accepted(route, input)?;
        let incremental = match route.incremental {
            Some(rule) => price_linear(rule, input)?,
            None => 0,
        };
        let structural = match route.structural {
            Some(rule) => price_linear(rule, input)?,
            None => 0,
        };
        ensure_nonnegative(accepted, "accepted_absolute")?;
        ensure_nonnegative(incremental, "incremental")?;
        ensure_nonnegative(structural, "structural")?;
        let total = match route.composition {
            Composition::Accepted => accepted,
            Composition::Maximum => accepted.max(incremental).max(structural),
        };
        Ok(PricingBreakdown {
            accepted_absolute_scaled: accepted,
            incremental_scaled: incremental,
            structural_scaled: structural,
            total_scaled: total,
            ..Default::default()
        })
    }

    /// Prices one complete block-local selector snapshot and applies the final PGU ceiling once.
    pub fn price_block(
        self,
        input: &BlockPricingInput<'_>,
    ) -> Result<PricingBreakdown, ZkGasV2Error> {
        let block_term_count = input
            .operation_routes
            .len()
            .checked_mul(4)
            .and_then(|count| count.checked_add(15))
            .ok_or(ZkGasV2Error::ArithmeticOverflow)?;
        enforce_term_count(block_term_count)?;
        let fixed = checked_dot(&[
            (BLOCK_FIXED[0], 1),
            (
                BLOCK_FIXED[1],
                bounded("started_non_anchor_transactions", input.started_non_anchor_transactions)?,
            ),
            (BLOCK_FIXED[2], bounded("native_value_transfers", input.native_value_transfers)?),
        ])?;

        let mut operations = 0_i128;
        let mut feature_errors = 0_i128;
        for (index, accumulated) in input.operation_routes.iter().enumerate() {
            if input.operation_routes[..index].iter().any(|row| row.route == accumulated.route) {
                return Err(ZkGasV2Error::InvalidInput("duplicate_operation_route"));
            }
            let route = lookup_route(accumulated.route)?;
            ensure_nonnegative(accumulated.accepted_absolute_scaled, "accepted_absolute")?;
            ensure_nonnegative(accumulated.incremental_scaled, "incremental")?;
            ensure_nonnegative(accumulated.structural_scaled, "structural")?;
            ensure_nonnegative(accumulated.feature_error_scaled, "feature_error")?;
            let normal = match route.composition {
                Composition::Accepted => accumulated.accepted_absolute_scaled,
                Composition::Maximum => accumulated
                    .accepted_absolute_scaled
                    .max(accumulated.incremental_scaled)
                    .max(accumulated.structural_scaled),
            };
            operations =
                checked_add(operations, checked_add(normal, accumulated.feature_error_scaled)?)?;
            feature_errors = checked_add(feature_errors, accumulated.feature_error_scaled)?;
        }

        let resources = price_resources(input.resources)?;
        let mpt = price_mpt(input.mpt, MPT_TARIFFS)?;
        let total = checked_sum(&[fixed, operations, resources, mpt])?;
        ensure_nonnegative(total, "block")?;
        Ok(PricingBreakdown {
            fixed_scaled: fixed,
            operations_scaled: operations,
            feature_error_scaled: feature_errors,
            resources_scaled: resources,
            mpt_scaled: mpt,
            total_scaled: total,
            total_pgu: Some(ceil_scaled(total)?),
            ..Default::default()
        })
    }

    /// Prices proposal-once selectors and applies the proposal-scope ceiling once.
    pub fn price_proposal_once(
        self,
        input: &ProposalPricingInput<'_>,
    ) -> Result<PricingBreakdown, ZkGasV2Error> {
        validate_blob_source_domain(input)?;
        let transaction_payload = checked_dot(&[
            (
                TRANSACTION_PAYLOAD_TARIFFS[0],
                bounded("canonical_transaction_bytes", input.canonical_transaction_bytes)?,
            ),
            (
                TRANSACTION_PAYLOAD_TARIFFS[1],
                bounded("nonempty_payload_transactions", input.nonempty_payload_transactions)?,
            ),
            (
                TRANSACTION_PAYLOAD_TARIFFS[2],
                bounded("true_unattempted_transactions", input.true_unattempted_transactions)?,
            ),
        ])?;
        let total_blobs = input.source_blob_counts.iter().try_fold(0_u64, |total, value| {
            total.checked_add(*value).ok_or(ZkGasV2Error::ArithmeticOverflow)
        })?;
        let extra_blobs = total_blobs - 1;
        let extra_sources = input.source_blob_counts.len() as u64 - 1;
        let blob_source = checked_dot(&[
            (BLOB_SOURCE_TARIFFS[0], extra_blobs),
            (BLOB_SOURCE_TARIFFS[1], extra_sources),
        ])?;
        let parent_mpt = price_mpt(input.parent_mpt, PARENT_MPT_TARIFFS)?;
        let total = checked_sum(&[PROPOSAL_STARTUP, transaction_payload, blob_source, parent_mpt])?;
        ensure_nonnegative(total, "proposal")?;
        Ok(PricingBreakdown {
            startup_scaled: PROPOSAL_STARTUP,
            transaction_payload_scaled: transaction_payload,
            blob_source_scaled: blob_source,
            parent_mpt_scaled: parent_mpt,
            total_scaled: total,
            total_pgu: Some(ceil_scaled(total)?),
            ..Default::default()
        })
    }
}

/// Finds one authenticated generated route.
fn lookup_route(route: OperationRoute) -> Result<&'static RouteSpec, ZkGasV2Error> {
    let index = match route {
        OperationRoute::Opcode(opcode) => OPCODE_ROUTE_INDEX[usize::from(opcode)],
        OperationRoute::ConfirmedSpawn(opcode) => SPAWN_ROUTE_INDEX[usize::from(opcode)],
        OperationRoute::Precompile(address) => PRECOMPILE_ROUTE_INDEX
            .binary_search_by_key(&address, |(candidate, _)| *candidate)
            .ok()
            .map_or(u16::MAX, |position| PRECOMPILE_ROUTE_INDEX[position].1),
    };
    if index == u16::MAX {
        return Err(ZkGasV2Error::UnknownRoute(route));
    }
    ROUTES.get(usize::from(index)).ok_or(ZkGasV2Error::UnknownRoute(route))
}

/// Prices the generated accepted absolute family for one event.
fn price_accepted(route: &RouteSpec, input: &OperationPricingInput) -> Result<i128, ZkGasV2Error> {
    let c = route.coefficients;
    validate_exact_raw_gas(route, input)?;
    match route.model_kind {
        ModelKind::Fixed => {
            validate_unparameterized_model_input(route, input)?;
            Ok(c[0])
        }
        ModelKind::FixedWithMemory => {
            let memory = validate_required_input(route.required_input, input)?;
            if let Some(maximum) = route.maximum_raw_gas {
                let raw = required_raw_gas(input)?;
                if raw > maximum {
                    let coefficient = route
                        .fallback_raw_gas_coefficient
                        .ok_or(ZkGasV2Error::InvalidInput("raw_gas_out_of_envelope"))?;
                    return checked_mul(coefficient, bounded("interpreter_raw_gas", raw)?);
                }
            }
            checked_add(c[0], price_memory(route.memory, memory)?)
        }
        ModelKind::Linear => {
            validate_unparameterized_model_input(route, input)?;
            let value = selector_value(Selector::from_code(c[3])?, input)?;
            let linear = checked_mul(c[0], value)?;
            Ok(linear.max(c[1]).max(c[2]))
        }
        ModelKind::StaticRawGas => {
            let raw = validate_static_raw_gas(input)?;
            checked_add(c[0], checked_mul(c[1], raw)?)
        }
        ModelKind::RawGasAffine => {
            let raw = validate_static_raw_gas(input)?;
            checked_sum(&[c[0], c[1], checked_mul(c[2], raw)?])
        }
        ModelKind::Exp => {
            let OperationModelInput::Exp { exponent_byte_length } = input.model_input else {
                return Err(ZkGasV2Error::MissingInput("exp"));
            };
            let length = bounded("exponent_byte_length", exponent_byte_length)?;
            let square_u64 = exponent_byte_length
                .checked_mul(exponent_byte_length)
                .ok_or(ZkGasV2Error::ArithmeticOverflow)?;
            let square = bounded("exponent_byte_length_squared", square_u64)?;
            let body = if exponent_byte_length <= 4 {
                c[4]
            } else {
                c[4].max(checked_sum(&[
                    c[1],
                    checked_mul(c[2], length)?,
                    checked_mul(c[3], square)?,
                ])?)
            };
            checked_add(c[0], body)
        }
        ModelKind::Keccak => {
            let OperationModelInput::Keccak { input_length, memory } = input.model_input else {
                return Err(ZkGasV2Error::MissingInput("keccak"));
            };
            let input_length = bounded("input_length", input_length)?;
            let permutations = if input_length == 0 { 0 } else { input_length / 136 + 1 };
            checked_sum(&[
                c[0],
                c[1],
                if input_length == 0 { c[3] } else { 0 },
                checked_mul(c[2], permutations)?,
                price_memory(route.memory, memory)?,
            ])
        }
        ModelKind::MemoryAccess => {
            let OperationModelInput::MemoryAccess { memory } = input.model_input else {
                return Err(ZkGasV2Error::MissingInput("memory_access"));
            };
            checked_sum(&[c[0], c[1], price_memory(route.memory, memory)?])
        }
        ModelKind::MemoryCopy => {
            let OperationModelInput::MemoryCopy { copy_words, memory } = input.model_input else {
                return Err(ZkGasV2Error::MissingInput("memory_copy"));
            };
            checked_sum(&[
                c[0],
                c[1],
                checked_mul(c[2], bounded("copy_words", copy_words)?)?,
                price_memory(route.memory, memory)?,
            ])
        }
        ModelKind::Copy => {
            let OperationModelInput::Copy { copied_bytes, copy_words, memory } = input.model_input
            else {
                return Err(ZkGasV2Error::MissingInput("copy"));
            };
            validate_word_count("copy_words", copied_bytes, copy_words)?;
            let raw = required_raw_gas(input)?;
            let expected = 3_u64
                .checked_add(3_u64.checked_mul(copy_words).ok_or(ZkGasV2Error::ArithmeticOverflow)?)
                .and_then(|value| value.checked_add(memory.evm_gas_delta))
                .ok_or(ZkGasV2Error::ArithmeticOverflow)?;
            if raw != expected {
                return Err(ZkGasV2Error::InvalidInput("copy_raw_gas"));
            }
            checked_sum(&[
                c[0],
                c[1],
                checked_mul(c[2], bounded("copy_words", copy_words)?)?,
                price_memory(route.memory, memory)?,
            ])
        }
        ModelKind::LogPiecewise => {
            let (topic_count, data_words, memory) = validate_log(input)?;
            checked_sum(&[
                c[0],
                c[1],
                checked_mul(c[2], topic_count)?,
                if data_words > 0 { c[3] } else { 0 },
                checked_mul(c[4], if data_words > 0 { data_words - 1 } else { 0 })?,
                price_memory(route.memory, memory)?,
            ])
        }
        ModelKind::Return => price_return(route, input),
        ModelKind::Storage => price_storage(route, input),
        ModelKind::Context => price_context(route, input),
    }
}

/// Enforces the mutually exclusive opcode, precompile, and spawn input namespaces.
fn validate_operation_namespace(input: &OperationPricingInput) -> Result<(), ZkGasV2Error> {
    match input.route {
        OperationRoute::Opcode(_) => {
            if input.interpreter_raw_gas.is_none() {
                return Err(ZkGasV2Error::MissingInput("interpreter_raw_gas"));
            }
            if input.native_gas.is_some() {
                return Err(ZkGasV2Error::InvalidInput("operation_namespace"));
            }
        }
        OperationRoute::Precompile(_) => {
            if input.native_gas.is_none() {
                return Err(ZkGasV2Error::MissingInput("native_gas"));
            }
            if input.interpreter_raw_gas.is_some() {
                return Err(ZkGasV2Error::InvalidInput("operation_namespace"));
            }
        }
        OperationRoute::ConfirmedSpawn(_) => {
            if input.interpreter_raw_gas.is_some() || input.native_gas.is_some() {
                return Err(ZkGasV2Error::InvalidInput("operation_namespace"));
            }
        }
    }
    Ok(())
}

/// Rejects typed feature payloads that no generated rule for this route consumes.
fn validate_unparameterized_model_input(
    route: &RouteSpec,
    input: &OperationPricingInput,
) -> Result<(), ZkGasV2Error> {
    let consumes_work_units = matches!(route.incremental, Some(rule) if rule.selector == Selector::WorkUnits) ||
        matches!(route.structural, Some(rule) if rule.selector == Selector::WorkUnits) ||
        (route.model_kind == ModelKind::Linear && route.coefficients[3] == 3);
    match input.model_input {
        OperationModelInput::None => Ok(()),
        OperationModelInput::WorkUnits { .. } if consumes_work_units => Ok(()),
        _ => Err(ZkGasV2Error::InvalidInput("model_input")),
    }
}

/// Prices RETURN, including its conservative repeated-boundary extension.
fn price_return(route: &RouteSpec, input: &OperationPricingInput) -> Result<i128, ZkGasV2Error> {
    let OperationModelInput::Return { returned_bytes, return_words, memory } = input.model_input
    else {
        return Err(ZkGasV2Error::MissingInput("return"));
    };
    bounded("returned_bytes", returned_bytes)?;
    bounded("return_words", return_words)?;
    validate_word_count("return_words", returned_bytes, return_words)?;
    let maximum =
        route.maximum_returned_bytes.ok_or(ZkGasV2Error::InvalidInput("return_envelope"))?;
    if returned_bytes > maximum {
        let chunk =
            route.return_boundary_chunk_bytes.ok_or(ZkGasV2Error::InvalidInput("return_chunk"))?;
        let boundary_words = chunk.div_ceil(32);
        let boundary_body = checked_sum(&[
            route.coefficients[0],
            route.coefficients[1],
            route.coefficients[2],
            checked_mul(route.coefficients[3], boundary_words - 1)?,
            price_memory(route.memory, memory)?,
        ])?;
        let chunks = returned_bytes.div_ceil(chunk);
        let raw_linear = checked_mul(
            route.return_raw_gas_coefficient.ok_or(ZkGasV2Error::InvalidInput("return_raw_gas"))?,
            bounded("interpreter_raw_gas", required_raw_gas(input)?)?,
        )?;
        return Ok(raw_linear.max(checked_mul(boundary_body, chunks)?));
    }
    checked_sum(&[
        route.coefficients[0],
        route.coefficients[1],
        if return_words > 0 { route.coefficients[2] } else { 0 },
        checked_mul(route.coefficients[3], if return_words > 0 { return_words - 1 } else { 0 })?,
        price_memory(route.memory, memory)?,
    ])
}

/// Prices the generated SLOAD/SSTORE model.
fn price_storage(route: &RouteSpec, input: &OperationPricingInput) -> Result<i128, ZkGasV2Error> {
    let c = route.coefficients;
    match (route.route, input.model_input) {
        (OperationRoute::Opcode(0x54), OperationModelInput::StorageLoad { access }) => {
            let extra = if access == AccessClass::Cold { c[1] } else { 0 };
            checked_add(c[0], extra)
        }
        (OperationRoute::Opcode(0x55), OperationModelInput::StorageStore { access, branch }) => {
            if access == AccessClass::Cold &&
                matches!(branch, StorageBranch::DirtyRewrite | StorageBranch::RestoreOriginal)
            {
                return Err(ZkGasV2Error::InvalidInput("cold_dirty_sstore"));
            }
            let branch_cost = match branch {
                StorageBranch::Clear => c[2],
                StorageBranch::DirtyRewrite => c[3],
                StorageBranch::Noop => c[4],
                StorageBranch::Reset => c[5],
                StorageBranch::RestoreOriginal => c[6],
                StorageBranch::Set => c[7],
            };
            checked_add(branch_cost, if access == AccessClass::Cold { c[8] } else { 0 })
        }
        _ => Err(ZkGasV2Error::MissingInput("storage")),
    }
}

/// Prices a generated declared-context class.
fn price_context(route: &RouteSpec, input: &OperationPricingInput) -> Result<i128, ZkGasV2Error> {
    let c = route.coefficients;
    match (route.route, input.model_input) {
        (OperationRoute::Opcode(0x30), OperationModelInput::ContextFixed) => Ok(c[0]),
        (OperationRoute::Opcode(0x33), OperationModelInput::ContextFixed) => Ok(c[1]),
        (OperationRoute::Opcode(0x34), OperationModelInput::ContextValue { class }) => {
            Ok(if class == ValueClass::Nonzero { c[2] } else { c[3] })
        }
        (OperationRoute::Opcode(0x35), OperationModelInput::CalldataLoad { access }) => {
            Ok(match access {
                CalldataAccessClass::Full => c[4],
                CalldataAccessClass::Partial => c[5],
                CalldataAccessClass::Zero => c[6],
            })
        }
        (OperationRoute::Opcode(0x36), OperationModelInput::CalldataSize { input_length }) => {
            bounded("calldata_length", input_length)?;
            Ok(c[7])
        }
        (
            OperationRoute::Opcode(0x42),
            OperationModelInput::ContextValue { class: ValueClass::Nonzero },
        ) => Ok(c[8]),
        (OperationRoute::Opcode(0x42), OperationModelInput::ContextValue { .. }) => {
            Err(ZkGasV2Error::InvalidInput("timestamp_value_class"))
        }
        _ => Err(ZkGasV2Error::MissingInput("declared_context")),
    }
}

/// Validates the fixed-model input shape and returns its memory selectors.
fn validate_required_input(
    required: RequiredInput,
    input: &OperationPricingInput,
) -> Result<MemorySelectors, ZkGasV2Error> {
    match (required, input.model_input) {
        (RequiredInput::None, OperationModelInput::None) => Ok(MemorySelectors::default()),
        (RequiredInput::StaticRawGas, OperationModelInput::StaticRawGas { raw_gas }) => {
            if Some(raw_gas) != input.interpreter_raw_gas {
                return Err(ZkGasV2Error::InvalidInput("static_raw_gas"));
            }
            bounded("interpreter_raw_gas", raw_gas)?;
            Ok(MemorySelectors::default())
        }
        (RequiredInput::Log, OperationModelInput::Log { .. }) => {
            let (_, _, memory) = validate_log(input)?;
            Ok(memory)
        }
        _ => Err(ZkGasV2Error::MissingInput("model_input")),
    }
}

/// Validates LOG relationships and returns bounded topic/data words plus memory.
fn validate_log(
    input: &OperationPricingInput,
) -> Result<(u64, u64, MemorySelectors), ZkGasV2Error> {
    let OperationRoute::Opcode(opcode @ 0xa0..=0xa4) = input.route else {
        return Err(ZkGasV2Error::InvalidInput("log_route"));
    };
    let OperationModelInput::Log { topic_count, data_bytes, data_words, memory } =
        input.model_input
    else {
        return Err(ZkGasV2Error::MissingInput("log"));
    };
    if topic_count != u64::from(opcode - 0xa0) {
        return Err(ZkGasV2Error::InvalidInput("log_topic_count"));
    }
    validate_word_count("log_data_words", data_bytes, data_words)?;
    let expected = 375_u64
        .checked_add(375_u64.checked_mul(topic_count).ok_or(ZkGasV2Error::ArithmeticOverflow)?)
        .and_then(|value| value.checked_add(8_u64.checked_mul(data_bytes)?))
        .and_then(|value| value.checked_add(memory.evm_gas_delta))
        .ok_or(ZkGasV2Error::ArithmeticOverflow)?;
    if Some(expected) != input.interpreter_raw_gas {
        return Err(ZkGasV2Error::InvalidInput("log_raw_gas"));
    }
    Ok((bounded("log_topic_count", topic_count)?, bounded("log_data_words", data_words)?, memory))
}

/// Validates and prices shared-memory selectors.
fn price_memory(
    coefficients: MemoryCoefficients,
    selectors: MemorySelectors,
) -> Result<i128, ZkGasV2Error> {
    checked_dot(&[
        (coefficients.growth_event, bounded("memory_growth_events", selectors.growth_events)?),
        (coefficients.evm_gas_delta, bounded("memory_evm_gas_delta", selectors.evm_gas_delta)?),
        (
            coefficients.boundary_4k_event,
            bounded("memory_4k_boundary_events", selectors.boundary_4k_events)?,
        ),
    ])
}

/// Prices a generated linear rule.
fn price_linear(rule: LinearRule, input: &OperationPricingInput) -> Result<i128, ZkGasV2Error> {
    let linear = checked_mul(rule.coefficient, selector_value(rule.selector, input)?)?;
    Ok(linear.max(rule.constant).max(rule.minimum))
}

/// Reads and bounds one operation selector.
fn selector_value(selector: Selector, input: &OperationPricingInput) -> Result<u64, ZkGasV2Error> {
    match selector {
        Selector::InterpreterRawGas => bounded("interpreter_raw_gas", required_raw_gas(input)?),
        Selector::NativeGas => {
            bounded("native_gas", input.native_gas.ok_or(ZkGasV2Error::MissingInput("native_gas"))?)
        }
        Selector::Event => Ok(1),
        Selector::WorkUnits => match input.model_input {
            OperationModelInput::WorkUnits { work_units } => bounded("work_units", work_units),
            _ => Err(ZkGasV2Error::MissingInput("work_units")),
        },
    }
}

impl Selector {
    /// Decodes a generated selector discriminator embedded in the coefficient array.
    fn from_code(code: i128) -> Result<Self, ZkGasV2Error> {
        match code {
            0 => Ok(Self::InterpreterRawGas),
            1 => Ok(Self::NativeGas),
            2 => Ok(Self::Event),
            3 => Ok(Self::WorkUnits),
            _ => Err(ZkGasV2Error::InvalidInput("generated_selector")),
        }
    }
}

/// Requires interpreter raw gas to be present.
fn required_raw_gas(input: &OperationPricingInput) -> Result<u64, ZkGasV2Error> {
    input.interpreter_raw_gas.ok_or(ZkGasV2Error::MissingInput("interpreter_raw_gas"))
}

/// Validates a static model's raw gas mirrors the interpreter observation.
fn validate_static_raw_gas(input: &OperationPricingInput) -> Result<u64, ZkGasV2Error> {
    let OperationModelInput::StaticRawGas { raw_gas } = input.model_input else {
        return Err(ZkGasV2Error::MissingInput("static_raw_gas"));
    };
    if Some(raw_gas) != input.interpreter_raw_gas {
        return Err(ZkGasV2Error::InvalidInput("static_raw_gas"));
    }
    bounded("interpreter_raw_gas", raw_gas)
}

/// Enforces a generated exact raw-gas envelope when present.
fn validate_exact_raw_gas(
    route: &RouteSpec,
    input: &OperationPricingInput,
) -> Result<(), ZkGasV2Error> {
    if let Some(expected) = route.exact_raw_gas &&
        required_raw_gas(input)? != expected
    {
        return Err(ZkGasV2Error::InvalidInput("exact_raw_gas"));
    }
    Ok(())
}

/// Validates the ceiling byte-to-word relationship.
fn validate_word_count(name: &'static str, bytes: u64, words: u64) -> Result<(), ZkGasV2Error> {
    bounded(name, words)?;
    bounded("byte_length", bytes)?;
    if words != bytes.div_ceil(32) {
        return Err(ZkGasV2Error::InvalidInput(name));
    }
    Ok(())
}

/// Prices the five unique-resource selectors.
fn price_resources(selectors: ResourceSelectors) -> Result<i128, ZkGasV2Error> {
    checked_dot(&[
        (RESOURCE_TARIFFS[0], bounded("unique_account_count", selectors.unique_account_count)?),
        (
            RESOURCE_TARIFFS[1],
            bounded(
                "unique_storage_slot_access_count",
                selectors.unique_storage_slot_access_count,
            )?,
        ),
        (
            RESOURCE_TARIFFS[2],
            bounded("unique_storage_slot_write_count", selectors.unique_storage_slot_write_count)?,
        ),
        (RESOURCE_TARIFFS[3], bounded("unique_code_hash_count", selectors.unique_code_hash_count)?),
        (RESOURCE_TARIFFS[4], bounded("unique_code_bytes", selectors.unique_code_bytes)?),
    ])
}

/// Prices seven MPT selectors with the supplied block or parent tariff table.
fn price_mpt(selectors: MptSelectors, tariffs: [i128; 7]) -> Result<i128, ZkGasV2Error> {
    checked_dot(&[
        (tariffs[0], bounded("path_node_visits", selectors.path_node_visits)?),
        (tariffs[1], bounded("materialization_work_units", selectors.materialization_work_units)?),
        (tariffs[2], bounded("hydration_hash_blocks", selectors.hydration_hash_blocks)?),
        (tariffs[3], bounded("dirty_encoded_nodes", selectors.dirty_encoded_nodes)?),
        (tariffs[4], bounded("dirty_hash_blocks", selectors.dirty_hash_blocks)?),
        (
            tariffs[5],
            bounded(
                "structural_branch_extension_churn_nodes",
                selectors.structural_branch_extension_churn_nodes,
            )?,
        ),
        (tariffs[6], bounded("dropped_trie_nodes", selectors.dropped_trie_nodes)?),
    ])
}

/// Validates the sealed BPO2 source/blob topology.
fn validate_blob_source_domain(input: &ProposalPricingInput<'_>) -> Result<(), ZkGasV2Error> {
    let source_count = input.source_blob_counts.len();
    if !(1..=11).contains(&source_count) {
        return Err(ZkGasV2Error::InvalidInput("source_count"));
    }
    for value in input.source_blob_counts {
        bounded("source_blob_count", *value)?;
    }
    if input.source_blob_counts[..source_count - 1].iter().any(|count| *count != 1) {
        return Err(ZkGasV2Error::InvalidInput("forced_source_blob_count"));
    }
    let normal = input.source_blob_counts[source_count - 1];
    if !(1..=6).contains(&normal) {
        return Err(ZkGasV2Error::InvalidInput("normal_source_blob_count"));
    }
    let total = input.source_blob_counts.iter().try_fold(0_u64, |sum, count| {
        sum.checked_add(*count).ok_or(ZkGasV2Error::ArithmeticOverflow)
    })?;
    if !(1..=16).contains(&total) {
        return Err(ZkGasV2Error::InvalidInput("total_blob_count"));
    }
    if [
        input.proposal_blob_hash_count,
        input.provider_blob_count,
        input.provider_commitment_count,
        input.provider_proof_count,
    ]
    .into_iter()
    .any(|count| count != total)
    {
        return Err(ZkGasV2Error::InvalidInput("provider_blob_counts"));
    }
    Ok(())
}

/// Rejects values outside the authenticated selector envelope.
fn bounded(name: &'static str, value: u64) -> Result<u64, ZkGasV2Error> {
    if value > SELECTOR_BOUND {
        return Err(ZkGasV2Error::SelectorOutOfRange { selector: name, value });
    }
    Ok(value)
}

/// Enforces the sealed maximum number of terms per pricing scope.
fn enforce_term_count(actual: usize) -> Result<(), ZkGasV2Error> {
    if actual > MAXIMUM_TERMS_PER_SCOPE {
        return Err(ZkGasV2Error::TooManyTerms { actual, maximum: MAXIMUM_TERMS_PER_SCOPE });
    }
    Ok(())
}

/// Adds two fixed-point values with typed overflow.
pub(super) fn checked_add(left: i128, right: i128) -> Result<i128, ZkGasV2Error> {
    left.checked_add(right).ok_or(ZkGasV2Error::ArithmeticOverflow)
}

/// Multiplies one fixed-point coefficient by a bounded selector.
fn checked_mul(coefficient: i128, selector: u64) -> Result<i128, ZkGasV2Error> {
    coefficient.checked_mul(i128::from(selector)).ok_or(ZkGasV2Error::ArithmeticOverflow)
}

/// Sums fixed-point values with typed overflow.
fn checked_sum(values: &[i128]) -> Result<i128, ZkGasV2Error> {
    values.iter().try_fold(0_i128, |total, value| checked_add(total, *value))
}

/// Computes a fixed-point dot product with checked signed arithmetic.
fn checked_dot(terms: &[(i128, u64)]) -> Result<i128, ZkGasV2Error> {
    terms.iter().try_fold(0_i128, |total, (coefficient, selector)| {
        checked_add(total, checked_mul(*coefficient, *selector)?)
    })
}

/// Rejects a negative composed component before state or admission decisions.
fn ensure_nonnegative(value: i128, component: &'static str) -> Result<(), ZkGasV2Error> {
    if value < 0 {
        return Err(ZkGasV2Error::NegativeComponent(component));
    }
    Ok(())
}

/// Applies mathematical ceiling at an authenticated block/proposal boundary.
fn ceil_scaled(value: i128) -> Result<u128, ZkGasV2Error> {
    ensure_nonnegative(value, "rounding_boundary")?;
    let quotient = value / FIXED_POINT_SCALE;
    let remainder = value % FIXED_POINT_SCALE;
    let rounded =
        quotient.checked_add(i128::from(remainder != 0)).ok_or(ZkGasV2Error::ArithmeticOverflow)?;
    u128::try_from(rounded).map_err(|_| ZkGasV2Error::ArithmeticOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zk_gas_v2_model_checked_arithmetic_returns_the_typed_overflow_error() {
        assert_eq!(checked_add(i128::MAX, 1), Err(ZkGasV2Error::ArithmeticOverflow));
        assert_eq!(checked_mul(i128::MAX, 2), Err(ZkGasV2Error::ArithmeticOverflow));
    }

    #[test]
    fn zk_gas_v2_model_generated_indexes_resolve_every_authenticated_route() {
        for expected in ROUTES {
            assert!(
                core::ptr::eq(lookup_route(expected.route).expect("route indexed"), expected),
                "wrong generated index for {:?}",
                expected.route
            );
        }
    }
}
