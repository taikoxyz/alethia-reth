//! Transactional collection for block-local zk gas pricing selectors.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256};

use super::{
    MemorySelectors, OperationModelInput, OperationPricingInput, OperationRoute,
    OperationRoutePricingInput, ResourceSelectors, TransactionPayloadSelectors, ZkGasError,
    ZkGasModel,
};

/// Per-transaction selectors known before execution begins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransactionMetadata {
    /// Whether the transaction is the mandatory anchor transaction.
    pub is_anchor: bool,
    /// Whether this transaction transfers nonzero native value.
    pub native_value_transfer: bool,
}

/// One exact execution resource observed by the EVM runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateAccess {
    /// Account metadata or balance access.
    Account(Address),
    /// Read of one account storage slot.
    StorageRead {
        /// Account whose storage was read.
        address: Address,
        /// Exact storage key.
        slot: U256,
    },
    /// Write of one account storage slot (also counts as an access).
    StorageWrite {
        /// Account whose storage was written.
        address: Address,
        /// Exact storage key.
        slot: U256,
    },
    /// Materialized executable code, deduplicated by code hash.
    Code {
        /// Canonical code hash.
        hash: B256,
        /// Materialized bytecode length.
        byte_length: u64,
    },
}

/// Fail-closed transactional collector error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZkGasAccumulatorError {
    /// An operation could not be priced by the authenticated pricing model.
    Pricing(ZkGasError),
    /// A transaction was started while another transaction remained in flight.
    TransactionAlreadyActive,
    /// An observation or commit occurred without an active transaction.
    NoActiveTransaction,
    /// The signed transaction envelope was unavailable for an included/attempted scope.
    MissingSignedTransactionPayload,
    /// The same transaction scope received signed payload metadata more than once.
    TransactionPayloadAlreadyRecorded,
    /// Checked aggregation overflowed.
    ArithmeticOverflow,
    /// A set size cannot be represented by the public selector type.
    CountOverflow,
    /// The same code hash was reported with inconsistent byte lengths.
    CodeLengthConflict,
}

impl From<ZkGasError> for ZkGasAccumulatorError {
    /// Wraps a pure-model pricing failure as a collector diagnostic.
    fn from(value: ZkGasError) -> Self {
        Self::Pricing(value)
    }
}

impl core::fmt::Display for ZkGasAccumulatorError {
    /// Formats a stable operator-facing description of the retained collector failure.
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Pricing(error) => write!(formatter, "{error}"),
            Self::TransactionAlreadyActive => {
                formatter.write_str("zk gas pricing transaction already active")
            }
            Self::NoActiveTransaction => {
                formatter.write_str("zk gas pricing transaction is not active")
            }
            Self::MissingSignedTransactionPayload => {
                formatter.write_str("signed transaction payload unavailable")
            }
            Self::TransactionPayloadAlreadyRecorded => {
                formatter.write_str("signed transaction payload already recorded")
            }
            Self::ArithmeticOverflow => formatter.write_str("zk gas pricing accumulator overflow"),
            Self::CountOverflow => formatter.write_str("zk gas pricing resource count overflow"),
            Self::CodeLengthConflict => {
                formatter.write_str("zk gas pricing code hash length conflict")
            }
        }
    }
}

impl std::error::Error for ZkGasAccumulatorError {}

#[derive(Debug, Default)]
/// Exact unique-resource set for one transaction or committed block prefix.
struct ResourceSet {
    /// Account addresses accessed by execution or validation.
    accounts: BTreeSet<Address>,
    /// Account-and-slot pairs read or written.
    storage_accesses: BTreeSet<(Address, U256)>,
    /// Account-and-slot pairs with an attempted write.
    storage_writes: BTreeSet<(Address, U256)>,
    /// Materialized code byte lengths keyed by code hash.
    code: BTreeMap<B256, u64>,
    /// Checked number of unique accounts.
    unique_account_count: u64,
    /// Checked number of unique storage slots accessed.
    unique_storage_slot_access_count: u64,
    /// Checked number of unique storage slots written.
    unique_storage_slot_write_count: u64,
    /// Checked number of unique nonempty code hashes.
    unique_code_hash_count: u64,
    /// Checked total bytes across unique nonempty code hashes.
    unique_code_bytes: u64,
}

