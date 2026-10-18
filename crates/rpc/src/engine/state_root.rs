//! State-root strategy for Taiko engine-tree validation.
//!
//! reth v2.6 has no storage wipe marker: [`HashedPostState::from_bundle_state`] carries only the
//! slots a block wrote, and the state providers' [`HashedPostStateProvider::hashed_post_state`]
//! writes explicit zeroes for a destroyed account's parent slots instead. reth's synchronous
//! state-root job still roots the engine validator's lazy hashed state, which is the raw bundle
//! conversion whenever no job supplies one. That job runs under `--engine.state-root-fallback` or
//! on hosts with fewer than five threads.
//!
//! Every pre-Unzen fork runs SHANGHAI (pre-EIP-6780) SELFDESTRUCT semantics, so a block can
//! destroy a contract that already holds storage. On reth's synchronous path that goes wrong:
//!
//! - If the same block re-creates the contract, the root keeps the destroyed slots, and a valid
//!   block is rejected.
//! - If it does not, the root is right but the persisted hashed state keeps the destroyed slots
//!   (`write_hashed_state` deletes only the `HashedAccounts` row). A later block that re-creates an
//!   account at that address can then fail its root check.
//!
//! reth's sparse-trie task is right only by accident. It emits the destroyed account as deleted
//! with no storage, but its trie keeps the account's old storage root and encodes a zero-balance
//! leaf instead of deleting it. The root then mismatches the header, and reth's serial fallback
//! recomputes it through the provider, whose hashed state (explicit zeroes included) is what gets
//! persisted. If every slot was zeroed before the destroy, the leaf is deleted correctly and the
//! zero writes are persisted as well.
//!
//! [`TaikoStateRootStrategy`] does not rely on that quirk: it roots every pre-Unzen block, and
//! every block when the tree runs reth's synchronous mode, with a provider-backed synchronous job
//! whose outcome carries the provider's hashed state, explicit zeroes included, for validation and
//! persistence. Post-Unzen blocks keep reth's default machinery: EIP-6780 destroys only accounts
//! created in the same transaction, which have no parent storage.
//!
//! Payload building has no header to check against: with
//! `--engine.share-sparse-trie-with-payload-builder`, the builder takes the sparse trie's root as
//! is, and that trie keeps a destroyed account's old storage root. Before Unzen the strategy
//! therefore hands the builder no sparse trie, so it computes the root through the provider. A
//! sparse trie carried across the Unzen boundary may still hold the storage trie of a contract a
//! pre-Unzen block destroyed, so a network that crosses Unzen with that flag on should leave it off
//! until the boundary is a few blocks behind.
//!
//! Re-check both on every reth bump. The validation override can go once every reth state-root
//! path persists the provider's zeroes for a destroyed account (its synchronous job, the lazy
//! hashed state in `payload_validator.rs`, and the sparse task's hashed state). The payload-builder
//! gate can go once reth's sparse trie wipes a destroyed account's storage.
//!
//! [`HashedPostState::from_bundle_state`]: reth_trie_common::HashedPostState::from_bundle_state
//! [`HashedPostStateProvider::hashed_post_state`]: reth_provider::HashedPostStateProvider::hashed_post_state
//! [`TaikoStateRootStrategy`]: crate::engine::state_root::TaikoStateRootStrategy

use std::sync::Arc;

use alethia_reth_chainspec::{hardfork::TaikoHardforks, spec::TaikoChainSpec};
use reth_engine_tree::tree::{
    TreeConfig,
    state_root_strategy::{
        DefaultStateRootStrategy, LazyHashedPostState, PayloadStateRootHandle,
        PayloadStateRootJobContext, PreparedStateRootJob, StateRootJob, StateRootJobContext,
        StateRootJobOutcome, StateRootStrategy,
    },
};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{AlloyBlockHeader as _, NodePrimitives, RecoveredBlock};
use reth_provider::{
    BlockExecutionOutput, DatabaseProviderFactory, DatabaseProviderROFactory,
    HashedPostStateProvider, ProviderResult, StateRootProvider,
};
use reth_revm::{context::Block as _, db::BundleState};
use reth_storage_overlay::{OverlayManager, OverlayStateProviderFactory};

