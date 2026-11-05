//! Transactional collection for block-local zk gas pricing selectors.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256};

use super::storage_journal::StorageJournalLedger;

use super::{
    AccessClass, CodePreparationSelectors, MemorySelectors, OperationModelInput,
    OperationPricingInput, OperationRoute, OperationRoutePricingInput, ResourceSelectors,
    StorageJournalDiagnostics, TransactionPayloadSelectors, ZkGasError, ZkGasModel,
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
    /// Potential journal observation, including independently priced comparison work for this scope.
    storage_journal: StorageJournalLedger,
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
            storage_journal: StorageJournalLedger::default(),
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
    /// Potential journal work; incomplete observations prevent publication of a valid subtotal.
    pub storage_journal: StorageJournalDiagnostics,
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
/// budget is not itself pricing-observed. Its captured ordinary-storage journal work is retained
/// independently before the legacy charge rejects it.
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
    /// Finalized storage observation, including priced comparison units across the prefix.
    storage_journal: StorageJournalDiagnostics,
    /// First diagnostic included in this prefix.
    diagnostic: Option<ZkGasAccumulatorError>,
}

impl SelectorPrefix {
    /// Merges one transaction into this prefix after validating the complete delta.
    fn merge_transaction(&mut self, transaction: &TransactionSelectors) {
        self.storage_journal.merge(transaction.storage_journal.diagnostics);
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
            ZkGasModel
                .price_storage_journal_work(self.storage_journal.comparison_work_units_upper)?;
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
            storage_journal: self.storage_journal,
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

    /// Records potential ordinary-storage journal work independently of operation-route pricing.
    ///
    /// This shared runtime hook is intentionally infallible: missing metadata and overflow only
    /// mark the journal diagnostics incomplete, preventing a valid subtotal. Outside an active
    /// scope there is no work to retain.
    pub(crate) fn observe_storage_journal(
        &mut self,
        owner: Address,
        occupancy: Option<usize>,
        access: AccessClass,
        store: bool,
    ) {
        if let Some(transaction) = &mut self.in_flight {
            transaction.storage_journal.observe(owner, occupancy, access, store);
        }
    }

    /// Retains an initial physical slot or a potential uncached materialization event.
    ///
    /// Existing cached-presence facts select these events; this does not perform a full-key
    /// lookup. Failed attempts or clear/refill may conservatively count the same key again.
    pub(crate) fn observe_storage_materialization(&mut self, owner: Address, key: U256) {
        if let Some(transaction) = &mut self.in_flight {
            transaction.storage_journal.observe_materialization(owner, key);
        }
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
        let Some(mut transaction) = self.in_flight.take() else {
            self.committed.diagnostic.get_or_insert(ZkGasAccumulatorError::NoActiveTransaction);
            self.attempted.diagnostic.get_or_insert(ZkGasAccumulatorError::NoActiveTransaction);
            return Ok(());
        };
        self.pending_spawn = None;
        transaction.storage_journal.finish();
        self.record_attempt(&transaction);
        self.committed.merge_transaction(&transaction);
        Ok(())
    }

    /// Retains an attempted observation and clears its in-flight selector scope.
    pub fn reset_transaction(&mut self) {
        if let Some(mut transaction) = self.in_flight.take() {
            transaction.storage_journal.finish();
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
            normal_dispatch_count: priced.normal_dispatch_count,
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
        normal_dispatch_count: existing
            .normal_dispatch_count
            .checked_add(row.normal_dispatch_count)
            .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?,
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
    // Prevalidate every component before publishing the row, including the new raw count.
    let next = merged_route(routes.get(&key), row)?;
    routes.insert(key, next);
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

#[cfg(test)]
mod normal_dispatch_tests {
    use super::*;

    fn operation() -> OperationPricingInput {
        OperationPricingInput {
            route: OperationRoute::Opcode(1),
            interpreter_raw_gas: Some(3),
            native_gas: None,
            model_input: super::super::OperationModelInput::StaticRawGas { raw_gas: 3 },
            feature_error: false,
        }
    }

    fn payload(accumulator: &mut ZkGasAccumulator) {
        accumulator
            .observe_transaction_payload(TransactionPayloadSelectors {
                canonical_transaction_bytes: 100,
                nonempty_payload_transactions: 0,
            })
            .unwrap();
    }

    #[test]
    fn normal_dispatch_count_overflow_leaves_operation_and_block_prefix_atomic() {
        let row = OperationRoutePricingInput {
            route: operation().route,
            normal_dispatch_count: u64::MAX,
            accepted_absolute_scaled: 10,
            incremental_scaled: 0,
            structural_scaled: 0,
            feature_error_scaled: 0,
        };
        let mut routes = BTreeMap::new();
        routes.insert(row.route.into(), row);
        let before = routes.clone();
        let next = OperationRoutePricingInput { normal_dispatch_count: 1, ..row };
        assert_eq!(
            merge_route(&mut routes, row.route.into(), next),
            Err(ZkGasAccumulatorError::ArithmeticOverflow)
        );
        assert_eq!(routes, before);
        let mut prefix = SelectorPrefix { operation_routes: routes, ..Default::default() };
        let mut tx = TransactionSelectors::new(TransactionMetadata::default());
        tx.transaction_payload = Some(TransactionPayloadSelectors {
            canonical_transaction_bytes: 100,
            nonempty_payload_transactions: 0,
        });
        tx.operation_routes.insert(next.route.into(), next);
        prefix.merge_transaction(&tx);
        assert_eq!(prefix.operation_routes, before);
        assert_eq!(prefix.non_anchor_transactions, 0);
        assert_eq!(prefix.transaction_payload, TransactionPayloadSelectors::default());
        assert_eq!(prefix.diagnostic, Some(ZkGasAccumulatorError::ArithmeticOverflow));
        let mut accumulator = ZkGasAccumulator::default();
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        accumulator.in_flight.as_mut().unwrap().operation_routes = before.clone();
        assert_eq!(
            accumulator.observe_step(operation()),
            Err(ZkGasAccumulatorError::ArithmeticOverflow)
        );
        assert_eq!(accumulator.in_flight.as_ref().unwrap().operation_routes, before);
    }

    #[test]
    fn normal_dispatch_discard_reset_and_pending_spawn_do_not_leak_or_double_count() {
        let mut accumulator = ZkGasAccumulator::default();
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        payload(&mut accumulator);
        accumulator.observe_step(operation()).unwrap();
        accumulator.reset_transaction();
        assert!(accumulator.committed_snapshot().operation_routes.is_empty());
        assert_eq!(
            accumulator.attempted_snapshot().block.operation_routes[0].normal_dispatch_count,
            1
        );
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        payload(&mut accumulator);
        accumulator.select_spawn(0xf4, Some(MemorySelectors::default())).unwrap();
        accumulator.commit_transaction().unwrap();
        assert!(accumulator.committed_snapshot().operation_routes.is_empty());
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        payload(&mut accumulator);
        accumulator.select_spawn(0xf4, Some(MemorySelectors::default())).unwrap();
        accumulator.confirm_spawn().unwrap();
        accumulator.confirm_spawn().unwrap();
        accumulator.commit_transaction().unwrap();
        assert_eq!(accumulator.committed_snapshot().operation_routes[0].normal_dispatch_count, 1);
        assert_eq!(
            accumulator
                .attempted_snapshot()
                .block
                .operation_routes
                .iter()
                .map(|r| r.normal_dispatch_count)
                .sum::<u64>(),
            2
        );
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        payload(&mut accumulator);
        accumulator.observe_step(operation()).unwrap();
        accumulator.commit_transaction().unwrap();
        assert_eq!(
            accumulator
                .committed_snapshot()
                .operation_routes
                .iter()
                .map(|r| r.normal_dispatch_count)
                .sum::<u64>(),
            2
        );
    }
}

#[cfg(test)]
mod storage_journal_tests {
    use super::*;
    use crate::zk_gas::pricing::AccessClass;

    fn begin(accumulator: &mut ZkGasAccumulator) {
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        accumulator
            .observe_transaction_payload(TransactionPayloadSelectors {
                canonical_transaction_bytes: 1,
                nonempty_payload_transactions: 0,
            })
            .unwrap();
    }

    #[test]
    fn storage_journal_direct_units_and_attempted_scope_reset() {
        let owner = Address::repeat_byte(7);
        let mut accumulator = ZkGasAccumulator::default();
        begin(&mut accumulator);
        for n in 0..512 {
            accumulator.observe_storage_journal(owner, Some(n), AccessClass::Cold, false);
        }
        accumulator.observe_storage_journal(owner, Some(512), AccessClass::Warm, false);
        accumulator.commit_transaction().unwrap();
        let diagnostic = accumulator.committed_snapshot().storage_journal;
        assert_eq!(diagnostic.potential_sload_count, 513);
        assert_eq!(diagnostic.direct_lookup_units, 131841);
        assert_eq!(diagnostic.rollback_lookup_units_upper, 513 * 512);
        assert_eq!(diagnostic.max_occupancy_before, 512);
        assert_eq!(diagnostic.high_water_upper, 513);
        assert_eq!(diagnostic.incomplete, None);

        begin(&mut accumulator);
        accumulator.observe_storage_journal(owner, Some(0), AccessClass::Cold, false);
        accumulator.reset_transaction();
        assert_eq!(accumulator.committed_snapshot().storage_journal, diagnostic);
        let attempted = accumulator.attempted_snapshot().block.storage_journal;
        assert_eq!(attempted.direct_lookup_units, 131842);
        assert_eq!(attempted.rollback_lookup_units_upper, 513 * 512 + 1);

        // The selected owner's additional warm probes change only direct units, not reserve.
        let mut probe = ZkGasAccumulator::default();
        begin(&mut probe);
        for n in 0..512 {
            probe.observe_storage_journal(owner, Some(n), AccessClass::Cold, false);
        }
        for _ in 0..=32768 {
            probe.observe_storage_journal(owner, Some(512), AccessClass::Warm, false);
        }
        probe.commit_transaction().unwrap();
        let probe = probe.committed_snapshot().storage_journal;
        assert_eq!(probe.direct_lookup_units - diagnostic.direct_lookup_units, 16809984);
        assert_eq!(probe.rollback_lookup_units_upper, diagnostic.rollback_lookup_units_upper);
        assert_eq!(probe.potential_sload_count, 513 + 32768);
    }
}

#[cfg(test)]
mod storage_journal_failure_tests {
    use super::{super::StorageJournalIncompleteness, *};

    #[test]
    fn incomplete_storage_metadata_prevents_partial_quotes() {
        let mut accumulator = ZkGasAccumulator::default();
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        accumulator
            .observe_transaction_payload(TransactionPayloadSelectors {
                canonical_transaction_bytes: 100,
                nonempty_payload_transactions: 0,
            })
            .unwrap();
        accumulator.observe_storage_journal(Address::ZERO, None, AccessClass::Cold, true);
        accumulator.commit_transaction().unwrap();
        let snapshot = accumulator.committed_snapshot();
        assert_eq!(snapshot.diagnostic, None);
        assert_eq!(
            snapshot.storage_journal.incomplete,
            Some(StorageJournalIncompleteness::MissingFootprint)
        );
        let mut baseline = snapshot.clone();
        baseline.storage_journal = StorageJournalDiagnostics::default();
        assert!(matches!(
            snapshot.shadow_report().status(),
            super::super::ZkGasShadowStatus::Diagnostic(
                super::super::ZkGasShadowDiagnostic::StorageJournal(
                    StorageJournalIncompleteness::MissingFootprint
                )
            )
        ));
        assert!(matches!(
            baseline.shadow_report().status(),
            super::super::ZkGasShadowStatus::Incomplete { .. }
        ));
        assert_eq!(snapshot.shadow_report().storage_journal(), snapshot.storage_journal);
        // Scope finalization drops owner state, including a previous incomplete scope.
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        accumulator
            .observe_transaction_payload(TransactionPayloadSelectors {
                canonical_transaction_bytes: 100,
                nonempty_payload_transactions: 0,
            })
            .unwrap();
        accumulator.observe_storage_journal(Address::ZERO, Some(0), AccessClass::Cold, false);
        accumulator.commit_transaction().unwrap();
        let next = accumulator.committed_snapshot();
        assert_eq!(next.storage_journal.direct_lookup_units, 1);
        assert_eq!(next.storage_journal.rollback_lookup_units_upper, 1);
        assert!(next.storage_journal.incomplete.is_some());
        assert_eq!(next.diagnostic, None);
    }

    #[test]
    fn storage_work_survives_pricing_feature_errors_and_resource_only_observations_do_not_count() {
        let mut accumulator = ZkGasAccumulator::default();
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        accumulator
            .observe_transaction_payload(TransactionPayloadSelectors {
                canonical_transaction_bytes: 100,
                nonempty_payload_transactions: 0,
            })
            .unwrap();
        accumulator
            .observe_state_access(StateAccess::StorageRead {
                address: Address::ZERO,
                slot: U256::ZERO,
            })
            .unwrap();
        accumulator.observe_storage_journal(Address::ZERO, Some(0), AccessClass::Cold, true);
        accumulator.observe_storage_materialization(Address::ZERO, U256::ZERO);
        accumulator
            .observe_step(OperationPricingInput {
                route: OperationRoute::Opcode(0x55),
                interpreter_raw_gas: Some(0),
                native_gas: None,
                model_input: OperationModelInput::None,
                feature_error: true,
            })
            .unwrap();
        accumulator.commit_transaction().unwrap();
        let snapshot = accumulator.committed_snapshot();
        assert_eq!(snapshot.storage_journal.potential_sstore_count, 1);
        assert_eq!(snapshot.storage_journal.potential_sload_count, 0);
        assert_eq!(snapshot.storage_journal.direct_lookup_units, 1);
        assert!(snapshot.operation_routes[0].feature_error_scaled > 0);
        let super::super::ZkGasShadowStatus::Incomplete { known, .. } =
            snapshot.shadow_report().status()
        else {
            panic!("priced feature error");
        };
        assert_eq!(known.storage_journal_work_units(), 4);
        assert_eq!(
            known.storage_journal_scaled(),
            ZkGasModel.price_storage_journal_work(4).unwrap()
        );
    }
}

#[cfg(test)]
mod priced_journal_scope_tests {
    use super::{super::StorageJournalIncompleteness, *};
    use crate::zk_gas::pricing::{ZkGasShadowDiagnostic, ZkGasShadowStatus};

    fn begin_one(accumulator: &mut ZkGasAccumulator) {
        accumulator.begin_transaction(TransactionMetadata::default()).unwrap();
        accumulator
            .observe_transaction_payload(TransactionPayloadSelectors {
                canonical_transaction_bytes: 1,
                nonempty_payload_transactions: 0,
            })
            .unwrap();
        accumulator.observe_storage_journal(Address::ZERO, Some(0), AccessClass::Cold, false);
        accumulator.observe_storage_materialization(Address::ZERO, U256::ZERO);
    }

    #[test]
    fn journal_commits_once_and_reset_retains_only_attempted_price() {
        let mut accumulator = ZkGasAccumulator::default();
        begin_one(&mut accumulator);
        accumulator.commit_transaction().unwrap();
        begin_one(&mut accumulator);
        accumulator.reset_transaction();
        for (snapshot, units) in
            [(accumulator.committed_snapshot(), 3), (accumulator.attempted_snapshot().block, 6)]
        {
            assert_eq!(snapshot.storage_journal.comparison_work_units_upper, units);
            let ZkGasShadowStatus::Incomplete { known, .. } = snapshot.shadow_report().status()
            else {
                panic!("valid")
            };
            assert_eq!(known.storage_journal_work_units(), units);
            assert_eq!(
                known.storage_journal_scaled(),
                ZkGasModel.price_storage_journal_work(units).unwrap()
            );
        }
    }

    #[test]
    fn journal_scope_bound_failure_is_typed_without_losing_diagnostics() {
        let mut transaction = TransactionSelectors::new(TransactionMetadata::default());
        transaction.transaction_payload = Some(TransactionPayloadSelectors::default());
        transaction.storage_journal.diagnostics.comparison_work_units_upper = 1_000_000_000_001;
        let mut prefix = SelectorPrefix::default();
        prefix.merge_transaction(&transaction);
        let snapshot = prefix.snapshot();
        assert_eq!(snapshot.storage_journal.comparison_work_units_upper, 1_000_000_000_001);
        assert_eq!(
            snapshot.shadow_report().status(),
            ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::Collector(
                ZkGasAccumulatorError::Pricing(ZkGasError::SelectorOutOfRange {
                    selector: "storage_journal_work_units",
                    value: 1_000_000_000_001,
                })
            ))
        );
    }

    #[test]
    fn journal_overflow_cannot_publish_partial_price() {
        let snapshot = ZkGasBlockSnapshot {
            storage_journal: StorageJournalDiagnostics {
                comparison_work_units_upper: 1,
                incomplete: Some(StorageJournalIncompleteness::ArithmeticOverflow),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            snapshot.shadow_report().status(),
            ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::StorageJournal(
                StorageJournalIncompleteness::ArithmeticOverflow
            ))
        );
    }
}
