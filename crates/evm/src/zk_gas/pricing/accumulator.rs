//! Transactional collection for block-local zk gas pricing selectors.

#[cfg(test)]
mod account_cache_prefix_tests {
    use super::*;

    fn finish(a: &mut ZkGasAccumulator, keys: &[Address], accepted: bool) {
        a.begin_transaction(TransactionMetadata::default()).unwrap();
        a.observe_transaction_payload(TransactionPayloadSelectors {
            canonical_transaction_bytes: 1,
            nonempty_payload_transactions: 0,
        })
        .unwrap();
        a.complete_account_journal_population(keys.iter().copied()).unwrap();
        a.complete_storage_transition_population([]).unwrap();
        if accepted {
            a.commit_transaction().unwrap();
        } else {
            a.reset_transaction();
        }
    }

    #[test]
    fn account_cache_progressive_population_and_repeated_transactions() {
        let mut a = ZkGasAccumulator::default();
        finish(&mut a, &[Address::with_last_byte(1)], true);
        finish(&mut a, &[Address::with_last_byte(2)], true);
        finish(&mut a, &[Address::with_last_byte(1)], true);
        let d = a.committed_snapshot().account_cache;
        assert_eq!((d.observed_accounts, d.max_prefix_group), (2, 2));
        assert_eq!((d.query_budget, d.work_units, d.completed_transactions), (6, 10, 3));
        assert_eq!(a.committed_snapshot().account_cache_work_units().unwrap(), 10);
    }

    #[test]
    fn account_cache_rejected_population_does_not_leak_into_committed_prefix() {
        let mut a = ZkGasAccumulator::default();
        finish(&mut a, &[Address::with_last_byte(1), Address::with_last_byte(2)], false);
        finish(&mut a, &[Address::with_last_byte(3)], true);
        assert_eq!(a.committed_snapshot().account_cache.work_units, 2);
        assert_eq!(a.attempted_snapshot().block.account_cache.work_units, 14);
        assert_eq!(ZkGasAccumulator::default().committed_snapshot().account_cache.work_units, 0);
    }

    #[test]
    fn account_cache_warm_opcodes_do_not_repeat_db_proxy_and_invalid_receipts_fail_closed() {
        let mut a = ZkGasAccumulator::default();
        a.begin_transaction(TransactionMetadata::default()).unwrap();
        a.observe_transaction_payload(TransactionPayloadSelectors {
            canonical_transaction_bytes: 1,
            nonempty_payload_transactions: 0,
        })
        .unwrap();
        for _ in 0..128 {
            a.observe_account_journal_opcode(0x31);
        }
        a.complete_account_journal_population([Address::ZERO]).unwrap();
        a.complete_storage_transition_population([]).unwrap();
        a.commit_transaction().unwrap();
        assert_eq!(a.committed_snapshot().account_cache.query_budget, 2);
        assert_eq!(a.committed_snapshot().account_cache.work_units, 2);
        assert_eq!(a.committed_snapshot().account_journal.potential_query_budget, 256);

        let mut duplicate = ZkGasAccumulator::default();
        duplicate.begin_transaction(TransactionMetadata::default()).unwrap();
        duplicate.complete_account_journal_population([]).unwrap();
        duplicate.complete_storage_transition_population([]).unwrap();
        assert!(duplicate.complete_account_journal_population([]).is_err());
        duplicate.reset_transaction();
        assert_eq!(
            duplicate.attempted_snapshot().block.account_cache.incomplete,
            Some(super::super::AccountCacheIncompleteness::InvalidPopulation)
        );
        assert!(duplicate.attempted_snapshot().block.account_cache_work_units().is_err());

        let mut missing = ZkGasAccumulator::default();
        missing.begin_transaction(TransactionMetadata::default()).unwrap();
        missing.reset_transaction();
        assert_eq!(
            missing.attempted_snapshot().block.account_cache.incomplete,
            Some(super::super::AccountCacheIncompleteness::InvalidPopulation)
        );
    }
}

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256};

