//! Transactional collection for block-local zk gas V2 selectors.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256};

use super::{
    OperationModelInput, OperationPricingInput, OperationRoute, OperationRoutePricingInput,
    ResourceSelectors, ZkGasV2Error, ZkGasV2Model,
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
pub enum ZkGasV2AccumulatorError {
    /// An operation could not be priced by the authenticated V2 model.
    Pricing(ZkGasV2Error),
    /// A transaction was started while another transaction remained in flight.
    TransactionAlreadyActive,
    /// An observation or commit occurred without an active transaction.
    NoActiveTransaction,
    /// Checked aggregation overflowed.
    ArithmeticOverflow,
    /// A set size cannot be represented by the public selector type.
    CountOverflow,
    /// The same code hash was reported with inconsistent byte lengths.
    CodeLengthConflict,
}

impl From<ZkGasV2Error> for ZkGasV2AccumulatorError {
    fn from(value: ZkGasV2Error) -> Self {
        Self::Pricing(value)
    }
}

impl core::fmt::Display for ZkGasV2AccumulatorError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Pricing(error) => write!(formatter, "{error}"),
            Self::TransactionAlreadyActive => {
                formatter.write_str("zk gas V2 transaction already active")
            }
            Self::NoActiveTransaction => formatter.write_str("zk gas V2 transaction is not active"),
            Self::ArithmeticOverflow => formatter.write_str("zk gas V2 accumulator overflow"),
            Self::CountOverflow => formatter.write_str("zk gas V2 resource count overflow"),
            Self::CodeLengthConflict => formatter.write_str("zk gas V2 code hash length conflict"),
        }
    }
}

