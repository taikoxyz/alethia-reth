//! State-root strategy for Taiko engine-tree validation.
//!
//! Since reth v2.5, [`HashedPostState::from_bundle_state`] no longer marks a destroyed account's
//! storage as wiped. The state providers' [`HashedPostStateProvider::hashed_post_state`] writes
//! explicit zeroes for the destroyed account's parent slots instead. reth's synchronous state-root
//! job still hashes the bundle with the raw conversion, though. That job runs under
//! `--engine.state-root-fallback` or on hosts with fewer than five threads.
//!
//! Every pre-Unzen fork runs SHANGHAI (pre-EIP-6780) SELFDESTRUCT semantics, so a block can
//! destroy a contract that already holds storage. On the synchronous path, reth then gets two
//! things wrong:
//!
//! - If the same block re-creates the contract, the root keeps the destroyed slots, and a valid
//!   block is rejected.
//! - If it does not, the root is right but the persisted hashed state keeps the slots. A later
//!   block that re-creates an account at that address can then fail its root check.
//!
//! [`TaikoStateRootStrategy`] replaces that one job. Every other block keeps reth's default
//! machinery. The sparse-trie task needs no change: its root check against the header falls back
//! to a serial recomputation that already hashes through the provider.
//!
//! [`HashedPostState::from_bundle_state`]: reth_trie_common::HashedPostState::from_bundle_state
//! [`HashedPostStateProvider::hashed_post_state`]: reth_provider::HashedPostStateProvider::hashed_post_state
//! [`TaikoStateRootStrategy`]: crate::engine::state_root::TaikoStateRootStrategy

use std::sync::Arc;

use reth_engine_tree::tree::{
    StateProviderBuilder, TreeConfig,
    state_root_strategy::{
        DefaultStateRootStrategy, LazyHashedPostState, PayloadStateRootHandle,
        PayloadStateRootJobContext, PreparedStateRootJob, StateRootJob, StateRootJobContext,
        StateRootJobOutcome, StateRootStrategy,
    },
};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{NodePrimitives, RecoveredBlock};
use reth_provider::{
    BlockExecutionOutput, BlockNumReader, DatabaseProviderFactory, HashedPostStateProvider,
    ProviderResult, PruneCheckpointReader, StageCheckpointReader, StateRootProvider,
    StorageSettingsCache, TryIntoHistoricalStateProvider,
};
use reth_revm::db::BundleState;

/// Engine-tree state-root strategy that keeps destroyed-account storage deletions on the
/// synchronous path.
///
/// It holds reth's [`DefaultStateRootStrategy`] and forwards every block to it, unless the tree
/// config selects reth's synchronous job. Those blocks get `SynchronousStateRootJob` instead.
#[derive(Debug)]
pub struct TaikoStateRootStrategy {
    /// reth's strategy, used for every block the synchronous override does not cover.
    default: DefaultStateRootStrategy,
    /// Whether the tree config routes blocks to the synchronous state-root job.
    synchronous: bool,
}

impl TaikoStateRootStrategy {
    /// Creates the strategy for an engine tree validating with `config`.
    ///
    /// `config` must be the tree config the validator runs with. It decides once which job
    /// [`DefaultStateRootStrategy`] would pick, because the strategy context does not expose it.
    pub fn new(config: &TreeConfig) -> Self {
        Self {
            default: DefaultStateRootStrategy::default(),
            synchronous: !config.skip_state_root() && !config.use_state_root_task(),
        }
    }
}