use super::{
    AccountCacheDiagnostics, AccountJournalDiagnostics,
    account_cache::AccountCacheLedger,
    account_journal::AccountJournalLedger,
    storage_journal::StorageJournalLedger,
    storage_transition::{StorageTransitionLedger, StorageTransitionReceipt},
    transient_journal::TransientStorageJournalLedger,
};

use super::{
    AccessClass, AccessListConstructionDiagnostics, AccessListConstructionIncompleteness,
    CodePreparationSelectors, MemorySelectors, OperationModelInput, OperationPricingInput,
    OperationRoute, OperationRoutePricingInput, ResourceSelectors, StorageJournalDiagnostics,
    StorageTransitionDiagnostics, StorageTransitionIncompleteness, TransactionPayloadSelectors,
    TransientStorageJournalDiagnostics, ZkGasError, ZkGasModel,
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
    /// One whole raw-list construction receipt; scratch groups are not retained during execution.
    access_list_construction: Option<AccessListConstructionDiagnostics>,
    /// Accepted nonempty code preparations, retained even if an ancestor reverts.
    code_preparation: CodePreparationSelectors,
    /// Priced operation components keyed by canonical route.
    operation_routes: BTreeMap<RouteKey, OperationRoutePricingInput>,
    /// Exact resources touched by the transaction.
    resources: ResourceSet,
    /// Potential journal observation, including independently priced comparison work for this
    /// scope.
    storage_journal: StorageJournalLedger,
    /// Scalar Account budget and exact physical-population receipt for this transaction.
    account_journal: AccountJournalLedger,
    /// Finalized physical keys retained only until both prefix merges complete.
    account_cache_keys: Option<Vec<Address>>,
    /// One physical final storage map receipt, dropped after both independent prefix merges.
    storage_transition: Option<StorageTransitionReceipt>,
    /// Independent transaction-local transient map work and distinct materialization scope.
    transient_storage_journal: TransientStorageJournalLedger,
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
            access_list_construction: None,
            code_preparation: CodePreparationSelectors::default(),
            operation_routes: BTreeMap::new(),
            resources: ResourceSet::default(),
            storage_journal: StorageJournalLedger::default(),
            account_journal: AccountJournalLedger::default(),
            account_cache_keys: None,
            storage_transition: None,
            transient_storage_journal: TransientStorageJournalLedger::default(),
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
    /// Independent raw-prefix construction receipt, summed across transaction scopes.
    pub access_list_construction: AccessListConstructionDiagnostics,
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
    /// Empirical Account density reserve, finalized separately for each observing transaction.
    pub account_journal: AccountJournalDiagnostics,
    /// Progressive block-cache population proxy, separate from transaction-local journal work.
    pub account_cache: AccountCacheDiagnostics,
    /// Independent progressive physical-slot population and forward changed-slot reserve.
    pub storage_transition: StorageTransitionDiagnostics,
    /// Transient-only map work; its units join the ordinary term without sharing owner/key state.
    pub transient_storage_journal: TransientStorageJournalDiagnostics,
    /// First diagnostic retained in this prefix, if any.
    ///
    /// Selectors remain available for shadow-mode analysis, but a snapshot with a diagnostic is
    /// not a trustworthy admission quote.
    pub diagnostic: Option<ZkGasAccumulatorError>,
}

