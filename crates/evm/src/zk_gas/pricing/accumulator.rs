//! Transactional collection for block-local zk gas pricing selectors.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256};

use super::{
    CodePreparationSelectors, MemorySelectors, OperationModelInput, OperationPricingInput,
    OperationRoute, OperationRoutePricingInput, ResourceSelectors, TransactionPayloadSelectors,
    ZkGasError, ZkGasModel,
};

/// Per-transaction selectors known before execution begins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransactionMetadata {
    /// Whether the transaction is the mandatory anchor transaction.
    pub is_anchor: bool,
    /// Whether this transaction transfers nonzero native value.
    pub native_value_transfer: bool,
    /// Raw EIP-7702 entries requiring validation, including stale or invalid authorities.
    pub authorization_validations: u64,
    /// Raw non-legacy access-list address items, including duplicates and Anchor entries.
    pub access_list_items: u64,
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
    /// Accepted nonempty code preparations, retained even if an ancestor reverts.
    code_preparation: CodePreparationSelectors,
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
            code_preparation: CodePreparationSelectors::default(),
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
    /// Raw authorization validations across this prefix, without deduplication.
    pub authorization_validations: u64,
    /// Raw non-legacy access-list address items, including duplicates and Anchor entries.
    pub access_list_items: u64,
    /// Actual accepted nonempty code preparations across this prefix.
    pub code_preparation: CodePreparationSelectors,
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
    /// Additive authorization validation count, including ignored entries.
    authorization_validations: u64,
    /// Additive raw access-list address item count, without deduplication.
    access_list_items: u64,
    /// Additive accepted code preparations, counted by occurrence rather than hash.
    code_preparation: CodePreparationSelectors,
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
            let authorization_validations = self
                .authorization_validations
                .checked_add(transaction.metadata.authorization_validations)
                .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?;
            ZkGasModel.price_authorization_validations(authorization_validations)?;
            let access_list_items = self
                .access_list_items
                .checked_add(transaction.metadata.access_list_items)
                .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?;
            ZkGasModel.price_access_list_items(access_list_items)?;
            let code_preparation =
                checked_code_preparation_sum(self.code_preparation, transaction.code_preparation)?;
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
            Ok((
                next_non_anchor,
                next_native_transfers,
                authorization_validations,
                access_list_items,
                code_preparation,
                transaction_payload,
                routes,
                resources,
            ))
        })();
        let Ok((
            next_non_anchor,
            next_native_transfers,
            authorization_validations,
            access_list_items,
            code_preparation,
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
        self.authorization_validations = authorization_validations;
        self.access_list_items = access_list_items;
        self.code_preparation = code_preparation;
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
            authorization_validations: self.authorization_validations,
            access_list_items: self.access_list_items,
            code_preparation: self.code_preparation,
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
        let validation = if metadata.is_anchor && metadata.authorization_validations != 0 {
            Err(ZkGasError::InvalidInput("anchor_authorization_validations"))
        } else {
            ZkGasModel
                .price_authorization_validations(metadata.authorization_validations)
                .and_then(|_| ZkGasModel.price_access_list_items(metadata.access_list_items))
                .map(|_| ())
        };
        if let Err(error) = validation {
            let error = ZkGasAccumulatorError::Pricing(error);
            self.committed.diagnostic.get_or_insert(error);
            self.attempted.diagnostic.get_or_insert(error);
            return Err(error);
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

    /// Records one actual accepted nonempty create output after code preparation.
    ///
    /// Empty output is a no-op. Checked validation precedes both selector updates;
    /// invalid lengths retain a diagnostic without changing EVM execution.
    pub fn observe_code_preparation(
        &mut self,
        byte_length: u64,
    ) -> Result<(), ZkGasAccumulatorError> {
        if byte_length == 0 {
            return Ok(());
        }
        let result = (|| {
            let transaction =
                self.in_flight.as_mut().ok_or(ZkGasAccumulatorError::NoActiveTransaction)?;
            let next = checked_code_preparation_sum(
                transaction.code_preparation,
                CodePreparationSelectors { nonempty_events: 1, total_bytes: byte_length },
            )?;
            transaction.code_preparation = next;
            Ok(())
        })();
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

#[cfg(test)]
mod authorization_tests {
    use super::*;

    #[test]
    fn access_list_count_overflow_keeps_all_prefix_fields_atomic() {
        let mut prefix = SelectorPrefix { access_list_items: u64::MAX, ..Default::default() };
        let mut transaction = TransactionSelectors::new(TransactionMetadata {
            access_list_items: 1,
            authorization_validations: 1,
            ..Default::default()
        });
        transaction.transaction_payload = Some(TransactionPayloadSelectors {
            canonical_transaction_bytes: 100,
            nonempty_payload_transactions: 0,
        });
        prefix.merge_transaction(&transaction);
        assert_eq!(prefix.access_list_items, u64::MAX);
        assert_eq!(prefix.authorization_validations, 0);
        assert_eq!(prefix.non_anchor_transactions, 0);
        assert_eq!(prefix.transaction_payload, TransactionPayloadSelectors::default());
        assert_eq!(prefix.diagnostic, Some(ZkGasAccumulatorError::ArithmeticOverflow));
    }

    #[test]
    fn authorization_count_overflow_keeps_prefix_unchanged_and_diagnostic() {
        let mut prefix =
            SelectorPrefix { authorization_validations: u64::MAX, ..Default::default() };
        let mut transaction = TransactionSelectors::new(TransactionMetadata {
            authorization_validations: 1,
            ..Default::default()
        });
        transaction.transaction_payload = Some(TransactionPayloadSelectors {
            canonical_transaction_bytes: 100,
            nonempty_payload_transactions: 0,
        });
        prefix.merge_transaction(&transaction);
        assert_eq!(prefix.authorization_validations, u64::MAX);
        assert_eq!(prefix.non_anchor_transactions, 0);
        assert_eq!(prefix.transaction_payload, TransactionPayloadSelectors::default());
        assert_eq!(prefix.diagnostic, Some(ZkGasAccumulatorError::ArithmeticOverflow));
    }
}

/// Atomically validates and adds occurrence selectors for transaction and prefix scopes.
fn checked_code_preparation_sum(
    left: CodePreparationSelectors,
    right: CodePreparationSelectors,
) -> Result<CodePreparationSelectors, ZkGasAccumulatorError> {
    ZkGasModel.price_code_preparation(left)?;
    ZkGasModel.price_code_preparation(right)?;
    let result = CodePreparationSelectors {
        nonempty_events: left
            .nonempty_events
            .checked_add(right.nonempty_events)
            .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?,
        total_bytes: left
            .total_bytes
            .checked_add(right.total_bytes)
            .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?,
    };
    ZkGasModel.price_code_preparation(result)?;
    Ok(result)
}

#[cfg(test)]
mod code_preparation_atomic_tests {
    use super::*;
    #[test]
    fn code_preparation_prefix_out_of_domain_does_not_mutate_other_totals() {
        let mut prefix = SelectorPrefix {
            code_preparation: CodePreparationSelectors {
                nonempty_events: 1_000_000_000,
                total_bytes: 1_000_000_000,
            },
            ..Default::default()
        };
        let mut transaction = TransactionSelectors::new(TransactionMetadata {
            native_value_transfer: true,
            ..Default::default()
        });
        transaction.transaction_payload = Some(TransactionPayloadSelectors {
            canonical_transaction_bytes: 100,
            nonempty_payload_transactions: 0,
        });
        transaction.code_preparation =
            CodePreparationSelectors { nonempty_events: 1, total_bytes: 1 };
        transaction.resources.observe(StateAccess::Account(Address::with_last_byte(1))).unwrap();
        prefix.merge_transaction(&transaction);
        assert_eq!(prefix.code_preparation.nonempty_events, 1_000_000_000);
        assert_eq!(prefix.code_preparation.total_bytes, 1_000_000_000);
        assert_eq!(prefix.non_anchor_transactions, 0);
        assert_eq!(prefix.native_value_transfers, 0);
        assert_eq!(prefix.transaction_payload, Default::default());
        assert_eq!(prefix.resources.selectors(), Default::default());
        assert!(prefix.diagnostic.is_some());
    }
    #[test]
    fn code_preparation_invalid_delta_cannot_be_hidden_by_valid_prefix() {
        assert!(
            checked_code_preparation_sum(
                CodePreparationSelectors { nonempty_events: 1, total_bytes: 1 },
                CodePreparationSelectors { nonempty_events: 1, total_bytes: 24577 }
            )
            .is_err()
        );
        assert!(
            checked_code_preparation_sum(
                CodePreparationSelectors { nonempty_events: 1, total_bytes: 32 },
                CodePreparationSelectors { nonempty_events: 0, total_bytes: 1 }
            )
            .is_err()
        );
    }
}