/// Engine-tree state-root strategy that keeps pre-Unzen destroyed-account storage deletions in the
/// computed root and the persisted hashed state.
///
/// It holds reth's [`DefaultStateRootStrategy`] and forwards every block to it, except pre-Unzen
/// blocks and blocks the tree config sends to reth's synchronous job: those get
/// `SynchronousStateRootJob`. Payload builds before Unzen get no shared sparse trie, so they root
/// through the provider.
#[derive(Debug)]
pub struct TaikoStateRootStrategy<P, N: NodePrimitives> {
    /// reth's strategy, used for every block the synchronous override does not cover.
    default: DefaultStateRootStrategy,
    /// Whether the tree config skips state-root computation (reth's debug mode).
    skip_state_root: bool,
    /// Whether the tree config routes blocks to the synchronous state-root job.
    synchronous: bool,
    /// Chain spec, used to keep the payload builder off the shared sparse trie before Unzen.
    chain_spec: Arc<TaikoChainSpec>,
    /// Database provider factory the synchronous job reads the parent state from.
    provider: P,
    /// The engine tree's overlay manager, which holds the parent's in-memory ancestors.
    overlay_manager: OverlayManager<N>,
}

impl<P, N: NodePrimitives> TaikoStateRootStrategy<P, N> {
    /// Creates the strategy for an engine tree validating with `config`.
    ///
    /// `config` must be the tree config the validator runs with. It decides once which job
    /// [`DefaultStateRootStrategy`] would pick, because the strategy context does not expose it.
    /// `provider` and `overlay_manager` must be the ones the validator runs with: the job context
    /// does not expose its state provider factory, so the strategy builds its own from them.
    pub fn new(
        config: &TreeConfig,
        chain_spec: Arc<TaikoChainSpec>,
        provider: P,
        overlay_manager: OverlayManager<N>,
    ) -> Self {
        Self {
            default: DefaultStateRootStrategy::default(),
            skip_state_root: config.skip_state_root(),
            synchronous: !config.skip_state_root() && !config.use_state_root_task(),
            chain_spec,
            provider,
            overlay_manager,
        }
    }

    /// Returns whether a block at `timestamp` is rooted by `SynchronousStateRootJob`: when the
    /// tree runs reth's synchronous mode, and for every pre-Unzen block unless state roots are
    /// skipped.
    fn roots_through_provider(&self, timestamp: u64) -> bool {
        !self.skip_state_root && (self.synchronous || !self.chain_spec.is_unzen_active(timestamp))
    }

    /// Returns whether a payload on a parent with timestamp `parent_timestamp` may take its state
    /// root from the shared sparse trie: only once Unzen's EIP-6780 semantics leave no pre-existing
    /// storage to delete.
    ///
    /// It keys on the parent rather than the payload attributes: the block is executed and sealed
    /// with its block-metadata timestamp, which nothing forces to equal the attributes' one, but is
    /// never below its parent's, so a pre-Unzen block always has a pre-Unzen parent.
    fn payload_builder_may_share_sparse_trie(&self, parent_timestamp: u64) -> bool {
        self.chain_spec.is_unzen_active(parent_timestamp)
    }
}