impl ZkGasBlockSnapshot {
    /// Returns complete forward-transition units, accepting readonly population with zero work.
    pub fn storage_transition_work_units(&self) -> Result<u64, ZkGasError> {
        let d = self.storage_transition;
        if d.incomplete.is_some() {
            return Err(ZkGasError::InvalidInput("incomplete_storage_transition_work"));
        }
        let invalid = || ZkGasError::InvalidInput("inconsistent_storage_transition_work");
        if (d.completed_transactions == 0 && d != StorageTransitionDiagnostics::EMPTY) ||
            d.changed_slots > d.physical_slot_receipts ||
            d.observed_storage_slots > d.physical_slot_receipts ||
            d.max_owner_slots > d.observed_storage_slots ||
            d.max_low32_group > d.max_owner_slots ||
            d.query_budget != d.changed_slots.checked_mul(2).ok_or_else(invalid)? ||
            (d.observed_storage_slots == 0 &&
                (d.physical_slot_receipts != 0 ||
                    d.max_owner_slots != 0 ||
                    d.max_low32_group != 0 ||
                    d.comparison_equivalents != 0 ||
                    d.work_units != 0)) ||
            (d.observed_storage_slots > 0 &&
                (d.max_owner_slots == 0 ||
                    d.max_low32_group == 0 ||
                    d.comparison_equivalents < d.max_low32_group ||
                    d.comparison_equivalents > d.max_owner_slots ||
                    u128::from(d.comparison_equivalents) <
                        (u128::from(d.max_owner_slots) + 14) / 8)) ||
            (d.query_budget == 0 && d.work_units != 0) ||
            (d.query_budget > 0 &&
                (d.work_units < d.query_budget ||
                    u128::from(d.work_units) >
                        u128::from(d.query_budget) *
                            u128::from(d.comparison_equivalents)))
        {
            return Err(invalid());
        }
        ZkGasModel.price_storage_transition_work(d.work_units)?;
        Ok(d.work_units)
    }

    /// Returns complete progressive block-cache work; partial observations fail closed.
    pub fn account_cache_work_units(&self) -> Result<u64, ZkGasError> {
        let d = self.account_cache;
        if d.incomplete.is_some() {
            return Err(ZkGasError::InvalidInput("incomplete_account_cache_work"));
        }
        if (d.completed_transactions == 0 && d != AccountCacheDiagnostics::EMPTY) ||
            d.max_prefix_group > d.observed_accounts ||
            (d.observed_accounts == 0 &&
                (d.max_prefix_group != 0 ||
                    d.comparison_equivalents != 0 ||
                    d.work_units != 0)) ||
            (d.observed_accounts > 0 &&
                (d.max_prefix_group == 0 ||
                    d.comparison_equivalents < d.max_prefix_group ||
                    d.comparison_equivalents > d.observed_accounts ||
                    d.work_units < d.query_budget))
        {
            return Err(ZkGasError::InvalidInput("inconsistent_account_cache_work"));
        }
        let numerator = d
            .observed_accounts
            .checked_add(
                d.max_prefix_group
                    .checked_mul(4)
                    .ok_or(ZkGasError::InvalidInput("inconsistent_account_cache_work"))?,
            )
            .ok_or(ZkGasError::InvalidInput("inconsistent_account_cache_work"))?;
        if d.comparison_equivalents != numerator / 5 + u64::from(numerator % 5 != 0) {
            return Err(ZkGasError::InvalidInput("inconsistent_account_cache_work"));
        }
        ZkGasModel.price_account_cache_work(d.work_units)?;
        Ok(d.work_units)
    }

    /// Returns finalized Account work, rejecting missing/duplicate or overflowed captures.
    pub fn account_journal_work_units(&self) -> Result<u64, ZkGasError> {
        if self.account_journal.incomplete.is_some() {
            return Err(ZkGasError::InvalidInput("incomplete_account_journal_work"));
        }
        let d = self.account_journal;
        if (d.completed_transactions == 0 && d != AccountJournalDiagnostics::EMPTY) ||
            (d.physical_account_count == 0 &&
                (d.max_prefix_group != 0 ||
                    d.comparison_equivalents != 0 ||
                    d.work_units != 0)) ||
            (d.physical_account_count > 0 &&
                (d.max_prefix_group == 0 ||
                    d.max_prefix_group > d.physical_account_count ||
                    d.comparison_equivalents < d.max_prefix_group ||
                    d.comparison_equivalents > d.physical_account_count ||
                    d.work_units < d.physical_account_count))
        {
            return Err(ZkGasError::InvalidInput("inconsistent_account_journal_work"));
        }
        ZkGasModel.price_account_journal_work(self.account_journal.work_units)?;
        Ok(self.account_journal.work_units)
    }

