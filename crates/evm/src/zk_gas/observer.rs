//! Owned, serializable execution events for host-only zk gas tracing.

use std::sync::Arc;

#[cfg(feature = "execution-observer")]
use serde::Serialize;

/// Receives observational execution events without influencing execution results.
pub trait ExecutionObserver: Send + Sync {
    /// Receives one owned event in block execution order.
    fn on_event(&self, event: ExecutionEvent);
}

/// Thread-safe observer shared by the executor and EVM metering adapter.
pub type SharedExecutionObserver = Arc<dyn ExecutionObserver>;

/// Owned execution ledger record emitted by the authoritative executor and zk gas adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "execution-observer", derive(Serialize))]
#[cfg_attr(feature = "execution-observer", serde(rename_all = "snake_case", tag = "event"))]
pub enum ExecutionEvent {
    /// Marks the beginning of a candidate block execution.
    BlockStart {
        /// Zero-based candidate block position in the enclosing trace request.
        block_index: u64,
        /// Executed block number.
        block_number: u64,
        /// Header difficulty expected by Unzen validation, encoded as big-endian bytes.
        expected_difficulty: Option<[u8; 32]>,
        /// Active block zk gas limit.
        block_limit: u64,
        /// Number of recovered transactions supplied to the executor.
        recovered_tx_count: u64,
    },
    /// Marks entry into a block execution phase.
    PhaseStart {
        /// Entered execution phase.
        phase: ExecutionPhase,
    },
    /// Marks completion of a block execution phase.
    PhaseEnd {
        /// Completed execution phase.
        phase: ExecutionPhase,
    },
    /// Opens an attempted transaction buffer.
    TransactionStart {
        /// Zero-based index in the recovered transaction sequence.
        tx_index: u64,
        /// Canonical transaction hash encoded as bytes.
        tx_hash: [u8; 32],
        /// Whether this is the mandatory anchor transaction.
        is_anchor: bool,
        /// Structured class determined at the executor boundary.
        execution_class: TransactionExecutionClass,
    },
    /// Records work that completed before its zk gas charge result is known.
    OperationExecuted {
        /// Block-unique monotonic identifier shared with the linked charge attempt.
        operation_id: u64,
        /// Phase that executed the operation.
        phase: ExecutionPhase,
        /// Enclosing transaction, absent for pre-execution system work.
        tx_index: Option<u64>,
        /// Executed opcode or precompile facts.
        component: OperationComponent,
    },
    /// Records the current-schedule charge selected for work or an intrinsic transaction charge.
    ChargeAttempt {
        /// Linked operation identifier, absent only for transaction intrinsic charges.
        operation_id: Option<u64>,
        /// Phase in which the charge was attempted.
        phase: ExecutionPhase,
        /// Enclosing transaction, absent for pre-execution system work.
        tx_index: Option<u64>,
        /// Charged component and spawn decision when applicable.
        component: ChargeComponent,
        /// Raw gas selected as the current schedule basis, when applicable.
        charge_raw_gas: Option<u64>,
        /// Source of the selected raw gas basis.
        raw_gas_source: RawGasSource,
        /// Effective multiplier selected by the active schedule, when applicable.
        multiplier: Option<u64>,
        /// Checked current-schedule charge requested before the meter outcome.
        requested_current_zkgas: Option<u64>,
        /// Metering result for this charge attempt.
        outcome: ChargeOutcome,
    },
    /// Closes an attempted transaction buffer.
    TransactionEnd {
        /// Zero-based index in the recovered transaction sequence.
        tx_index: u64,
        /// Commit or filter outcome owned by the executor.
        disposition: TransactionDisposition,
        /// Meter total visible while finishing this transaction.
        observed_current_zkgas: u64,
        /// Meter total finalized by committed transactions.
        committed_current_zkgas: u64,
    },
    /// Marks normal completion, truncation, or fatal termination of a block trace.
    BlockStop {
        /// Final block termination reason.
        reason: BlockStopReason,
        /// First transaction that did not begin because zk gas truncation stopped the loop.
        first_unattempted_tx_index: Option<u64>,
    },
    /// Records the finalized committed zk gas total for a successful block.
    BlockEnd {
        /// Finalized zk gas from committed transactions.
        finalized_current_zkgas: u64,
    },
}