impl ResourceSet {
    /// Inserts one exact resource observation.
    fn observe(&mut self, access: StateAccess) -> Result<(), ZkGasAccumulatorError> {
        match access {
            StateAccess::Account(address) => {
                let next = self
                    .unique_account_count
                    .checked_add(u64::from(!self.accounts.contains(&address)));
                let next = next.ok_or(ZkGasAccumulatorError::CountOverflow)?;
                self.accounts.insert(address);
                self.unique_account_count = next;
            }
            StateAccess::StorageRead { address, slot } => {
                let next_accounts = self
                    .unique_account_count
                    .checked_add(u64::from(!self.accounts.contains(&address)))
                    .ok_or(ZkGasAccumulatorError::CountOverflow)?;
                let next_accesses = self
                    .unique_storage_slot_access_count
                    .checked_add(u64::from(!self.storage_accesses.contains(&(address, slot))))
                    .ok_or(ZkGasAccumulatorError::CountOverflow)?;
                self.accounts.insert(address);
                self.storage_accesses.insert((address, slot));
                self.unique_account_count = next_accounts;
                self.unique_storage_slot_access_count = next_accesses;
            }
            StateAccess::StorageWrite { address, slot } => {
                let next_accounts = self
                    .unique_account_count
                    .checked_add(u64::from(!self.accounts.contains(&address)))
                    .ok_or(ZkGasAccumulatorError::CountOverflow)?;
                let next_accesses = self
                    .unique_storage_slot_access_count
                    .checked_add(u64::from(!self.storage_accesses.contains(&(address, slot))))
                    .ok_or(ZkGasAccumulatorError::CountOverflow)?;
                let next_writes = self
                    .unique_storage_slot_write_count
                    .checked_add(u64::from(!self.storage_writes.contains(&(address, slot))))
                    .ok_or(ZkGasAccumulatorError::CountOverflow)?;
                self.accounts.insert(address);
                self.storage_accesses.insert((address, slot));
                self.storage_writes.insert((address, slot));
                self.unique_account_count = next_accounts;
                self.unique_storage_slot_access_count = next_accesses;
                self.unique_storage_slot_write_count = next_writes;
            }
            StateAccess::Code { hash, byte_length } => match self.code.get(&hash) {
                _ if byte_length == 0 => {}
                Some(previous) if *previous != byte_length => {
                    return Err(ZkGasAccumulatorError::CodeLengthConflict);
                }
                Some(_) => {}
                None => {
                    let next_hashes = self
                        .unique_code_hash_count
                        .checked_add(1)
                        .ok_or(ZkGasAccumulatorError::CountOverflow)?;
                    let next_bytes = self
                        .unique_code_bytes
                        .checked_add(byte_length)
                        .ok_or(ZkGasAccumulatorError::CountOverflow)?;
                    self.code.insert(hash, byte_length);
                    self.unique_code_hash_count = next_hashes;
                    self.unique_code_bytes = next_bytes;
                }
            },
        }
        Ok(())
    }

    /// Validates a transaction delta against the committed prefix without copying either set.
    fn prevalidate_merge(&self, other: &Self) -> Result<ResourceSelectors, ZkGasAccumulatorError> {
        let added_accounts = u64::try_from(
            other.accounts.iter().filter(|address| !self.accounts.contains(*address)).count(),
        )
        .map_err(|_| ZkGasAccumulatorError::CountOverflow)?;
        let added_accesses = u64::try_from(
            other
                .storage_accesses
                .iter()
                .filter(|slot| !self.storage_accesses.contains(slot))
                .count(),
        )
        .map_err(|_| ZkGasAccumulatorError::CountOverflow)?;
        let added_writes = u64::try_from(
            other.storage_writes.iter().filter(|slot| !self.storage_writes.contains(slot)).count(),
        )
        .map_err(|_| ZkGasAccumulatorError::CountOverflow)?;
        let mut added_hashes = 0_u64;
        let mut added_bytes = 0_u64;
        for (hash, byte_length) in &other.code {
            match self.code.get(hash) {
                Some(previous) if previous != byte_length => {
                    return Err(ZkGasAccumulatorError::CodeLengthConflict);
                }
                Some(_) => {}
                None => {
                    added_hashes =
                        added_hashes.checked_add(1).ok_or(ZkGasAccumulatorError::CountOverflow)?;
                    added_bytes = added_bytes
                        .checked_add(*byte_length)
                        .ok_or(ZkGasAccumulatorError::CountOverflow)?;
                }
            }
        }
        Ok(ResourceSelectors {
            unique_account_count: self
                .unique_account_count
                .checked_add(added_accounts)
                .ok_or(ZkGasAccumulatorError::CountOverflow)?,
            unique_storage_slot_access_count: self
                .unique_storage_slot_access_count
                .checked_add(added_accesses)
                .ok_or(ZkGasAccumulatorError::CountOverflow)?,
            unique_storage_slot_write_count: self
                .unique_storage_slot_write_count
                .checked_add(added_writes)
                .ok_or(ZkGasAccumulatorError::CountOverflow)?,
            unique_code_hash_count: self
                .unique_code_hash_count
                .checked_add(added_hashes)
                .ok_or(ZkGasAccumulatorError::CountOverflow)?,
            unique_code_bytes: self
                .unique_code_bytes
                .checked_add(added_bytes)
                .ok_or(ZkGasAccumulatorError::CountOverflow)?,
        })
    }

