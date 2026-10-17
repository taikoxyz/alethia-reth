//! Transactional collection for block-local zk gas V2 selectors.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256};

use super::{
    BlockPricingInput, MptSelectors, OperationModelInput, OperationPricingInput, OperationRoute,
    OperationRoutePricingInput, ResourceSelectors, ZkGasV2Error, ZkGasV2Model,
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

#[derive(Clone, Debug, Default)]
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
}

impl ResourceSet {
    /// Inserts one exact resource observation.
    fn observe(&mut self, access: StateAccess) -> Result<(), ZkGasV2AccumulatorError> {
        match access {
            StateAccess::Account(address) => {
                self.accounts.insert(address);
            }
            StateAccess::StorageRead { address, slot } => {
                self.accounts.insert(address);
                self.storage_accesses.insert((address, slot));
            }
            StateAccess::StorageWrite { address, slot } => {
                self.accounts.insert(address);
                self.storage_accesses.insert((address, slot));
                self.storage_writes.insert((address, slot));
            }
            StateAccess::Code { hash, byte_length } => match self.code.get(&hash) {
                _ if byte_length == 0 => {}
                Some(previous) if *previous != byte_length => {
                    return Err(ZkGasV2AccumulatorError::CodeLengthConflict);
                }
                Some(_) => {}
                None => {
                    self.code.insert(hash, byte_length);
                }
            },
        }
        Ok(())
    }

    /// Merges another unique-resource set.
    fn merge(&mut self, other: ResourceSet) -> Result<(), ZkGasV2AccumulatorError> {
        self.accounts.extend(other.accounts);
        self.storage_accesses.extend(other.storage_accesses);
        self.storage_writes.extend(other.storage_writes);
        for (hash, byte_length) in other.code {
            self.observe(StateAccess::Code { hash, byte_length })?;
        }
        Ok(())
    }

    /// Converts exact sets into public aggregate selectors.
    fn selectors(&self) -> Result<ResourceSelectors, ZkGasV2AccumulatorError> {
        let unique_code_bytes = self.code.values().try_fold(0_u64, |total, bytes| {
            total.checked_add(*bytes).ok_or(ZkGasV2AccumulatorError::CountOverflow)
        })?;
        Ok(ResourceSelectors {
            unique_account_count: u64::try_from(self.accounts.len())
                .map_err(|_| ZkGasV2AccumulatorError::CountOverflow)?,
            unique_storage_slot_access_count: u64::try_from(self.storage_accesses.len())
                .map_err(|_| ZkGasV2AccumulatorError::CountOverflow)?,
            unique_storage_slot_write_count: u64::try_from(self.storage_writes.len())
                .map_err(|_| ZkGasV2AccumulatorError::CountOverflow)?,
            unique_code_hash_count: u64::try_from(self.code.len())
                .map_err(|_| ZkGasV2AccumulatorError::CountOverflow)?,
            unique_code_bytes,
        })
    }
}

#[derive(Clone, Debug)]
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
}

impl TransactionSelectors {
    /// Creates an empty transaction scope.
    fn new(metadata: TransactionMetadata) -> Self {
        Self {
            metadata,
            operation_routes: BTreeMap::new(),
            resources: ResourceSet::default(),
            diagnostic: None,
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
    /// Exact block MPT selectors, populated by the Task-5 trie collector.
    pub mpt: MptSelectors,
    /// First diagnostic committed by an included transaction, if any.
    ///
    /// Selectors remain available for shadow-mode analysis, but a snapshot with a diagnostic is
    /// not a trustworthy admission quote.
    pub diagnostic: Option<ZkGasV2AccumulatorError>,
}

impl ZkGasV2BlockSnapshot {
    /// Borrows this owned snapshot as the pure model input.
    pub fn block_input(&self) -> BlockPricingInput<'_> {
        BlockPricingInput {
            started_non_anchor_transactions: self.started_non_anchor_transactions,
            native_value_transfers: self.native_value_transfers,
            operation_routes: &self.operation_routes,
            resources: self.resources,
            mpt: self.mpt,
        }
    }
}

/// Transactional V2 collector. In-flight selectors are invisible until commit.
#[derive(Clone, Debug, Default)]
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
        if self.in_flight.is_none() {
            return Err(ZkGasV2AccumulatorError::NoActiveTransaction);
        }
        self.pending_spawn = Some(opcode);
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
        let merged = (|| {
            let next_non_anchor = self
                .committed_non_anchor_transactions
                .checked_add(non_anchor)
                .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?;
            let next_native_transfers = self
                .committed_native_value_transfers
                .checked_add(native_transfer)
                .ok_or(ZkGasV2AccumulatorError::ArithmeticOverflow)?;
            let mut routes = self.committed_operation_routes.clone();
            for (key, row) in transaction.operation_routes {
                merge_route(&mut routes, key, row)?;
            }
            let mut resources = self.committed_resources.clone();
            resources.merge(transaction.resources)?;
            Ok((next_non_anchor, next_native_transfers, routes, resources))
        })();
        match merged {
            Ok((next_non_anchor, next_native_transfers, routes, resources)) => {
                self.committed_non_anchor_transactions = next_non_anchor;
                self.committed_native_value_transfers = next_native_transfers;
                self.committed_operation_routes = routes;
                self.committed_resources = resources;
            }
            Err(error) => {
                self.committed_diagnostic.get_or_insert(error);
            }
        }
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
            resources: self
                .committed_resources
                .selectors()
                .expect("resource sizes cannot exceed u64 on supported platforms"),
            mpt: MptSelectors::default(),
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
        let priced = ZkGasV2Model.price_operation(&input)?;
        let transaction =
            self.in_flight.as_mut().ok_or(ZkGasV2AccumulatorError::NoActiveTransaction)?;
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