/// Executor phase used to separate system work from transaction work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "execution-observer", derive(Serialize))]
#[cfg_attr(feature = "execution-observer", serde(rename_all = "snake_case"))]
pub enum ExecutionPhase {
    /// System calls executed before recovered block transactions.
    PreExecutionSystem,
    /// Recovered block transactions and their EVM work.
    Transactions,
}

/// Structured transaction class used by the host collector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "execution-observer", derive(Serialize))]
#[cfg_attr(feature = "execution-observer", serde(rename_all = "snake_case"))]
pub enum TransactionExecutionClass {
    /// A positive-value transfer to an account with no executable recipient code.
    NativeValueTransfer,
    /// A call that executes recipient contract code.
    ContractCall,
    /// A contract-creation transaction.
    ContractCreate,
    /// A zero-value call to an account without executable code.
    NoCodeNoValue,
    /// Any transaction that does not match a more specific class.
    Other,
}

/// Work facts captured after an opcode or precompile body executed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "execution-observer", derive(Serialize))]
#[cfg_attr(feature = "execution-observer", serde(rename_all = "snake_case", tag = "component"))]
pub enum OperationComponent {
    /// An EVM opcode and its measured interpreter step gas.
    Opcode {
        /// Raw opcode byte.
        opcode: u8,
        /// Interpreter gas consumed by this completed step.
        interpreter_raw_gas: u64,
    },
    /// A precompile and its native gas consumption.
    Precompile {
        /// Full precompile address encoded as bytes.
        address: [u8; 20],
        /// Native precompile gas consumed by the body.
        native_gas: u64,
    },
}

/// Component selected by a zk gas charge attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "execution-observer", derive(Serialize))]
#[cfg_attr(feature = "execution-observer", serde(rename_all = "snake_case", tag = "component"))]
pub enum ChargeComponent {
    /// Fixed per-transaction intrinsic charge.
    TxIntrinsic {
        /// Active schedule intrinsic amount.
        amount: u64,
    },
    /// Opcode charge with the resolved child-frame spawn status.
    Opcode {
        /// Raw opcode byte.
        opcode: u8,
        /// Whether this CALL/CREATE-family opcode opened child work.
        spawned: bool,
    },
    /// Precompile charge selected by full address.
    Precompile {
        /// Full precompile address encoded as bytes.
        address: [u8; 20],
    },
}

/// Basis used by the active schedule to select raw gas for a charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "execution-observer", derive(Serialize))]
#[cfg_attr(feature = "execution-observer", serde(rename_all = "snake_case"))]
pub enum RawGasSource {
    /// Fixed transaction intrinsic schedule value.
    IntrinsicFixed,
    /// Measured opcode interpreter gas delta.
    InterpreterDelta,
    /// Fixed CALL/CREATE child-frame spawn estimate.
    SpawnEstimate,
    /// Native precompile body gas.
    PrecompileNative,
}

/// Checked meter outcome preserved independently from consensus error normalization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "execution-observer", derive(Serialize))]
#[cfg_attr(feature = "execution-observer", serde(rename_all = "snake_case"))]
pub enum ChargeOutcome {
    /// Charge was added to the in-flight transaction total.
    Applied,
    /// Charge would exceed the remaining block budget.
    LimitExceeded,
    /// Checked multiplication or accumulation overflowed `u64`.
    ArithmeticOverflow,
}

/// Executor disposition for an attempted transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "execution-observer", derive(Serialize))]
#[cfg_attr(feature = "execution-observer", serde(rename_all = "snake_case"))]
pub enum TransactionDisposition {
    /// Transaction committed with a successful EVM result.
    CommittedSuccess,
    /// Transaction committed despite an EVM revert.
    CommittedRevert,
    /// Recovered sender was zero and filtering skipped execution.
    FilteredZeroSigner,
    /// Transaction validation failed before EVM execution.
    FilteredInvalid,
    /// Transaction gas limit exceeded the remaining block gas.
    FilteredBlockGasLimit,
    /// Zk gas metering stopped the block before the transaction committed.
    FilteredZkGasLimit,
    /// Non-recoverable execution failure terminated the block.
    Fatal,
}

/// Block trace termination reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "execution-observer", derive(Serialize))]
#[cfg_attr(feature = "execution-observer", serde(rename_all = "snake_case"))]
pub enum BlockStopReason {
    /// Every supplied transaction was processed.
    Complete,
    /// A zk gas limit prevented the remaining tail from starting.
    ZkGasTruncated,
    /// A non-recoverable error terminated execution.
    Fatal,
}
