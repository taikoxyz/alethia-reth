//! Public typed inputs, outputs, and failures for zk gas pricing.

use core::fmt;

/// Maximum value accepted for any nonnegative runtime selector.
pub const SELECTOR_BOUND: u64 = 1_000_000_000;

/// Warm/cold state-access classification emitted by the EVM handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessClass {
    /// The state location was already warm for this transaction.
    Warm,
    /// The state location was cold before this access.
    Cold,
}

/// CALLDATALOAD's typed access class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalldataAccessClass {
    /// No calldata byte was available at the requested offset.
    Zero,
    /// Fewer than 32 calldata bytes were available.
    Partial,
    /// A complete 32-byte word was available.
    Full,
}

/// Zero/nonzero value class used by declared context models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueClass {
    /// The observed value was zero.
    Zero,
    /// The observed value was nonzero.
    Nonzero,
}

/// SSTORE semantic branch selected by the EVM state transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageBranch {
    /// The write clears a previously nonzero value.
    Clear,
    /// The slot was already dirty and is rewritten.
    DirtyRewrite,
    /// The write leaves the current value unchanged.
    Noop,
    /// The write resets a nonzero slot to another value.
    Reset,
    /// The write restores the original slot value.
    RestoreOriginal,
    /// The write changes a zero slot to nonzero.
    Set,
}

/// Shared memory-expansion selectors attached to one opcode event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemorySelectors {
    /// Number of memory-growth events, normally zero or one.
    pub growth_events: u64,
    /// Native EVM gas attributed to memory expansion.
    pub evm_gas_delta: u64,
    /// Whether this event crosses at least one additional 4 KiB memory page, encoded as zero or
    /// one.
    pub boundary_4k_events: u64,
}

/// Typed operation feature payload consumed by the accepted model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationModelInput {
    /// The route has no accepted-model feature payload.
    None,
    /// Direct-precompile structural work units, such as BN254 scalar iterations.
    WorkUnits {
        /// Route-specific nonnegative structural work units.
        work_units: u64,
    },
    /// A static opcode whose body is parameterized by native gas.
    StaticRawGas {
        /// Native gas observed by the interpreter for this opcode.
        raw_gas: u64,
    },
    /// EXP exponent length.
    Exp {
        /// Significant exponent bytes.
        exponent_byte_length: u64,
    },
    /// KECCAK input length and memory selectors.
    Keccak {
        /// Number of bytes hashed.
        input_length: u64,
        /// Memory-expansion selectors for this event.
        memory: MemorySelectors,
    },
    /// MLOAD/MSTORE/MSTORE8 memory selectors.
    MemoryAccess {
        /// Memory-expansion selectors for this event.
        memory: MemorySelectors,
    },
    /// Parent-frame reservation by a confirmed CALL/CALLCODE/DELEGATECALL/STATICCALL wrapper.
    SpawnMemory {
        /// Parent memory-expansion selectors, excluding forwarded gas and child work.
        memory: MemorySelectors,
    },
    /// MCOPY word count and memory selectors.
    MemoryCopy {
        /// Ceiling of copied bytes divided by 32.
        copy_words: u64,
        /// Memory-expansion selectors for this event.
        memory: MemorySelectors,
    },
    /// CALLDATACOPY or RETURNDATACOPY typed features.
    Copy {
        /// Number of bytes copied.
        copied_bytes: u64,
        /// Ceiling of copied bytes divided by 32.
        copy_words: u64,
        /// Memory-expansion selectors for this event.
        memory: MemorySelectors,
    },
    /// LOG typed features.
    Log {
        /// Topic count implied by the opcode.
        topic_count: u64,
        /// Number of log-data bytes.
        data_bytes: u64,
        /// Ceiling of data bytes divided by 32.
        data_words: u64,
        /// Memory-expansion selectors for this event.
        memory: MemorySelectors,
    },
    /// RETURN typed features.
    Return {
        /// Number of returned bytes.
        returned_bytes: u64,
        /// Ceiling of returned bytes divided by 32.
        return_words: u64,
        /// Memory-expansion selectors for this event.
        memory: MemorySelectors,
    },
    /// SLOAD access class.
    StorageLoad {
        /// Warm/cold access classification.
        access: AccessClass,
    },
    /// SSTORE access and semantic branch.
    StorageStore {
        /// Warm/cold access classification.
        access: AccessClass,
        /// Semantic SSTORE branch.
        branch: StorageBranch,
    },
    /// ADDRESS or CALLER fixed context class.
    ContextFixed,
    /// CALLVALUE or TIMESTAMP value class.
    ContextValue {
        /// Zero/nonzero classification.
        class: ValueClass,
    },
    /// CALLDATALOAD access class.
    CalldataLoad {
        /// Zero/partial/full word classification.
        access: CalldataAccessClass,
    },
    /// CALLDATASIZE input length.
    CalldataSize {
        /// Calldata length in bytes.
        input_length: u64,
    },
}