    /// Merges a transaction delta after [`Self::prevalidate_merge`] succeeds.
    fn merge_prevalidated(&mut self, other: &ResourceSet, totals: ResourceSelectors) {
        self.accounts.extend(other.accounts.iter().copied());
        self.storage_accesses.extend(other.storage_accesses.iter().copied());
        self.storage_writes.extend(other.storage_writes.iter().copied());
        for (&hash, &byte_length) in &other.code {
            self.code.entry(hash).or_insert(byte_length);
        }
        self.unique_account_count = totals.unique_account_count;
        self.unique_storage_slot_access_count = totals.unique_storage_slot_access_count;
        self.unique_storage_slot_write_count = totals.unique_storage_slot_write_count;
        self.unique_code_hash_count = totals.unique_code_hash_count;
        self.unique_code_bytes = totals.unique_code_bytes;
    }

    /// Returns the checked selectors maintained alongside the exact sets.
    const fn selectors(&self) -> ResourceSelectors {
        ResourceSelectors {
            unique_account_count: self.unique_account_count,
            unique_storage_slot_access_count: self.unique_storage_slot_access_count,
            unique_storage_slot_write_count: self.unique_storage_slot_write_count,
            unique_code_hash_count: self.unique_code_hash_count,
            unique_code_bytes: self.unique_code_bytes,
        }
    }
}

#[derive(Debug)]
/// In-flight selectors isolated until the transaction is included.
struct TransactionSelectors {
    /// Transaction-wide selector metadata.
    metadata: TransactionMetadata,
    /// Signed envelope metadata, populated by the outer block pipeline.
    transaction_payload: Option<TransactionPayloadSelectors>,
    /// Priced operation components keyed by canonical route.
    operation_routes: BTreeMap<RouteKey, OperationRoutePricingInput>,
    /// Exact resources touched by the transaction.
    resources: ResourceSet,
    /// First observation failure that makes this quote untrusted.
    diagnostic: Option<ZkGasAccumulatorError>,
    /// Whether execution emitted any opcode, spawn, or precompile operation event.
    has_operation_trace: bool,
}

impl TransactionSelectors {
    /// Creates an empty transaction scope.
    fn new(metadata: TransactionMetadata) -> Self {
        Self {
            metadata,
            transaction_payload: None,
            operation_routes: BTreeMap::new(),
            resources: ResourceSet::default(),
            diagnostic: None,
            has_operation_trace: false,
        }
    }
}

/// Sortable representation of the authenticated operation namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RouteKey {
    /// Ordinary opcode route.
    Opcode(u8),
    /// Direct-precompile route.
    Precompile(u16),
    /// Spawn wrapper confirmed by frame dispatch.
    ConfirmedSpawn(u8),
}

impl From<OperationRoute> for RouteKey {
    /// Converts a public authenticated route into its sortable accumulator key.
    fn from(route: OperationRoute) -> Self {
        match route {
            OperationRoute::Opcode(opcode) => Self::Opcode(opcode),
            OperationRoute::Precompile(address) => Self::Precompile(address),
            OperationRoute::ConfirmedSpawn(opcode) => Self::ConfirmedSpawn(opcode),
        }
    }
}