impl<N, P, Evm> StateRootStrategy<N, P, Evm> for TaikoStateRootStrategy<P, N>
where
    N: NodePrimitives,
    P: DatabaseProviderFactory + Clone + 'static,
    OverlayStateProviderFactory<P, N>:
        DatabaseProviderROFactory<Provider: StateRootProvider + HashedPostStateProvider>,
    Evm: ConfigureEvm<Primitives = N>,
    DefaultStateRootStrategy: StateRootStrategy<N, P, Evm>,
{
    /// Prepares `SynchronousStateRootJob` for pre-Unzen blocks and where reth would prepare its
    /// synchronous job, and defers to [`DefaultStateRootStrategy`] otherwise.
    ///
    /// This mirrors the mode selection in `DefaultStateRootStrategy::prepare` at the pinned reth
    /// rev: skipped, then synchronous when the state-root task is off, then the sparse-trie task.
    /// The job reads the parent state the way the validator does, through an
    /// [`OverlayStateProviderFactory`] over the parent's in-memory ancestors. Re-check both on
    /// every reth bump.
    fn prepare(
        &self,
        ctx: StateRootJobContext<'_, N, P, Evm>,
    ) -> ProviderResult<PreparedStateRootJob<N>> {
        let timestamp = ctx.env().evm_env.block_env.timestamp().saturating_to();
        if !self.roots_through_provider(timestamp) {
            return self.default.prepare(ctx);
        }
        let state_provider_factory = OverlayStateProviderFactory::new(
            self.provider.clone(),
            self.overlay_manager.overlay_builder(ctx.parent_header().hash()),
        );
        let job = SynchronousStateRootJob { state_provider_factory };
        Ok(PreparedStateRootJob::new(Box::new(job), None))
    }

    /// Defers to [`DefaultStateRootStrategy`] once Unzen is active at the parent's timestamp.
    /// Before that it returns `None`, so the builder roots the block through the provider instead
    /// of accepting a sparse-trie root that keeps a destroyed account's old storage.
    fn prepare_payload_builder(
        &self,
        ctx: PayloadStateRootJobContext<'_, N, P>,
    ) -> ProviderResult<Option<PayloadStateRootHandle>> {
        if !self.payload_builder_may_share_sparse_trie(ctx.parent_header().timestamp()) {
            return Ok(None);
        }
        self.default.prepare_payload_builder(ctx)
    }
}

/// Synchronous state-root job that hashes the post state through the parent state provider.
///
/// It replaces reth's synchronous job, which roots the raw bundle conversion. The only difference
/// is that it hashes through the provider; see [`state_root_with_parent_storage`].
#[derive(Debug)]
struct SynchronousStateRootJob<N: NodePrimitives, P> {
    /// Opens the state at the block's parent, including in-memory ancestors.
    state_provider_factory: OverlayStateProviderFactory<P, N>,
}