impl std::error::Error for ZkGasV2AccumulatorError {}

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
    fn observe(&mut self, access: StateAccess) -> Result<(), ZkGasV2AccumulatorError> {
        match access {
            StateAccess::Account(address) => {
                let next = self
                    .unique_account_count
                    .checked_add(u64::from(!self.accounts.contains(&address)));
                let next = next.ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
                self.accounts.insert(address);
                self.unique_account_count = next;
            }
            StateAccess::StorageRead { address, slot } => {
                let next_accounts = self
                    .unique_account_count
                    .checked_add(u64::from(!self.accounts.contains(&address)))
                    .ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
                let next_accesses = self
                    .unique_storage_slot_access_count
                    .checked_add(u64::from(!self.storage_accesses.contains(&(address, slot))))
                    .ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
                self.accounts.insert(address);
                self.storage_accesses.insert((address, slot));
                self.unique_account_count = next_accounts;
                self.unique_storage_slot_access_count = next_accesses;
            }
            StateAccess::StorageWrite { address, slot } => {
                let next_accounts = self
                    .unique_account_count
                    .checked_add(u64::from(!self.accounts.contains(&address)))
                    .ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
                let next_accesses = self
                    .unique_storage_slot_access_count
                    .checked_add(u64::from(!self.storage_accesses.contains(&(address, slot))))
                    .ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
                let next_writes = self
                    .unique_storage_slot_write_count
                    .checked_add(u64::from(!self.storage_writes.contains(&(address, slot))))
                    .ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
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
                    return Err(ZkGasV2AccumulatorError::CodeLengthConflict);
                }
                Some(_) => {}
                None => {
                    let next_hashes = self
                        .unique_code_hash_count
                        .checked_add(1)
                        .ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
                    let next_bytes = self
                        .unique_code_bytes
                        .checked_add(byte_length)
                        .ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
                    self.code.insert(hash, byte_length);
                    self.unique_code_hash_count = next_hashes;
                    self.unique_code_bytes = next_bytes;
                }
            },
        }
        Ok(())
    }

    /// Validates a transaction delta against the committed prefix without copying either set.
    fn prevalidate_merge(
        &self,
        other: &Self,
    ) -> Result<ResourceSelectors, ZkGasV2AccumulatorError> {
        let added_accounts = u64::try_from(
            other.accounts.iter().filter(|address| !self.accounts.contains(*address)).count(),
        )
        .map_err(|_| ZkGasV2AccumulatorError::CountOverflow)?;
        let added_accesses = u64::try_from(
            other
                .storage_accesses
                .iter()
                .filter(|slot| !self.storage_accesses.contains(slot))
                .count(),
        )
        .map_err(|_| ZkGasV2AccumulatorError::CountOverflow)?;
        let added_writes = u64::try_from(
            other.storage_writes.iter().filter(|slot| !self.storage_writes.contains(slot)).count(),
        )
        .map_err(|_| ZkGasV2AccumulatorError::CountOverflow)?;
        let mut added_hashes = 0_u64;
        let mut added_bytes = 0_u64;
        for (hash, byte_length) in &other.code {
            match self.code.get(hash) {
                Some(previous) if previous != byte_length => {
                    return Err(ZkGasV2AccumulatorError::CodeLengthConflict);
                }
                Some(_) => {}
                None => {
                    added_hashes = added_hashes
                        .checked_add(1)
                        .ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
                    added_bytes = added_bytes
                        .checked_add(*byte_length)
                        .ok_or(ZkGasV2AccumulatorError::CountOverflow)?;
                }
            }
        }
        Ok(ResourceSelectors {
            unique_account_count: self
                .unique_account_count
                .checked_add(added_accounts)
                .ok_or(ZkGasV2AccumulatorError::CountOverflow)?,
            unique_storage_slot_access_count: self
                .unique_storage_slot_access_count
                .checked_add(added_accesses)
                .ok_or(ZkGasV2AccumulatorError::CountOverflow)?,
            unique_storage_slot_write_count: self
                .unique_storage_slot_write_count
                .checked_add(added_writes)
                .ok_or(ZkGasV2AccumulatorError::CountOverflow)?,
            unique_code_hash_count: self
                .unique_code_hash_count
                .checked_add(added_hashes)
                .ok_or(ZkGasV2AccumulatorError::CountOverflow)?,
            unique_code_bytes: self
                .unique_code_bytes
                .checked_add(added_bytes)
                .ok_or(ZkGasV2AccumulatorError::CountOverflow)?,
        })
    }

    /// Merges a transaction delta after [`Self::prevalidate_merge`] succeeds.
    fn merge_prevalidated(&mut self, other: ResourceSet, totals: ResourceSelectors) {
        self.accounts.extend(other.accounts);
        self.storage_accesses.extend(other.storage_accesses);
        self.storage_writes.extend(other.storage_writes);
        for (hash, byte_length) in other.code {
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
    /// Priced operation components keyed by canonical route.
    operation_routes: BTreeMap<RouteKey, OperationRoutePricingInput>,
    /// Exact resources touched by the transaction.
    resources: ResourceSet,
    /// First observation failure that makes this quote untrusted.
    diagnostic: Option<ZkGasV2AccumulatorError>,
    /// Whether execution emitted any opcode, spawn, or precompile operation event.
    has_operation_trace: bool,
}

impl TransactionSelectors {
    /// Creates an empty transaction scope.
    fn new(metadata: TransactionMetadata) -> Self {
        Self {
            metadata,
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
/// use alethia_reth_evm::zk_gas::v2::ZkGasV2BlockSnapshot;
/// let snapshot = ZkGasV2BlockSnapshot::default();
/// let complete_input = snapshot.block_input();
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZkGasV2BlockSnapshot {
    /// Committed non-anchor transaction count.
    pub started_non_anchor_transactions: u64,
    /// Committed native value-transfer count.
    pub native_value_transfers: u64,
    /// Committed accumulated operation candidates in canonical route order.
    pub operation_routes: Vec<OperationRoutePricingInput>,
    /// Committed unique-resource selectors.
    pub resources: ResourceSelectors,
    /// First diagnostic committed by an included transaction, if any.
    ///
    /// Selectors remain available for shadow-mode analysis, but a snapshot with a diagnostic is
    /// not a trustworthy admission quote.
    pub diagnostic: Option<ZkGasV2AccumulatorError>,
}

/// Transactional V2 collector. In-flight selectors are invisible until commit.
///
/// The collector intentionally does not implement `Clone`: transaction commit must prevalidate
/// and merge only the incoming delta, never copy the full committed resource prefix.
///
/// ```compile_fail
/// use alethia_reth_evm::zk_gas::v2::ZkGasV2Accumulator;
/// let accumulator = ZkGasV2Accumulator::default();
/// let duplicated = accumulator.clone();
/// ```
#[derive(Debug, Default)]
pub struct ZkGasV2Accumulator {
    /// Included non-anchor transaction count.
    committed_non_anchor_transactions: u64,
    /// Included nonzero native-value transfer count.
    committed_native_value_transfers: u64,
    /// Included operation components aggregated by route.
    committed_operation_routes: BTreeMap<RouteKey, OperationRoutePricingInput>,
    /// Block-global unique execution resources.
    committed_resources: ResourceSet,
    /// Transaction scope currently being executed.
    in_flight: Option<TransactionSelectors>,
    /// Spawn opcode selected by the interpreter but not yet dispatched.
    pending_spawn: Option<u8>,
    /// First diagnostic committed by an included transaction.
    committed_diagnostic: Option<ZkGasV2AccumulatorError>,
}

impl ZkGasV2Accumulator {
    /// Starts a fresh transaction selector scope.
    pub fn begin_transaction(
        &mut self,
        metadata: TransactionMetadata,
    ) -> Result<(), ZkGasV2AccumulatorError> {
        if self.in_flight.is_some() {
            return Err(ZkGasV2AccumulatorError::TransactionAlreadyActive);
        }
        self.pending_spawn = None;
        self.in_flight = Some(TransactionSelectors::new(metadata));
        Ok(())
    }

    /// Prices and stages one completed opcode or confirmed spawn event.
    pub fn observe_step(
        &mut self,
        input: OperationPricingInput,
    ) -> Result<(), ZkGasV2AccumulatorError> {
        if matches!(input.route, OperationRoute::Precompile(_)) {
            return Err(ZkGasV2AccumulatorError::Pricing(ZkGasV2Error::InvalidInput(
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
    ) -> Result<(), ZkGasV2AccumulatorError> {
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
    ) -> Result<(), ZkGasV2AccumulatorError> {
        let result = self
            .in_flight
            .as_mut()
            .ok_or(ZkGasV2AccumulatorError::NoActiveTransaction)?
            .resources
            .observe(access);
        self.retain_diagnostic(result.err());
        result
    }

    /// Stages a spawn wrapper selected by the interpreter until frame dispatch confirms it.
    pub(crate) fn select_spawn(&mut self, opcode: u8) -> Result<(), ZkGasV2AccumulatorError> {
        let transaction =
            self.in_flight.as_mut().ok_or(ZkGasV2AccumulatorError::NoActiveTransaction)?;
        transaction.has_operation_trace = true;
        self.pending_spawn = Some(opcode);
        Ok(())
    }

    /// Finalizes the calibrated native-transfer marker after execution disposition is known.
    pub(crate) fn finalize_native_value_transfer(
        &mut self,
        eligible: bool,
    ) -> Result<(), ZkGasV2AccumulatorError> {
        let transaction =
            self.in_flight.as_mut().ok_or(ZkGasV2AccumulatorError::NoActiveTransaction)?;
        transaction.metadata.native_value_transfer = eligible && !transaction.has_operation_trace;
        Ok(())
    }

    /// Converts the last selected spawn wrapper into its authenticated confirmed route.
    pub(crate) fn confirm_spawn(&mut self) -> Result<(), ZkGasV2AccumulatorError> {
        let Some(opcode) = self.pending_spawn.take() else { return Ok(()) };
        let result = self.observe_operation(OperationPricingInput {
            route: OperationRoute::ConfirmedSpawn(opcode),
            interpreter_raw_gas: None,
            native_gas: None,
            model_input: OperationModelInput::None,
            feature_error: false,
        });
        self.retain_diagnostic(result.err());
        result
    }

    /// Atomically merges the in-flight selector scope into the block snapshot.
    pub fn commit_transaction(&mut self) -> Result<(), ZkGasV2AccumulatorError> {
        let transaction =
            self.in_flight.take().ok_or(ZkGasV2AccumulatorError::NoActiveTransaction)?;
        self.pending_spawn = None;
        let diagnostic = transaction.diagnostic;
        let non_anchor = u64::from(!transaction.metadata.is_anchor);
        let native_transfer = u64::from(transaction.metadata.native_value_transfer);
        if self.committed_diagnostic.is_none() {
            self.committed_diagnostic = diagnostic;
        }
        let prevalidated = (|| {
            let next_non_anchor = self
                .committed_non_anchor_transactions
                .checked_add(non_anchor)
                .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?;
            let next_native_transfers = self
                .committed_native_value_transfers
                .checked_add(native_transfer)
                .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?;
            let mut routes = Vec::with_capacity(transaction.operation_routes.len());
            for (key, row) in &transaction.operation_routes {
                routes.push((*key, merged_route(self.committed_operation_routes.get(key), *row)?));
            }
            let resources = self.committed_resources.prevalidate_merge(&transaction.resources)?;
            Ok((next_non_anchor, next_native_transfers, routes, resources))
        })();
        let Ok((next_non_anchor, next_native_transfers, routes, resource_totals)) = prevalidated
        else {
            self.committed_diagnostic.get_or_insert(prevalidated.unwrap_err());
            return Ok(());
        };
        self.committed_non_anchor_transactions = next_non_anchor;
        self.committed_native_value_transfers = next_native_transfers;
        for (key, row) in routes {
            self.committed_operation_routes.insert(key, row);
        }
        self.committed_resources.merge_prevalidated(transaction.resources, resource_totals);
        Ok(())
    }

    /// Discards every in-flight selector while preserving prior committed transactions.
    pub fn reset_transaction(&mut self) {
        self.in_flight = None;
        self.pending_spawn = None;
    }

    /// Returns an owned committed block snapshot; in-flight data is excluded.
    pub fn committed_snapshot(&self) -> ZkGasV2BlockSnapshot {
        ZkGasV2BlockSnapshot {
            started_non_anchor_transactions: self.committed_non_anchor_transactions,
            native_value_transfers: self.committed_native_value_transfers,
            operation_routes: self.committed_operation_routes.values().copied().collect(),
            resources: self.committed_resources.selectors(),
            diagnostic: self.committed_diagnostic,
        }
    }

    /// Returns the first diagnostic affecting the current block quote.
    pub const fn diagnostic(&self) -> Option<ZkGasV2AccumulatorError> {
        match &self.in_flight {
            Some(transaction) if transaction.diagnostic.is_some() => transaction.diagnostic,
            _ => self.committed_diagnostic,
        }
    }

    /// Retains the first typed observer diagnostic without changing EVM execution.
    pub(crate) fn retain_diagnostic(&mut self, error: Option<ZkGasV2AccumulatorError>) {
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
    ) -> Result<(), ZkGasV2AccumulatorError> {
        let transaction =
            self.in_flight.as_mut().ok_or(ZkGasV2AccumulatorError::NoActiveTransaction)?;
        transaction.has_operation_trace = true;
        let priced = ZkGasV2Model.price_operation(&input)?;
        let row = OperationRoutePricingInput {
            route: input.route,
            accepted_absolute_scaled: priced.accepted_absolute_scaled,
            incremental_scaled: priced.incremental_scaled,
            structural_scaled: priced.structural_scaled,
            feature_error_scaled: priced.feature_error_scaled,
        };
        merge_route(&mut transaction.operation_routes, input.route.into(), row)
    }
}

/// Computes the post-merge route row without mutating the committed prefix.
fn merged_route(
    existing: Option<&OperationRoutePricingInput>,
    row: OperationRoutePricingInput,
) -> Result<OperationRoutePricingInput, ZkGasV2AccumulatorError> {
    let Some(existing) = existing else { return Ok(row) };
    if existing.route != row.route {
        return Err(ZkGasV2AccumulatorError::Pricing(ZkGasV2Error::InvalidInput("operation_route")));
    }
    Ok(OperationRoutePricingInput {
        route: row.route,
        accepted_absolute_scaled: existing
            .accepted_absolute_scaled
            .checked_add(row.accepted_absolute_scaled)
            .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?,
        incremental_scaled: existing
            .incremental_scaled
            .checked_add(row.incremental_scaled)
            .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?,
        structural_scaled: existing
            .structural_scaled
            .checked_add(row.structural_scaled)
            .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?,
        feature_error_scaled: existing
            .feature_error_scaled
            .checked_add(row.feature_error_scaled)
            .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?,
    })
}

/// Checked component-wise aggregation for one canonical operation route.
fn merge_route(
    routes: &mut BTreeMap<RouteKey, OperationRoutePricingInput>,
    key: RouteKey,
    row: OperationRoutePricingInput,
) -> Result<(), ZkGasV2AccumulatorError> {
    let accumulated = routes.entry(key).or_insert(OperationRoutePricingInput {
        route: row.route,
        accepted_absolute_scaled: 0,
        incremental_scaled: 0,
        structural_scaled: 0,
        feature_error_scaled: 0,
    });
    if accumulated.route != row.route {
        return Err(ZkGasV2AccumulatorError::Pricing(ZkGasV2Error::InvalidInput("operation_route")));
    }
    accumulated.accepted_absolute_scaled = accumulated
        .accepted_absolute_scaled
        .checked_add(row.accepted_absolute_scaled)
        .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?;
    accumulated.incremental_scaled = accumulated
        .incremental_scaled
        .checked_add(row.incremental_scaled)
        .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?;
    accumulated.structural_scaled = accumulated
        .structural_scaled
        .checked_add(row.structural_scaled)
        .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?;
    accumulated.feature_error_scaled = accumulated
        .feature_error_scaled
        .checked_add(row.feature_error_scaled)
        .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?;
    Ok(())
}