/// Owned committed block-local selector snapshot.
///
/// Runtime execution snapshots deliberately cannot be converted into complete block pricing
/// inputs until the production state-root pipeline supplies an exact MPT snapshot.
///
/// ```compile_fail
/// use alethia_reth_evm::zk_gas::pricing::ZkGasBlockSnapshot;
/// let snapshot = ZkGasBlockSnapshot::default();
/// let complete_input = snapshot.block_input();
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZkGasBlockSnapshot {
    /// Non-anchor transaction count in this prefix.
    pub started_non_anchor_transactions: u64,
    /// Native value-transfer count in this prefix.
    pub native_value_transfers: u64,
    /// Canonical payload of all included signed transactions.
    pub transaction_payload: TransactionPayloadSelectors,
    /// Accumulated operation candidates in canonical route order.
    pub operation_routes: Vec<OperationRoutePricingInput>,
    /// Unique-resource selectors in this prefix.
    pub resources: ResourceSelectors,
    /// First diagnostic retained in this prefix, if any.
    ///
    /// Selectors remain available for shadow-mode analysis, but a snapshot with a diagnostic is
    /// not a trustworthy admission quote.
    pub diagnostic: Option<ZkGasAccumulatorError>,
}

/// Owned block-local selector snapshot for every transaction whose EVM pricing scope began.
///
/// The scope begins before revm transaction/header validation. Consequently, validation failures
/// such as an invalid nonce or insufficient balance are counted, and their declared access-list
/// accounts and slots are retained. Executor prechecks performed before `transact_raw` are not
/// counted; these currently include a native gas limit above the remaining block gas and an
/// intrinsic-zk-gas charge above the remaining zk gas budget. A transaction discarded after its
/// scope begins remains attempted, including a transaction rejected by the commit predicate or
/// truncated by the zk gas limit. For a truncation, completed operation steps and resources from
/// the finalized journal are retained, but the operation whose meter charge first exceeds the
/// budget is not itself pricing-observed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZkGasAttemptedBlockSnapshot {
    /// Number of attempted transactions, including the anchor transaction.
    pub transaction_count: u64,
    /// Aggregated selectors across all attempted transaction scopes.
    pub block: ZkGasBlockSnapshot,
}

#[derive(Debug, Default)]
/// One block-prefix aggregate shared by committed and attempted observations.
struct SelectorPrefix {
    /// Non-anchor transaction count.
    non_anchor_transactions: u64,
    /// Nonzero native-value transfer count.
    native_value_transfers: u64,
    /// Additive canonical signed payload across this prefix.
    transaction_payload: TransactionPayloadSelectors,
    /// Operation components aggregated by route.
    operation_routes: BTreeMap<RouteKey, OperationRoutePricingInput>,
    /// Block-global unique execution resources.
    resources: ResourceSet,
    /// First diagnostic included in this prefix.
    diagnostic: Option<ZkGasAccumulatorError>,
}

impl SelectorPrefix {
    /// Merges one transaction into this prefix after validating the complete delta.
    fn merge_transaction(&mut self, transaction: &TransactionSelectors) {
        if self.diagnostic.is_none() {
            self.diagnostic = transaction.diagnostic;
        }
        let payload = transaction.transaction_payload.unwrap_or_else(|| {
            self.diagnostic.get_or_insert(ZkGasAccumulatorError::MissingSignedTransactionPayload);
            TransactionPayloadSelectors::default()
        });
        let non_anchor = u64::from(!transaction.metadata.is_anchor);
        let native_transfer = u64::from(transaction.metadata.native_value_transfer);
        let prevalidated = (|| {
            let next_non_anchor = self
                .non_anchor_transactions
                .checked_add(non_anchor)
                .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?;
            let next_native_transfers = self
                .native_value_transfers
                .checked_add(native_transfer)
                .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?;
            let transaction_payload = TransactionPayloadSelectors {
                canonical_transaction_bytes: self
                    .transaction_payload
                    .canonical_transaction_bytes
                    .checked_add(payload.canonical_transaction_bytes)
                    .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?,
                nonempty_payload_transactions: self
                    .transaction_payload
                    .nonempty_payload_transactions
                    .checked_add(payload.nonempty_payload_transactions)
                    .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?,
            };
            ZkGasModel.price_transaction_payload(transaction_payload)?;
            let mut routes = Vec::with_capacity(transaction.operation_routes.len());
            for (key, row) in &transaction.operation_routes {
                routes.push((*key, merged_route(self.operation_routes.get(key), *row)?));
            }
            let resources = self.resources.prevalidate_merge(&transaction.resources)?;
            Ok((next_non_anchor, next_native_transfers, transaction_payload, routes, resources))
        })();
        let Ok((
            next_non_anchor,
            next_native_transfers,
            transaction_payload,
            routes,
            resource_totals,
        )) = prevalidated
        else {
            self.diagnostic.get_or_insert(prevalidated.unwrap_err());
            return;
        };
        self.non_anchor_transactions = next_non_anchor;
        self.native_value_transfers = next_native_transfers;
        self.transaction_payload = transaction_payload;
        for (key, row) in routes {
            self.operation_routes.insert(key, row);
        }
        self.resources.merge_prevalidated(&transaction.resources, resource_totals);
    }