/// Authenticated operation namespace used for generated route lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationRoute {
    /// Ordinary opcode route.
    Opcode(u8),
    /// Direct precompile route; includes P256 at `0x100`.
    Precompile(u16),
    /// Confirmed child-frame wrapper for a spawn opcode.
    ConfirmedSpawn(u8),
}

/// One execution-local operation presented to the pure evaluator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationPricingInput {
    /// Authenticated route selected by runtime collection.
    pub route: OperationRoute,
    /// Interpreter gas for opcode routes.
    pub interpreter_raw_gas: Option<u64>,
    /// Native precompile gas for direct-precompile routes.
    pub native_gas: Option<u64>,
    /// Typed accepted-model features.
    pub model_input: OperationModelInput,
    /// Whether this event must use the conservative fallback because exact accepted-model inputs
    /// were unavailable for the executed outcome.
    pub feature_error: bool,
}

/// Transactionally accumulated candidates for one authenticated route in a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationRoutePricingInput {
    /// Authenticated route represented by these accumulated candidates.
    pub route: OperationRoute,
    /// Sum of accepted absolute event contributions.
    pub accepted_absolute_scaled: i128,
    /// Sum of controlled incremental event contributions.
    pub incremental_scaled: i128,
    /// Sum of structural event contributions.
    pub structural_scaled: i128,
    /// Sum of explicit feature-error event contributions.
    pub feature_error_scaled: i128,
}

/// Five block-local unique-resource selectors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceSelectors {
    /// Unique accounts accessed by committed transactions.
    pub unique_account_count: u64,
    /// Unique storage slots accessed by committed transactions.
    pub unique_storage_slot_access_count: u64,
    /// Unique storage slots written by committed transactions.
    pub unique_storage_slot_write_count: u64,
    /// Unique code hashes materialized by committed transactions.
    pub unique_code_hash_count: u64,
    /// Total bytes across unique materialized code hashes.
    pub unique_code_bytes: u64,
}

/// Seven exact sparse-trie workload selectors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MptSelectors {
    /// Trie nodes visited while following access paths.
    pub path_node_visits: u64,
    /// Work units spent materializing witnessed nodes.
    pub materialization_work_units: u64,
    /// Hash blocks processed while hydrating witnessed nodes.
    pub hydration_hash_blocks: u64,
    /// Dirty nodes encoded during root computation.
    pub dirty_encoded_nodes: u64,
    /// Hash blocks processed for dirty nodes.
    pub dirty_hash_blocks: u64,
    /// Branch/extension nodes created or removed by structural churn.
    pub structural_branch_extension_churn_nodes: u64,
    /// Trie nodes dropped during structural updates.
    pub dropped_trie_nodes: u64,
}

/// Additive canonical signed-transaction payload selectors for one accounting scope.
///
/// The block pipeline supplies signed EIP-2718 sizes, not calldata-only sizes. Proposal
/// completion supplies only input payload not already charged by committed block scopes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransactionPayloadSelectors {
    /// Sum of canonical signed EIP-2718 encoded transaction lengths, in bytes.
    pub canonical_transaction_bytes: u64,
    /// Number of transactions with nonempty calldata.
    pub nonempty_payload_transactions: u64,
}

/// Actual accepted nonempty create-frame preparations in one accounting scope.
///
/// Counts are per occurrence, including work later reverted by an ancestor; they
/// are never deduplicated by code hash. The current post-Unzen domain permits
/// 1..=24576 bytes per nonempty event. Future code-size forks need a new domain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodePreparationSelectors {
    /// Number of accepted create frames that prepared nonempty output code.
    pub nonempty_events: u64,
    /// Sum of original output byte lengths across those events.
    pub total_bytes: u64,
}

/// Complete block-local input; every slice entry must be committed.
#[derive(Clone, Copy, Debug)]
pub struct BlockPricingInput<'a> {
    /// Started non-anchor transaction count.
    pub started_non_anchor_transactions: u64,
    /// Native value-transfer count.
    pub native_value_transfers: u64,
    /// Raw included EIP-7702 authorization entries, including ignored validations.
    pub authorization_validations: u64,
    /// Raw included non-legacy access-list address items, including duplicate and Anchor items.
    pub access_list_items: u64,
    /// Accepted code preparation occurrences and processed bytes, without deduplication.
    pub code_preparation: CodePreparationSelectors,
    /// Committed per-route candidate aggregates.
    pub operation_routes: &'a [OperationRoutePricingInput],
    /// Unique-resource selectors.
    pub resources: ResourceSelectors,
    /// Payload of committed signed transactions, including Anchor and failed receipts.
    pub transaction_payload: TransactionPayloadSelectors,
    /// Exact block-local MPT selectors.
    pub mpt: MptSelectors,
}