impl<N, P> StateRootJob<N> for SynchronousStateRootJob<N, P>
where
    N: NodePrimitives,
    P: DatabaseProviderFactory + 'static,
    OverlayStateProviderFactory<P, N>:
        DatabaseProviderROFactory<Provider: StateRootProvider + HashedPostStateProvider>,
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
        let provider = self.state_provider_factory.database_provider_ro()?;
        state_root_with_parent_storage(&provider, &output.state)
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
    let hashed_state = provider.hashed_post_state(bundle_state)?;
    let (state_root, trie_updates) = provider.state_root_with_updates(hashed_state.clone())?;
    Ok(StateRootJobOutcome::new(state_root, Arc::new(trie_updates))
        .with_hashed_state(Some(Arc::new(hashed_state))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alethia_reth_chainspec::{TAIKO_DEVNET, hardfork::TaikoHardfork};
    use alloy_consensus::Header;
    use alloy_hardforks::ForkCondition;
    use alloy_primitives::{Address, B256, U256, keccak256};
    use reth_chain_state::ExecutedBlock;
    use reth_ethereum_primitives::{Block, EthPrimitives};
    use reth_primitives_traits::{Account, SealedBlock};
    use reth_provider::{
        BlockWriter, ProviderFactory, StageCheckpointWriter, StateWriter,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory},
    };
    use reth_revm::{
        db::{
            AccountStatus, BundleAccount,
            states::{StorageSlot, StorageWithOriginalValues},
        },
        state::AccountInfo,
    };
    use reth_trie_common::{
        ComputedTrieData, HashedPostState, HashedStorage, KeccakKeyHasher, updates::TrieUpdates,
    };

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

    /// Empty block `number` on top of `parent_hash`.
    fn block(number: u64, parent_hash: B256) -> RecoveredBlock<Block> {
        let header = Header { number, parent_hash, ..Default::default() };
        RecoveredBlock::new_sealed(
            SealedBlock::seal_slow(Block { header, body: Default::default() }),
            Vec::new(),
        )
    }

    /// The engine tree's view of a chain: block 0 persisted with the Finish checkpoint on it, and
    /// every later block held in memory by the overlay manager, as before persistence.
    struct Chain {
        factory: ProviderFactory<MockNodeTypesWithDB>,
        overlay_manager: OverlayManager<EthPrimitives>,
        tip: RecoveredBlock<Block>,
    }

    impl Chain {
        /// Parent state of the tested blocks: [`CALLER`] and [`CONTRACT`] with its first parent
        /// slot persisted at block 0, and its second slot written by block 1, kept in memory.
        fn parent_state() -> Self {
            let factory = create_test_provider_factory();
            let genesis = block(0, B256::ZERO);
            let mut state = HashedPostState::default();
            state.accounts.insert(keccak256(CALLER), Some(caller_account()));
            state.accounts.insert(keccak256(CONTRACT), Some(parent_contract().into()));
            let (slot, value) = PARENT_SLOTS[0];
            state.storages.insert(
                keccak256(CONTRACT),
                HashedStorage::from_iter([(hashed_slot(slot), U256::from(value))]),
            );
            let provider_rw = factory.provider_rw().expect("read-write provider opens");
            provider_rw.insert_block(&genesis).expect("block 0 inserts");
            provider_rw.update_pipeline_stages(0, false).expect("checkpoints write");
            provider_rw.write_hashed_state(&state.into_sorted()).expect("hashed state writes");
            provider_rw.commit().expect("block 0 commits");

            let mut chain =
                Self { factory, overlay_manager: OverlayManager::default(), tip: genesis };
            let (slot, value) = PARENT_SLOTS[1];
            let mut block_1 = HashedPostState::default();
            block_1.storages.insert(
                keccak256(CONTRACT),
                HashedStorage::from_iter([(hashed_slot(slot), U256::from(value))]),
            );
            chain.push(block_1, TrieUpdates::default());
            chain
        }

        /// Appends an in-memory block with `hashed_state` and `trie_updates` on top of the tip.
        fn push(&mut self, hashed_state: HashedPostState, trie_updates: TrieUpdates) {
            let block = block(self.tip.number + 1, self.tip.hash());
            self.overlay_manager.insert_block(ExecutedBlock::new(
                Arc::new(block.clone()),
                Arc::default(),
                ComputedTrieData::new(
                    Arc::new(hashed_state.into_sorted()),
                    Arc::new(trie_updates.into_sorted()),
                ),
            ));
            self.tip = block;
        }

        /// Finishes the synchronous job for a block on top of the tip that wrote `bundle`, handing
        /// it the raw bundle conversion, as reth's validator does.
        fn finish(&self, bundle: BundleState) -> StateRootJobOutcome {
            let raw = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
            let mut job = SynchronousStateRootJob {
                state_provider_factory: OverlayStateProviderFactory::new(
                    self.factory.clone(),
                    self.overlay_manager.overlay_builder(self.tip.hash()),
                ),
            };
            job.finish(
                &block(self.tip.number + 1, self.tip.hash()),
                Arc::new(BlockExecutionOutput { state: bundle, ..Default::default() }),
                &LazyHashedPostState::ready(Arc::new(raw)),
            )
            .expect("state root computes")
        }
    }

    /// Hashed state the outcome hands engine validation in place of the raw conversion.
    fn outcome_hashed_state(outcome: &StateRootJobOutcome) -> HashedPostState {
        outcome
            .hashed_state
            .as_deref()
            .expect("the outcome must replace the raw hashed state")
            .clone()
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

    #[test]
    fn parent_state_matches_the_expected_root() {
        let outcome = Chain::parent_state().finish(BundleState::default());

        assert_eq!(outcome.state_root, expected_root(Some((parent_contract(), &PARENT_SLOTS))));
    }

    #[test]
    fn serial_root_drops_the_old_storage_of_a_contract_recreated_in_the_same_block() {
        let chain = Chain::parent_state();
        // The re-created contract overwrites slot 1, leaves slot 2 alone, and adds slot 3.
        let recreated_slots = [(1, 5), (3, 9)];
        let bundle = contract_bundle(BundleAccount::new(
            Some(parent_contract()),
            Some(recreated_contract()),
            storage_writes(&recreated_slots),
            AccountStatus::DestroyedChanged,
        ));

        let outcome = chain.finish(bundle);
        assert_eq!(
            outcome.state_root,
            expected_root(Some((recreated_contract(), &recreated_slots)))
        );
        assert_eq!(
            outcome_hashed_state(&outcome)
                .storages
                .get(&keccak256(CONTRACT))
                .map(|storage| storage.storage.clone()),
            Some(
                [
                    (hashed_slot(1), U256::from(5)),
                    (hashed_slot(2), U256::ZERO),
                    (hashed_slot(3), U256::from(9))
                ]
                .into_iter()
                .collect()
            ),
            "slot 2, written by an in-memory ancestor, must be zeroed, and the re-created \
             contract's writes must win"
        );
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

        // Block 2 destroys the contract and block 3 puts one back at its address. Block 2 enters
        // the overlay with the outcome's hashed state, which the engine keeps in memory and later
        // persists: its zeroes for the persisted slot 1 and the in-memory slot 2 are what keep the
        // old slots out of block 3's root. reth's raw conversion carries none, and its sparse task
        // only reaches them through its serial fallback.
        let mut chain = Chain::parent_state();
        let outcome = chain.finish(destroyed);
        assert_eq!(outcome.state_root, expected_root(None));
        chain.push(outcome_hashed_state(&outcome), (*outcome.trie_updates).clone());
        let outcome = chain.finish(recreated);
        assert_eq!(
            outcome.state_root,
            expected_root(Some((recreated_contract(), &recreated_slots)))
        );
    }

    #[test]
    fn overrides_only_the_synchronous_job() {
        let synchronous = |config: &TreeConfig| strategy(config, TAIKO_DEVNET.clone()).synchronous;
        let task = TreeConfig::default().with_has_enough_parallelism(true);
        assert!(!synchronous(&task));
        assert!(synchronous(&task.clone().with_state_root_fallback(true)));
        assert!(synchronous(&task.clone().with_has_enough_parallelism(false)));
        assert!(!synchronous(&task.with_has_enough_parallelism(false).with_skip_state_root(true)));
    }

    /// Strategy over a test provider for `config` and `chain_spec`.
    fn strategy(
        config: &TreeConfig,
        chain_spec: Arc<TaikoChainSpec>,
    ) -> TaikoStateRootStrategy<ProviderFactory<MockNodeTypesWithDB>, EthPrimitives> {
        TaikoStateRootStrategy::new(
            config,
            chain_spec,
            create_test_provider_factory(),
            OverlayManager::<EthPrimitives>::default(),
        )
    }

    /// Devnet chain spec with Unzen activating at timestamp 10.
    fn unzen_at_10() -> Arc<TaikoChainSpec> {
        let mut chain_spec = TAIKO_DEVNET.as_ref().clone();
        chain_spec.inner.hardforks.insert(TaikoHardfork::Unzen, ForkCondition::Timestamp(10));
        Arc::new(chain_spec)
    }

    #[test]
    fn roots_pre_unzen_blocks_through_the_provider() {
        let task = TreeConfig::default().with_has_enough_parallelism(true);
        let sparse = strategy(&task, unzen_at_10());
        assert!(sparse.roots_through_provider(9));
        assert!(!sparse.roots_through_provider(10));
        let fallback = strategy(&task.clone().with_state_root_fallback(true), unzen_at_10());
        assert!(fallback.roots_through_provider(10));
        let skipped = strategy(&task.with_skip_state_root(true), unzen_at_10());
        assert!(!skipped.roots_through_provider(9));
    }

    #[test]
    fn payload_builder_shares_the_sparse_trie_only_from_unzen() {
        let strategy = strategy(&TreeConfig::default(), unzen_at_10());
        assert!(!strategy.payload_builder_may_share_sparse_trie(9));
        assert!(strategy.payload_builder_may_share_sparse_trie(10));
    }
}