    /// Returns checked raw-prefix pairs; an incomplete capture cannot price as zero.
    pub fn access_list_construction_pairs(&self) -> Result<u64, ZkGasError> {
        if self.access_list_construction.incomplete.is_some() {
            return Err(ZkGasError::InvalidInput("incomplete_access_list_construction"));
        }
        let pairs = self.access_list_construction.comparison_pairs;
        ZkGasModel.price_access_list_construction_pairs(pairs)?;
        Ok(pairs)
    }

    /// Returns checked different-low32 pairs without treating incomplete captures as zero.
    pub fn access_list_construction_other_pairs(&self) -> Result<u64, ZkGasError> {
        if self.access_list_construction.incomplete.is_some() {
            return Err(ZkGasError::InvalidInput("incomplete_access_list_construction"));
        }
        if self
            .access_list_construction
            .comparison_pairs
            .checked_add(self.access_list_construction.other_pairs) !=
            Some(self.access_list_construction.owner_pairs)
        {
            return Err(ZkGasError::InvalidInput("inconsistent_access_list_construction_pairs"));
        }
        let pairs = self.access_list_construction.other_pairs;
        ZkGasModel.price_access_list_construction_other_pairs(pairs)?;
        Ok(pairs)
    }

    /// Returns finalized transaction-local raw address pairs, never cross-transaction pairs.
    pub fn access_list_address_pairs(&self) -> Result<u64, ZkGasError> {
        if self.access_list_construction.incomplete.is_some() {
            return Err(ZkGasError::InvalidInput("incomplete_access_list_construction"));
        }
        if self.access_list_construction.raw_address_items != self.access_list_items {
            return Err(ZkGasError::InvalidInput("access_list_address_item_count"));
        }
        let pairs = self.access_list_construction.raw_address_pairs;
        ZkGasModel.price_access_list_address_pairs(pairs)?;
        Ok(pairs)
    }

    /// Returns the aggregate ordinary plus transient journal units within the single priced domain.
    ///
    /// Both independent diagnostics must be complete. Addition and the existing aggregate
    /// work-unit bound are checked; partial/incomplete work is never silently replaced by zero.
    pub fn storage_journal_work_units(&self) -> Result<u64, ZkGasError> {
        if self.storage_journal.incomplete.is_some() ||
            self.transient_storage_journal.incomplete.is_some()
        {
            return Err(ZkGasError::InvalidInput("incomplete_storage_journal_work"));
        }
        let units = self
            .storage_journal
            .comparison_work_units_upper
            .checked_add(self.transient_storage_journal.comparison_work_units_upper)
            .ok_or(ZkGasError::ArithmeticOverflow)?;
        ZkGasModel.price_storage_journal_work(units)?;
        Ok(units)
    }
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
/// budget is not itself pricing-observed. Its captured ordinary/transient journal work is retained
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
    /// Checked raw-prefix construction receipts across finalized transaction scopes.
    access_list_construction: AccessListConstructionDiagnostics,
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
    /// Per-TX Account units and checked density diagnostics; never a block-wide product.
    account_journal: AccountJournalDiagnostics,
    /// Prefix-local virtual State-cache population and progressive work.
    account_cache: AccountCacheLedger,
    /// Independent owner-key membership for this committed or attempted prefix.
    storage_transition: StorageTransitionLedger,
    /// Checked transient observations, additive across finalized transaction scopes.
    transient_storage_journal: TransientStorageJournalDiagnostics,
    /// First diagnostic included in this prefix.
    diagnostic: Option<ZkGasAccumulatorError>,
}