    /// Returns an owned public snapshot of this prefix.
    fn snapshot(&self) -> ZkGasBlockSnapshot {
        ZkGasBlockSnapshot {
            started_non_anchor_transactions: self.non_anchor_transactions,
            native_value_transfers: self.native_value_transfers,
            transaction_payload: self.transaction_payload,
            operation_routes: self.operation_routes.values().copied().collect(),
            resources: self.resources.selectors(),
            diagnostic: self.diagnostic,
        }
    }
}

/// Selected wrapper facts retained only until frame dispatch authenticates the event.
#[derive(Debug)]
struct PendingSpawn {
    /// CALL/CREATE-family opcode that selected a new frame.
    opcode: u8,
    /// Parent reservation delta for CALL-family wrappers; absent for CREATE/CREATE2.
    memory: Option<MemorySelectors>,
}

/// Transactional pricing collector with committed and attempted block prefixes.
///
/// In-flight selectors are invisible in snapshots until the scope is either committed or reset.
/// Commit adds the scope to both prefixes; reset retains it only in the attempted prefix.
///
/// The collector intentionally does not implement `Clone`: transaction commit must prevalidate
/// and merge only the incoming delta, never copy the full committed resource prefix.
///
/// ```compile_fail
/// use alethia_reth_evm::zk_gas::pricing::ZkGasAccumulator;
/// let accumulator = ZkGasAccumulator::default();
/// let duplicated = accumulator.clone();
/// ```
#[derive(Debug, Default)]
pub struct ZkGasAccumulator {
    /// Selectors from included transactions only.
    committed: SelectorPrefix,
    /// Selectors from every transaction whose pricing scope began.
    attempted: SelectorPrefix,
    /// Number of attempted transaction scopes, including the anchor transaction.
    attempted_transaction_count: u64,
    /// Transaction scope currently being executed.
    in_flight: Option<TransactionSelectors>,
    /// Spawn opcode selected by the interpreter but not yet dispatched.
    pending_spawn: Option<PendingSpawn>,
}

impl ZkGasAccumulator {
    /// Starts a fresh transaction selector scope.
    pub fn begin_transaction(
        &mut self,
        metadata: TransactionMetadata,
    ) -> Result<(), ZkGasAccumulatorError> {
        if self.in_flight.is_some() {
            return Err(ZkGasAccumulatorError::TransactionAlreadyActive);
        }
        self.pending_spawn = None;
        self.in_flight = Some(TransactionSelectors::new(metadata));
        Ok(())
    }

    /// Attaches the original signed envelope to the active execution scope exactly once.
    ///
    /// Call after EVM execution opens the scope and before commit/reset. Validation failures
    /// remain attempted; missing metadata becomes a diagnostic rather than a zero payload quote.
    pub fn observe_transaction_payload(
        &mut self,
        payload: TransactionPayloadSelectors,
    ) -> Result<(), ZkGasAccumulatorError> {
        if payload.canonical_transaction_bytes == 0 || payload.nonempty_payload_transactions > 1 {
            return Err(ZkGasAccumulatorError::Pricing(ZkGasError::InvalidInput(
                "transaction_payload",
            )));
        }
        ZkGasModel.price_transaction_payload(payload)?;
        let scope = self.in_flight.as_mut().ok_or(ZkGasAccumulatorError::NoActiveTransaction)?;
        if scope.transaction_payload.is_some() {
            return Err(ZkGasAccumulatorError::TransactionPayloadAlreadyRecorded);
        }
        scope.transaction_payload = Some(payload);
        Ok(())
    }