/// Proposal-once input disjoint from all block-local pricing.
#[derive(Clone, Copy, Debug)]
pub struct ProposalPricingInput<'a> {
    /// Additional canonical signed EIP-2718 bytes not already charged by block scopes.
    pub canonical_transaction_bytes: u64,
    /// Additional nonempty-calldata transactions not already charged by block scopes.
    pub nonempty_payload_transactions: u64,
    /// Transactions truly unattempted by proposal execution.
    pub true_unattempted_transactions: u64,
    /// Per-source blob counts in forced-prefix then normal-source order.
    pub source_blob_counts: &'a [u64],
    /// Blob hash count committed by the proposal.
    pub proposal_blob_hash_count: u64,
    /// Blob count supplied by the provider.
    pub provider_blob_count: u64,
    /// Commitment count supplied by the provider.
    pub provider_commitment_count: u64,
    /// Proof count supplied by the provider.
    pub provider_proof_count: u64,
    /// Exact proposal-parent MPT selectors.
    pub parent_mpt: MptSelectors,
}

/// Field-by-field fixed-point pricing result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PricingBreakdown {
    /// Accepted absolute contribution for one operation.
    pub accepted_absolute_scaled: i128,
    /// Controlled incremental contribution for one operation or block route group.
    pub incremental_scaled: i128,
    /// Structural floor contribution for one operation or block route group.
    pub structural_scaled: i128,
    /// Explicit feature-error contribution for one operation or block scope.
    pub feature_error_scaled: i128,
    /// Block fixed-cost contribution.
    pub fixed_scaled: i128,
    /// EIP-7702 authorization validation contribution, charged once per raw entry.
    pub authorization_scaled: i128,
    /// Raw access-list address item contribution in normalized fixed-point PGU.
    pub access_list_scaled: i128,
    /// Per-occurrence accepted code preparation contribution in normalized fixed-point PGU.
    pub code_preparation_scaled: i128,
    /// Block operation contribution after per-route composition.
    pub operations_scaled: i128,
    /// Block unique-resource contribution.
    pub resources_scaled: i128,
    /// Block-local MPT contribution.
    pub mpt_scaled: i128,
    /// Proposal startup contribution.
    pub startup_scaled: i128,
    /// Proposal transaction-payload contribution.
    pub transaction_payload_scaled: i128,
    /// Proposal blob/source-topology contribution.
    pub blob_source_scaled: i128,
    /// Proposal parent-MPT contribution.
    pub parent_mpt_scaled: i128,
    /// Unrounded fixed-point total for this scope.
    pub total_scaled: i128,
    /// Final integer PGU at block/proposal boundaries; absent for operations.
    pub total_pgu: Option<u128>,
}

/// Fail-closed zk gas pricing error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasError {
    /// No generated rule exists for the requested route.
    UnknownRoute(OperationRoute),
    /// A required selector was absent.
    MissingInput(&'static str),
    /// Typed inputs contradicted one another or the generated route contract.
    InvalidInput(&'static str),
    /// A selector exceeded the authenticated nonnegative bound.
    SelectorOutOfRange {
        /// Selector whose value was rejected.
        selector: &'static str,
        /// Rejected value.
        value: u64,
    },
    /// A checked fixed-point multiplication or addition overflowed `i128`.
    ArithmeticOverflow,
    /// A composed component was negative before boundary rounding.
    NegativeComponent(&'static str),
    /// More terms were supplied than the sealed scope bound permits.
    TooManyTerms {
        /// Number of terms supplied by the caller.
        actual: usize,
        /// Maximum number authenticated by the runtime specification.
        maximum: usize,
    },
}

impl fmt::Display for ZkGasError {
    /// Formats a stable operator-facing description without embedding input payloads.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownRoute(route) => {
                write!(formatter, "unknown zk gas pricing route: {route:?}")
            }
            Self::MissingInput(name) => write!(formatter, "missing zk gas pricing input: {name}"),
            Self::InvalidInput(name) => write!(formatter, "invalid zk gas pricing input: {name}"),
            Self::SelectorOutOfRange { selector, value } => {
                write!(formatter, "zk gas pricing selector exceeds bound: {selector}={value}")
            }
            Self::ArithmeticOverflow => formatter.write_str("zk gas pricing arithmetic overflow"),
            Self::NegativeComponent(name) => {
                write!(formatter, "negative zk gas pricing component: {name}")
            }
            Self::TooManyTerms { actual, maximum } => {
                write!(formatter, "too many zk gas pricing terms: {actual} > {maximum}")
            }
        }
    }
}

impl std::error::Error for ZkGasError {}