impl<N, P, Evm> StateRootStrategy<N, P, Evm> for TaikoStateRootStrategy
where
    N: NodePrimitives,
    P: DatabaseProviderFactory + Clone + 'static,
    P::Provider: BlockNumReader
        + PruneCheckpointReader
        + StageCheckpointReader
        + StorageSettingsCache
        + TryIntoHistoricalStateProvider
        + 'static,
    Evm: ConfigureEvm<Primitives = N>,
    DefaultStateRootStrategy: StateRootStrategy<N, P, Evm>,
{
    /// Prepares `SynchronousStateRootJob` where reth would prepare its synchronous job, and
    /// defers to [`DefaultStateRootStrategy`] otherwise.
    ///
    /// This mirrors the mode selection in `DefaultStateRootStrategy::prepare` at the pinned reth
    /// rev: skipped, then synchronous when the state-root task is off, then the sparse-trie task.
    /// Re-check it on every reth bump.
    fn prepare(
        &self,
        ctx: StateRootJobContext<'_, N, P, Evm>,
    ) -> ProviderResult<PreparedStateRootJob<N>> {
        if !self.synchronous {
            return self.default.prepare(ctx);
        }
        let job = SynchronousStateRootJob { provider_builder: ctx.provider_builder() };
        Ok(PreparedStateRootJob::new(Box::new(job), None))
    }

    /// Defers to [`DefaultStateRootStrategy`]. Payload building runs at the tip, where Unzen's
    /// EIP-6780 semantics leave no pre-existing storage to delete.
    fn prepare_payload_builder(
        &self,
        ctx: PayloadStateRootJobContext<'_, N, P>,
    ) -> ProviderResult<Option<PayloadStateRootHandle>> {
        self.default.prepare_payload_builder(ctx)
    }
}

/// Synchronous state-root job that hashes the post state through the parent state provider.
///
/// It replaces reth's synchronous job, which roots the raw bundle conversion. The only difference
/// is that it hashes through the provider; see [`state_root_with_parent_storage`].
#[derive(Debug)]
struct SynchronousStateRootJob<N: NodePrimitives, P> {
    /// Builds the state provider at the block's parent, including in-memory ancestors.
    provider_builder: StateProviderBuilder<N, P>,
}

impl<N, P> StateRootJob<N> for SynchronousStateRootJob<N, P>
where
    N: NodePrimitives,
    P: DatabaseProviderFactory + Clone + 'static,
    P::Provider: BlockNumReader
        + PruneCheckpointReader
        + StageCheckpointReader
        + StorageSettingsCache
        + TryIntoHistoricalStateProvider
        + 'static,
{
    /// Uses the name of the reth job it replaces, so the engine's state-root logs are unchanged.
    fn name(&self) -> &'static str {
        "synchronous"
    }

    /// Computes the root at the block's parent state, ignoring the background raw conversion.
    fn finish(
        &mut self,
        _block: &RecoveredBlock<N::Block>,
        output: Arc<BlockExecutionOutput<N::Receipt>>,
        _hashed_state: &LazyHashedPostState,
    ) -> ProviderResult<StateRootJobOutcome> {
        let provider = self.provider_builder.build()?;
        state_root_with_parent_storage(provider.as_ref(), &output.state)
    }
}