    /// Prices and stages one completed opcode or confirmed spawn event.
    pub fn observe_step(
        &mut self,
        input: OperationPricingInput,
    ) -> Result<(), ZkGasAccumulatorError> {
        if matches!(input.route, OperationRoute::Precompile(_)) {
            return Err(ZkGasAccumulatorError::Pricing(ZkGasError::InvalidInput(
                "observe_step_route",
            )));
        }
        self.pending_spawn = None;
        let result = self.observe_operation(input);
        self.retain_diagnostic(result.err());
        result
    }

    /// Prices and stages one direct-precompile event.
    pub fn observe_precompile(
        &mut self,
        address: u16,
        native_gas: u64,
        work_units: Option<u64>,
    ) -> Result<(), ZkGasAccumulatorError> {
        self.pending_spawn = None;
        let result = self.observe_operation(OperationPricingInput {
            route: OperationRoute::Precompile(address),
            interpreter_raw_gas: None,
            native_gas: Some(native_gas),
            model_input: work_units.map_or(OperationModelInput::None, |work_units| {
                OperationModelInput::WorkUnits { work_units }
            }),
            feature_error: false,
        });
        self.retain_diagnostic(result.err());
        result
    }

    /// Stages one exact execution-resource observation.
    pub fn observe_state_access(
        &mut self,
        access: StateAccess,
    ) -> Result<(), ZkGasAccumulatorError> {
        let result = self
            .in_flight
            .as_mut()
            .ok_or(ZkGasAccumulatorError::NoActiveTransaction)?
            .resources
            .observe(access);
        self.retain_diagnostic(result.err());
        result
    }

    /// Stages a spawn wrapper selected by the interpreter until frame dispatch confirms it.
    pub(crate) fn select_spawn(
        &mut self,
        opcode: u8,
        memory: Option<MemorySelectors>,
    ) -> Result<(), ZkGasAccumulatorError> {
        let transaction =
            self.in_flight.as_mut().ok_or(ZkGasAccumulatorError::NoActiveTransaction)?;
        transaction.has_operation_trace = true;
        self.pending_spawn = Some(PendingSpawn { opcode, memory });
        Ok(())
    }

    /// Finalizes the calibrated native-transfer marker after execution disposition is known.
    pub(crate) fn finalize_native_value_transfer(
        &mut self,
        eligible: bool,
    ) -> Result<(), ZkGasAccumulatorError> {
        let transaction =
            self.in_flight.as_mut().ok_or(ZkGasAccumulatorError::NoActiveTransaction)?;
        transaction.metadata.native_value_transfer = eligible && !transaction.has_operation_trace;
        Ok(())
    }

    /// Converts the last selected spawn wrapper into its authenticated confirmed route.
    pub(crate) fn confirm_spawn(&mut self) -> Result<(), ZkGasAccumulatorError> {
        let Some(pending) = self.pending_spawn.take() else { return Ok(()) };
        let result = self.observe_operation(OperationPricingInput {
            route: OperationRoute::ConfirmedSpawn(pending.opcode),
            interpreter_raw_gas: None,
            native_gas: None,
            model_input: pending.memory.map_or(OperationModelInput::None, |memory| {
                OperationModelInput::SpawnMemory { memory }
            }),
            feature_error: false,
        });
        self.retain_diagnostic(result.err());
        result
    }

    /// Atomically merges the in-flight selector scope into the block snapshot.
    pub fn commit_transaction(&mut self) -> Result<(), ZkGasAccumulatorError> {
        let Some(transaction) = self.in_flight.take() else {
            self.committed.diagnostic.get_or_insert(ZkGasAccumulatorError::NoActiveTransaction);
            self.attempted.diagnostic.get_or_insert(ZkGasAccumulatorError::NoActiveTransaction);
            return Ok(());
        };
        self.pending_spawn = None;
        self.record_attempt(&transaction);
        self.committed.merge_transaction(&transaction);
        Ok(())
    }

    /// Retains an attempted observation and clears its in-flight selector scope.
    pub fn reset_transaction(&mut self) {
        if let Some(transaction) = self.in_flight.take() {
            self.record_attempt(&transaction);
        }
        self.pending_spawn = None;
    }

    /// Returns an owned committed block snapshot; in-flight data is excluded.
    pub fn committed_snapshot(&self) -> ZkGasBlockSnapshot {
        self.committed.snapshot()
    }

    /// Returns selectors from every transaction whose EVM pricing scope began.
    pub fn attempted_snapshot(&self) -> ZkGasAttemptedBlockSnapshot {
        ZkGasAttemptedBlockSnapshot {
            transaction_count: self.attempted_transaction_count,
            block: self.attempted.snapshot(),
        }
    }