impl SelectorPrefix {
    /// Merges one transaction into this prefix after validating the complete delta.
    fn merge_transaction(&mut self, transaction: &TransactionSelectors) {
        self.access_list_construction.merge(transaction.access_list_construction.unwrap_or_else(
            || {
                AccessListConstructionDiagnostics::uncaptured(
                    transaction.metadata.access_list_items,
                )
            },
        ));
        self.storage_journal.merge(transaction.storage_journal.diagnostics);
        let account = transaction.account_journal.diagnostics();
        self.account_journal.merge(account);
        self.account_cache.merge(
            transaction.account_cache_keys.as_deref(),
            transaction.storage_journal.diagnostics.materialization_count_upper,
            account.incomplete.is_none() &&
                transaction.storage_journal.diagnostics.incomplete.is_none(),
        );
        self.storage_transition.merge(transaction.storage_transition.as_ref());
        self.transient_storage_journal.merge(transaction.transient_storage_journal.diagnostics());
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
            let journal_units = self
                .storage_journal
                .comparison_work_units_upper
                .checked_add(self.transient_storage_journal.comparison_work_units_upper)
                .ok_or(ZkGasAccumulatorError::ArithmeticOverflow)?;
            ZkGasModel.price_storage_journal_work(journal_units)?;
            ZkGasModel.price_account_journal_work(self.account_journal.work_units)?;
            ZkGasModel
                .price_storage_transition_work(self.storage_transition.diagnostics().work_units)?;
            ZkGasModel.price_access_list_construction_pairs(
                self.access_list_construction.comparison_pairs,
            )?;
            ZkGasModel.price_access_list_construction_other_pairs(
                self.access_list_construction.other_pairs,
            )?;
            ZkGasModel
                .price_access_list_address_pairs(self.access_list_construction.raw_address_pairs)?;
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
            access_list_construction: self.access_list_construction,
            code_preparation: self.code_preparation,
            transaction_payload: self.transaction_payload,
            operation_routes: self.operation_routes.values().copied().collect(),
            resources: self.resources.selectors(),
            storage_journal: self.storage_journal,
            account_journal: self.account_journal,
            account_cache: self.account_cache.diagnostics(),
            storage_transition: self.storage_transition.diagnostics(),
            transient_storage_journal: self.transient_storage_journal,
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
    /// Retains one policy budget per executed opcode attempt, including early failures.
    pub(crate) fn observe_account_journal_opcode(&mut self, opcode: u8) {
        if let Some(transaction) = self.in_flight.as_mut() {
            transaction.account_journal.observe_opcode(opcode);
        }
    }

    /// Supplies actual finalized journal map keys once, on success and handler error alike.
    ///
    /// Keys must be the distinct map iterator, not declared AL owners or resource observations.
    /// A compact receipt also feeds independent committed/attempted progressive cache proxies.
    /// It is dropped after both merges; unobserved preexecution cache keys are not fabricated.
    pub fn complete_account_journal_population(
        &mut self,
        addresses: impl IntoIterator<Item = Address>,
    ) -> Result<(), ZkGasAccumulatorError> {
        let result = (|| {
            let transaction =
                self.in_flight.as_mut().ok_or(ZkGasAccumulatorError::NoActiveTransaction)?;
            let keys: Vec<_> = addresses.into_iter().collect();
            transaction.account_journal.complete(keys.iter().copied());
            if transaction.account_cache_keys.is_none() {
                transaction.account_cache_keys = Some(keys);
            }
            if transaction.account_journal.diagnostics().incomplete.is_some() {
                return Err(ZkGasError::InvalidInput("incomplete_account_journal_work").into());
            }
            Ok(())
        })();
        self.retain_diagnostic(result.err());
        result
    }

    /// Supplies the actual finalized owner/key/changed iterator exactly once, including empty
    /// scopes.
    ///
    /// This capture is independent of declared AL keys, opcode writes and logical resources.
    /// It never reads a database or changes EVM state; both prefixes derive their own union.
    pub fn complete_storage_transition_population(
        &mut self,
        slots: impl IntoIterator<Item = (Address, U256, bool)>,
    ) -> Result<(), ZkGasAccumulatorError> {
        let result = (|| {
            let transaction =
                self.in_flight.as_mut().ok_or(ZkGasAccumulatorError::NoActiveTransaction)?;
            if let Some(receipt) = transaction.storage_transition.as_mut() {
                receipt
                    .incomplete
                    .get_or_insert(StorageTransitionIncompleteness::DuplicatePopulation);
            } else {
                transaction.storage_transition = Some(StorageTransitionReceipt::capture(slots));
            }
            if transaction
                .storage_transition
                .as_ref()
                .is_some_and(|receipt| receipt.incomplete.is_some())
            {
                return Err(ZkGasError::InvalidInput("incomplete_storage_transition_work").into());
            }
            Ok(())
        })();
        self.retain_diagnostic(result.err());
        result
    }

    /// Records one whole signed raw access list before execution, then drops its scratch map.
    ///
    /// Raw owner items share one transaction-local prefix scope. A second capture, mismatched
    /// address-item count or arithmetic failure is sticky and prevents a valid quote. Empty
    /// zero-item scopes may omit this call; nonempty scopes cannot silently omit their receipt.
    pub fn observe_access_list_construction<I, J>(
        &mut self,
        items: I,
    ) -> Result<(), ZkGasAccumulatorError>
    where
        I: IntoIterator<Item = (Address, J)>,
        J: IntoIterator<Item = U256>,
    {
        let result = (|| {
            let transaction =
                self.in_flight.as_mut().ok_or(ZkGasAccumulatorError::NoActiveTransaction)?;
            if let Some(receipt) = transaction.access_list_construction.as_mut() {
                receipt
                    .incomplete
                    .get_or_insert(AccessListConstructionIncompleteness::DuplicateCapture);
                return Err(ZkGasError::InvalidInput("duplicate_access_list_construction").into());
            }
            let receipt = AccessListConstructionDiagnostics::capture(
                items,
                transaction.metadata.access_list_items,
            );
            let incomplete = receipt.incomplete.is_some();
            transaction.access_list_construction = Some(receipt);
            if incomplete {
                return Err(ZkGasError::InvalidInput("incomplete_access_list_construction").into());
            }
            Ok(())
        })();
        self.retain_diagnostic(result.err());
        result
    }

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

    /// Retains a potential TLOAD/TSTORE attempt separately from persistent resource selectors.
    pub(crate) fn observe_transient_storage_journal(
        &mut self,
        owner: Address,
        occupancy: Option<usize>,
        store: bool,
    ) {
        if let Some(transaction) = &mut self.in_flight {
            transaction.transient_storage_journal.observe(owner, occupancy, store);
        }
    }

    /// Seeds a physical initial transient key or first vacant nonzero-store candidate.
    ///
    /// The transient ledger deduplicates full `(owner, U256)` keys across clear and rollback.
    /// Read misses and absent zero stores must not call this hook or consume its seen set.
    pub(crate) fn observe_transient_storage_materialization(&mut self, owner: Address, key: U256) {
        if let Some(transaction) = &mut self.in_flight {
            transaction.transient_storage_journal.observe_materialization(owner, key);
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
        let Some(pending) = self.pending_spawn.take() else {
            return Ok(());
        };
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
        transaction.account_journal.finish();
        transaction.transient_storage_journal.finish();
        self.record_attempt(&transaction);
        self.committed.merge_transaction(&transaction);
        Ok(())
    }

    /// Retains an attempted observation and clears its in-flight selector scope.
    pub fn reset_transaction(&mut self) {
        if let Some(mut transaction) = self.in_flight.take() {
            transaction.storage_journal.finish();
            transaction.account_journal.finish();
            transaction.transient_storage_journal.finish();
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
    let Some(existing) = existing else {
        return Ok(row);
    };
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
        accumulator.complete_account_journal_population([]).unwrap();
        accumulator.complete_storage_transition_population([]).unwrap();
        accumulator.commit_transaction().unwrap();
        let snapshot = accumulator.committed_snapshot();
        assert_eq!(snapshot.diagnostic, None);
        assert_eq!(
            snapshot.storage_journal.incomplete,
            Some(StorageJournalIncompleteness::MissingFootprint)
        );
        let mut baseline = snapshot.clone();
        baseline.storage_journal = StorageJournalDiagnostics::default();
        // Both dependent receipts were deliberately incomplete in this synthetic control.
        baseline.account_cache = AccountCacheDiagnostics::EMPTY;
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
        accumulator.complete_account_journal_population([]).unwrap();
        accumulator.complete_storage_transition_population([]).unwrap();
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
        accumulator.complete_account_journal_population([]).unwrap();
        accumulator.complete_storage_transition_population([]).unwrap();
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
        accumulator.complete_account_journal_population([]).unwrap();
        accumulator.complete_storage_transition_population([]).unwrap();
        accumulator.commit_transaction().unwrap();
        begin_one(&mut accumulator);
        accumulator.complete_account_journal_population([]).unwrap();
        accumulator.complete_storage_transition_population([]).unwrap();
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

#[cfg(test)]
mod transient_journal_composition_tests {
    use super::*;

    #[test]
    fn one_combined_domain_and_incomplete_transient_cannot_publish_a_partial_quote() {
        let mut snapshot = ZkGasBlockSnapshot::default();
        snapshot.storage_journal.comparison_work_units_upper = 600_000_000_000;
        snapshot.transient_storage_journal.comparison_work_units_upper = 600_000_000_000;
        assert!(matches!(
            snapshot.storage_journal_work_units(),
            Err(ZkGasError::SelectorOutOfRange { selector: "storage_journal_work_units", .. })
        ));
        snapshot.storage_journal.comparison_work_units_upper = u64::MAX;
        assert_eq!(snapshot.storage_journal_work_units(), Err(ZkGasError::ArithmeticOverflow));
        snapshot.storage_journal.comparison_work_units_upper = 1;
        snapshot.transient_storage_journal.comparison_work_units_upper = 2;
        assert_eq!(snapshot.storage_journal_work_units(), Ok(3));
        snapshot.transient_storage_journal.incomplete =
            Some(super::super::StorageJournalIncompleteness::MissingFootprint);
        assert!(snapshot.storage_journal_work_units().is_err());
        assert!(matches!(
            snapshot.shadow_report().status(),
            super::super::ZkGasShadowStatus::Diagnostic(
                super::super::ZkGasShadowDiagnostic::StorageJournal(_)
            )
        ));
    }
}

#[cfg(test)]
mod storage_transition_scope_tests {
    use super::*;
    use crate::zk_gas::pricing::{ZkGasShadowDiagnostic, ZkGasShadowStatus};

    fn begin(a: &mut ZkGasAccumulator) {
        a.begin_transaction(TransactionMetadata::default()).unwrap();
        a.observe_transaction_payload(TransactionPayloadSelectors {
            canonical_transaction_bytes: 1,
            nonempty_payload_transactions: 0,
        })
        .unwrap();
        a.complete_account_journal_population([]).unwrap();
    }
    fn finish(a: &mut ZkGasAccumulator, slots: &[(Address, U256, bool)], commit: bool) {
        begin(a);
        a.complete_storage_transition_population(slots.iter().copied()).unwrap();
        if commit {
            a.commit_transaction().unwrap();
        } else {
            a.reset_transaction();
        }
    }
    #[test]
    fn rejected_population_is_attempted_only_and_new_block_starts_empty() {
        let owner = Address::ZERO;
        let key = |n: u64| U256::from(n) << 48;
        let mut a = ZkGasAccumulator::default();
        finish(&mut a, &[(owner, key(1), false), (owner, key(2), true)], false);
        finish(&mut a, &[(owner, key(3), true)], true);
        let committed = a.committed_snapshot();
        let attempted = a.attempted_snapshot().block;
        assert_eq!(
            (
                committed.storage_transition.observed_storage_slots,
                committed.storage_transition.work_units
            ),
            (1, 2)
        );
        assert_eq!(
            (
                attempted.storage_transition.observed_storage_slots,
                attempted.storage_transition.work_units
            ),
            (3, 10)
        );
        assert_eq!(committed.storage_transition_work_units().unwrap(), 2);
        assert_eq!(attempted.storage_transition_work_units().unwrap(), 10);
        assert_eq!(
            ZkGasAccumulator::default().committed_snapshot().storage_transition,
            StorageTransitionDiagnostics::EMPTY
        );
    }
    #[test]
    fn missing_duplicate_and_invalid_population_cannot_publish_partial_quotes() {
        let mut missing = ZkGasAccumulator::default();
        begin(&mut missing);
        missing.commit_transaction().unwrap();
        let snapshot = missing.committed_snapshot();
        assert_eq!(
            snapshot.storage_transition.incomplete,
            Some(StorageTransitionIncompleteness::MissingPopulation)
        );
        assert!(matches!(
            snapshot.shadow_report().status(),
            ZkGasShadowStatus::Diagnostic(ZkGasShadowDiagnostic::StorageTransition(
                StorageTransitionIncompleteness::MissingPopulation
            ))
        ));
        let mut duplicate = ZkGasAccumulator::default();
        begin(&mut duplicate);
        duplicate.complete_storage_transition_population([]).unwrap();
        assert!(duplicate.complete_storage_transition_population([]).is_err());
        duplicate.reset_transaction();
        assert_eq!(
            duplicate.attempted_snapshot().block.storage_transition.incomplete,
            Some(StorageTransitionIncompleteness::DuplicatePopulation)
        );
        let mut invalid = ZkGasAccumulator::default();
        begin(&mut invalid);
        assert!(
            invalid
                .complete_storage_transition_population([(Address::ZERO, U256::ZERO, true); 2])
                .is_err()
        );
        invalid.commit_transaction().unwrap();
        assert_eq!(invalid.committed_snapshot().storage_transition.work_units, 0);
        assert_eq!(
            invalid.committed_snapshot().storage_transition.incomplete,
            Some(StorageTransitionIncompleteness::DuplicateKey)
        );
        assert!(invalid.committed_snapshot().storage_transition_work_units().is_err());
    }
    #[test]
    fn readonly_nonempty_and_multiowner_diagnostics_validate_without_global_formula() {
        let mut a = ZkGasAccumulator::default();
        finish(&mut a, &[(Address::ZERO, U256::ZERO, false)], true);
        assert_eq!(a.committed_snapshot().storage_transition_work_units().unwrap(), 0);
        let mut snapshot = a.committed_snapshot();
        snapshot.storage_transition.query_budget = 1;
        assert!(snapshot.storage_transition_work_units().is_err());
        let d = StorageTransitionDiagnostics {
            physical_slot_receipts: 15,
            changed_slots: 2,
            observed_storage_slots: 15,
            max_owner_slots: 10,
            max_low32_group: 6,
            comparison_equivalents: 6,
            query_budget: 4,
            work_units: 24,
            completed_transactions: 1,
            incomplete: None,
        };
        snapshot.storage_transition = d;
        assert_eq!(snapshot.storage_transition_work_units().unwrap(), 24);
        snapshot.storage_transition.work_units = 25;
        assert!(snapshot.storage_transition_work_units().is_err());
        snapshot.storage_transition = d;
        snapshot.storage_transition.work_units = 1_000_000_000_001;
        assert!(snapshot.storage_transition_work_units().is_err());
    }
}