/// Computes the state root of `bundle_state` on top of `provider`, the block's parent state.
///
/// Hashes the bundle with [`HashedPostStateProvider::hashed_post_state`], which zeroes every
/// parent slot of a destroyed pre-existing account. That is what reth's own serial fallback does.
/// The hashed state is returned in the outcome, so engine validation drops its raw conversion and
/// the in-memory overlay and persisted hashed state both carry the deletions.
fn state_root_with_parent_storage<P>(
    provider: &P,
    bundle_state: &BundleState,
) -> ProviderResult<StateRootJobOutcome>
where
    P: StateRootProvider + HashedPostStateProvider + ?Sized,
{
    let hashed_state = Arc::new(provider.hashed_post_state(bundle_state)?);
    let (state_root, trie_updates) =
        provider.state_root_with_updates(hashed_state.as_ref().clone())?;
    Ok(StateRootJobOutcome::new(state_root, Arc::new(trie_updates))
        .with_hashed_state(Some(hashed_state)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, U256, keccak256};
    use reth_primitives_traits::Account;
    use reth_provider::{
        ProviderFactory, StateWriter, TrieWriter,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory},
    };
    use reth_revm::{
        db::{
            AccountStatus, BundleAccount,
            states::{StorageSlot, StorageWithOriginalValues},
        },
        state::AccountInfo,
    };
    use reth_trie_common::{HashedPostState, HashedStorage, KeccakKeyHasher, updates::TrieUpdates};

    /// Externally owned account that keeps the state trie non-trivial.
    const CALLER: Address = Address::with_last_byte(0x30);

    /// Contract that exists before the block and is destroyed by it.
    const CONTRACT: Address = Address::with_last_byte(0x40);

    /// Storage the contract holds before the block, as `(slot, value)`.
    const PARENT_SLOTS: [(u64, u64); 2] = [(1, 42), (2, 7)];

    fn caller_account() -> Account {
        Account { nonce: 3, balance: U256::from(1_000_u64), bytecode_hash: None }
    }

    fn parent_contract() -> AccountInfo {
        AccountInfo { nonce: 1, code_hash: B256::repeat_byte(0xc0), ..Default::default() }
    }

    fn recreated_contract() -> AccountInfo {
        AccountInfo { nonce: 1, code_hash: B256::repeat_byte(0xc1), ..Default::default() }
    }

    fn hashed_slot(slot: u64) -> B256 {
        keccak256(B256::from(U256::from(slot)))
    }

    /// Test database holding [`CALLER`] and [`CONTRACT`] with [`PARENT_SLOTS`].
    fn parent_state() -> ProviderFactory<MockNodeTypesWithDB> {
        let factory = create_test_provider_factory();
        let mut state = HashedPostState::default();
        state.accounts.insert(keccak256(CALLER), Some(caller_account()));
        state.accounts.insert(keccak256(CONTRACT), Some(parent_contract().into()));
        state.storages.insert(
            keccak256(CONTRACT),
            HashedStorage::from_iter(
                PARENT_SLOTS.iter().map(|&(slot, value)| (hashed_slot(slot), U256::from(value))),
            ),
        );
        persist(&factory, state, TrieUpdates::default());
        factory
    }

    /// Writes one block's hashed state and trie updates, as engine persistence does.
    fn persist(
        factory: &ProviderFactory<MockNodeTypesWithDB>,
        hashed_state: HashedPostState,
        trie_updates: TrieUpdates,
    ) {
        let provider_rw = factory.provider_rw().expect("read-write provider opens");
        provider_rw.write_hashed_state(&hashed_state.into_sorted()).expect("hashed state writes");
        provider_rw.write_trie_updates(trie_updates).expect("trie updates write");
        provider_rw.commit().expect("block commits");
    }

    /// Bundle whose only account is [`CONTRACT`].
    fn contract_bundle(account: BundleAccount) -> BundleState {
        let mut bundle = BundleState::default();
        bundle.state.insert(CONTRACT, account);
        bundle
    }

    /// Storage changes written by the block, as `(slot, value)`.
    fn storage_writes(writes: &[(u64, u64)]) -> StorageWithOriginalValues {
        writes
            .iter()
            .map(|&(slot, value)| {
                (U256::from(slot), StorageSlot::new_changed(U256::ZERO, U256::from(value)))
            })
            .collect()
    }

    /// State root computed with `triehash` straight from the expected accounts and storage.
    fn expected_root(contract: Option<(AccountInfo, &[(u64, u64)])>) -> B256 {
        let mut accounts = vec![(CALLER, (caller_account(), Vec::new()))];
        if let Some((info, slots)) = contract {
            let storage = slots
                .iter()
                .map(|&(slot, value)| (B256::from(U256::from(slot)), U256::from(value)))
                .collect();
            accounts.push((CONTRACT, (info.into(), storage)));
        }
        reth_trie::test_utils::state_root(accounts)
    }

    /// Root and inputs from the fixed job, plus the hashed state engine validation keeps.
    fn fixed_serial_root(
        factory: &ProviderFactory<MockNodeTypesWithDB>,
        bundle: &BundleState,
    ) -> (B256, HashedPostState, TrieUpdates) {
        let provider = factory.latest().expect("latest state provider opens");
        let outcome =
            state_root_with_parent_storage(provider.as_ref(), bundle).expect("state root computes");
        let hashed_state =
            outcome.hashed_state.expect("the outcome must replace the raw hashed state");
        (outcome.state_root, (*hashed_state).clone(), (*outcome.trie_updates).clone())
    }

    /// What reth's synchronous job computes: the root of the raw bundle conversion.
    fn raw_serial_root(
        factory: &ProviderFactory<MockNodeTypesWithDB>,
        bundle: &BundleState,
    ) -> (B256, HashedPostState, TrieUpdates) {
        let provider = factory.latest().expect("latest state provider opens");
        let hashed_state = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
        let (root, updates) =
            provider.state_root_with_updates(hashed_state.clone()).expect("state root computes");
        (root, hashed_state, updates)
    }

    #[test]
    fn parent_state_matches_the_expected_root() {
        let factory = parent_state();
        let root = factory
            .latest()
            .expect("latest state provider opens")
            .state_root(HashedPostState::default())
            .expect("state root computes");

        assert_eq!(root, expected_root(Some((parent_contract(), &PARENT_SLOTS))));
    }

    #[test]
    fn serial_root_drops_the_old_storage_of_a_contract_recreated_in_the_same_block() {
        let factory = parent_state();
        // The re-created contract overwrites slot 1, leaves slot 2 alone, and adds slot 3.
        let recreated_slots = [(1, 5), (3, 9)];
        let bundle = contract_bundle(BundleAccount::new(
            Some(parent_contract()),
            Some(recreated_contract()),
            storage_writes(&recreated_slots),
            AccountStatus::DestroyedChanged,
        ));
        let expected = expected_root(Some((recreated_contract(), &recreated_slots)));

        let (root, hashed_state, _) = fixed_serial_root(&factory, &bundle);
        assert_eq!(root, expected);
        assert_eq!(
            hashed_state.storages.get(&keccak256(CONTRACT)).map(|storage| &storage.storage),
            Some(
                &[
                    (hashed_slot(1), U256::from(5)),
                    (hashed_slot(2), U256::ZERO),
                    (hashed_slot(3), U256::from(9))
                ]
                .into_iter()
                .collect()
            ),
            "slot 2 must be zeroed, and the re-created contract's writes must win"
        );

        // Documents the reth behavior the override exists for. If this starts to match, reth
        // fixed its synchronous job and the override can be reconsidered.
        let (raw_root, _, _) = raw_serial_root(&factory, &bundle);
        assert_ne!(raw_root, expected, "reth's raw conversion keeps the destroyed slot 2");
    }

    #[test]
    fn serial_root_deletes_the_storage_of_a_destroyed_contract() {
        let destroyed = contract_bundle(BundleAccount::new(
            Some(parent_contract()),
            None,
            Default::default(),
            AccountStatus::Destroyed,
        ));
        let recreated_slots = [(3, 9)];
        let recreated = contract_bundle(BundleAccount::new(
            None,
            Some(recreated_contract()),
            storage_writes(&recreated_slots),
            AccountStatus::InMemoryChange,
        ));
        let expected = expected_root(Some((recreated_contract(), &recreated_slots)));

        // Block 1 destroys the contract and block 2 puts one back at its address. The persisted
        // zeroes from block 1 are what keep the old slots out of block 2's root.
        let factory = parent_state();
        let (root, hashed_state, updates) = fixed_serial_root(&factory, &destroyed);
        assert_eq!(root, expected_root(None));
        persist(&factory, hashed_state, updates);
        let (root, _, _) = fixed_serial_root(&factory, &recreated);
        assert_eq!(root, expected);

        // Documents the reth behavior the override exists for. The raw conversion gets block 1's
        // root right but persists nothing for the destroyed storage, so block 2 inherits it.
        let factory = parent_state();
        let (root, hashed_state, updates) = raw_serial_root(&factory, &destroyed);
        assert_eq!(root, expected_root(None));
        persist(&factory, hashed_state, updates);
        let (root, _, _) = raw_serial_root(&factory, &recreated);
        assert_ne!(root, expected, "the raw conversion leaves the destroyed slots in the database");
    }

    #[test]
    fn overrides_only_the_synchronous_job() {
        let task = TreeConfig::default().with_has_enough_parallelism(true);
        assert!(!TaikoStateRootStrategy::new(&task).synchronous);
        assert!(
            TaikoStateRootStrategy::new(&task.clone().with_state_root_fallback(true)).synchronous
        );
        assert!(
            TaikoStateRootStrategy::new(&task.clone().with_has_enough_parallelism(false))
                .synchronous
        );
        assert!(
            !TaikoStateRootStrategy::new(
                &task.with_has_enough_parallelism(false).with_skip_state_root(true)
            )
            .synchronous
        );
    }
}