    /// Returns the first diagnostic affecting the current block quote.
    pub const fn diagnostic(&self) -> Option<ZkGasAccumulatorError> {
        match &self.in_flight {
            Some(transaction) if transaction.diagnostic.is_some() => transaction.diagnostic,
            _ => self.committed.diagnostic,
        }
    }

    /// Retains the first typed observer diagnostic without changing EVM execution.
    pub(crate) fn retain_diagnostic(&mut self, error: Option<ZkGasAccumulatorError>) {
        if let Some(error) = error &&
            let Some(transaction) = &mut self.in_flight
        {
            transaction.diagnostic.get_or_insert(error);
        }
    }

    /// Prices and aggregates one operation into the current transaction scope.
    fn observe_operation(
        &mut self,
        input: OperationPricingInput,
    ) -> Result<(), ZkGasAccumulatorError> {
        let transaction =
            self.in_flight.as_mut().ok_or(ZkGasAccumulatorError::NoActiveTransaction)?;
        transaction.has_operation_trace = true;
        let priced = ZkGasModel.price_operation(&input)?;
        let row = OperationRoutePricingInput {
            route: input.route,
            accepted_absolute_scaled: priced.accepted_absolute_scaled,
            incremental_scaled: priced.incremental_scaled,
            structural_scaled: priced.structural_scaled,
            feature_error_scaled: priced.feature_error_scaled,
        };
        merge_route(&mut transaction.operation_routes, input.route.into(), row)
    }

    /// Records one completed or discarded in-flight scope in the attempted prefix.
    fn record_attempt(&mut self, transaction: &TransactionSelectors) {
        let Some(next_count) = self.attempted_transaction_count.checked_add(1) else {
            self.attempted.diagnostic.get_or_insert(ZkGasAccumulatorError::ArithmeticOverflow);
            return;
        };
        self.attempted.merge_transaction(transaction);
        self.attempted_transaction_count = next_count;
    }
}

/// Computes the post-merge route row without mutating the committed prefix.
fn merged_route(
    existing: Option<&OperationRoutePricingInput>,
    row: OperationRoutePricingInput,
) -> Result<OperationRoutePricingInput, ZkGasAccumulatorError> {
    let Some(existing) = existing else { return Ok(row) };
    if existing.route != row.route {
        return Err(ZkGasAccumulatorError::Pricing(ZkGasError::InvalidInput("operation_route")));
    }
    Ok(OperationRoutePricingInput {
        route: row.route,
        accepted_absolute_scaled: existing
            .accepted_absolute_scaled
            .checked_add(row.accepted_absolute_scaled)
            .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?,
        incremental_scaled: existing
            .incremental_scaled
            .checked_add(row.incremental_scaled)
            .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?,
        structural_scaled: existing
            .structural_scaled
            .checked_add(row.structural_scaled)
            .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?,
        feature_error_scaled: existing
            .feature_error_scaled
            .checked_add(row.feature_error_scaled)
            .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?,
    })
}

/// Checked component-wise aggregation for one canonical operation route.
fn merge_route(
    routes: &mut BTreeMap<RouteKey, OperationRoutePricingInput>,
    key: RouteKey,
    row: OperationRoutePricingInput,
) -> Result<(), ZkGasAccumulatorError> {
    let accumulated = routes.entry(key).or_insert(OperationRoutePricingInput {
        route: row.route,
        accepted_absolute_scaled: 0,
        incremental_scaled: 0,
        structural_scaled: 0,
        feature_error_scaled: 0,
    });
    if accumulated.route != row.route {
        return Err(ZkGasAccumulatorError::Pricing(ZkGasError::InvalidInput("operation_route")));
    }
    accumulated.accepted_absolute_scaled = accumulated
        .accepted_absolute_scaled
        .checked_add(row.accepted_absolute_scaled)
        .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?;
    accumulated.incremental_scaled = accumulated
        .incremental_scaled
        .checked_add(row.incremental_scaled)
        .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?;
    accumulated.structural_scaled = accumulated
        .structural_scaled
        .checked_add(row.structural_scaled)
        .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?;
    accumulated.feature_error_scaled = accumulated
        .feature_error_scaled
        .checked_add(row.feature_error_scaled)
        .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?;
    Ok(())
}
