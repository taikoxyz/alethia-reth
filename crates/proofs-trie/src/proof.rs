//! Provides proof, state-root, storage-root and witness operation implementations for
//! [`ProofsStore`](crate::ProofsStore) types.

use crate::{
    api::ProofsProviderRO,
    cursor_factory::{ProofsHashedAccountCursorFactory, ProofsTrieCursorFactory},
};
use alloy_primitives::{
    Address, B256, Bytes, keccak256,
    map::{B256Map, HashMap},
};
use reth_execution_errors::{StateProofError, StateRootError, StorageRootError, TrieWitnessError};
use reth_trie::{
    StateRoot, StorageRoot, TrieType,
    hashed_cursor::HashedPostStateCursorFactory,
    metrics::TrieRootMetrics,
    proof::{self, Proof},
    trie_cursor::InMemoryTrieCursorFactory,
    witness::TrieWitness,
};
use reth_trie_common::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedPostStateSorted, HashedStorage,
    MultiProof, MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};

/// Extends [`Proof`] with operations specific for working with [`ProofsStore`](crate::ProofsStore).
pub trait DatabaseProof<P> {
    /// Creates a new `DatabaseProof` instance from external storage.
    fn from_provider(provider: P, block_number: u64) -> Self;

    /// Generates the state proof for target account based on [`TrieInput`].
    fn overlay_account_proof(
        provider: P,
        block_number: u64,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof, StateProofError>;

    /// Generates the state [`MultiProof`] for target hashed account and storage keys.
    fn overlay_multiproof(
        provider: P,
        block_number: u64,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> Result<MultiProof, StateProofError>;
}

impl<P> DatabaseProof<P> for Proof<ProofsTrieCursorFactory<P>, ProofsHashedAccountCursorFactory<P>>
where
    P: ProofsProviderRO + Clone,
{
    /// Create a new [`Proof`] instance from [`ProofsStore`](crate::ProofsStore).
    fn from_provider(provider: P, block_number: u64) -> Self {
        Self::new(
            ProofsTrieCursorFactory::new(provider.clone(), block_number),
            ProofsHashedAccountCursorFactory::new(provider, block_number),
        )
    }

    /// Generates the state proof for target account based on [`TrieInput`].
    fn overlay_account_proof(
        provider: P,
        block_number: u64,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof, StateProofError> {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        Self::from_provider(provider.clone(), block_number)
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(
                ProofsTrieCursorFactory::new(provider.clone(), block_number),
                &nodes_sorted,
            ))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .account_proof(address, slots)
    }

    /// Generates the state [`MultiProof`] for target hashed account and storage keys.
    fn overlay_multiproof(
        provider: P,
        block_number: u64,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> Result<MultiProof, StateProofError> {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        Self::from_provider(provider.clone(), block_number)
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(
                ProofsTrieCursorFactory::new(provider.clone(), block_number),
                &nodes_sorted,
            ))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .multiproof(targets)
    }
}

/// Extends [`StorageProof`] with operations specific for working with
/// [`ProofsStore`](crate::ProofsStore).
pub trait DatabaseStorageProof<P> {
    /// Create a new [`StorageProof`] from [`ProofsStore`](crate::ProofsStore) and account address.
    fn from_provider(provider: P, block_number: u64, address: Address) -> Self;

    /// Generates the storage proof for target slot based on [`TrieInput`].
    fn overlay_storage_proof(
        provider: P,
        block_number: u64,
        address: Address,
        slot: B256,
        storage: HashedStorage,
    ) -> Result<StorageProof, StateProofError>;

    /// Generates the storage multiproof for target slots based on [`TrieInput`].
    fn overlay_storage_multiproof(
        provider: P,
        block_number: u64,
        address: Address,
        slots: &[B256],
        storage: HashedStorage,
    ) -> Result<StorageMultiProof, StateProofError>;
}

impl<P> DatabaseStorageProof<P>
    for proof::StorageProof<
        'static,
        ProofsTrieCursorFactory<P>,
        ProofsHashedAccountCursorFactory<P>,
    >
where
    P: ProofsProviderRO + Clone,
{
    /// Create a new [`StorageProof`] from [`ProofsStore`](crate::ProofsStore) and account address.
    fn from_provider(provider: P, block_number: u64, address: Address) -> Self {
        Self::new(
            ProofsTrieCursorFactory::new(provider.clone(), block_number),
            ProofsHashedAccountCursorFactory::new(provider, block_number),
            address,
        )
    }

    fn overlay_storage_proof(
        provider: P,
        block_number: u64,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> Result<StorageProof, StateProofError> {
        let hashed_address = keccak256(address);
        let prefix_set = hashed_storage.construct_prefix_set();
        let state_sorted = HashedPostStateSorted::new(
            Default::default(),
            HashMap::from_iter([(hashed_address, hashed_storage.into_sorted())]),
        );
        Self::from_provider(provider.clone(), block_number, address)
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ))
            .with_prefix_set_mut(prefix_set)
            .storage_proof(slot)
    }

    fn overlay_storage_multiproof(
        provider: P,
        block_number: u64,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> Result<StorageMultiProof, StateProofError> {
        let hashed_address = keccak256(address);
        let targets = slots.iter().map(keccak256).collect();
        let prefix_set = hashed_storage.construct_prefix_set();
        let state_sorted = HashedPostStateSorted::new(
            Default::default(),
            HashMap::from_iter([(hashed_address, hashed_storage.into_sorted())]),
        );
        Self::from_provider(provider.clone(), block_number, address)
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ))
            .with_prefix_set_mut(prefix_set)
            .storage_multiproof(targets)
    }
}

/// Extends [`StateRoot`] with operations specific for working with
/// [`ProofsStore`](crate::ProofsStore).
pub trait DatabaseStateRoot<P>: Sized {
    /// Calculate the state root for this [`HashedPostState`].
    /// Internally, this method retrieves prefixsets and uses them
    /// to calculate incremental state root.
    ///
    /// # Returns
    ///
    /// The state root for this [`HashedPostState`].
    fn overlay_root(
        provider: P,
        block_number: u64,
        post_state: HashedPostState,
    ) -> Result<B256, StateRootError>;

    /// Calculates the state root for this [`HashedPostState`] and returns it alongside trie
    /// updates. See [`Self::overlay_root`] for more info.
    fn overlay_root_with_updates(
        provider: P,
        block_number: u64,
        post_state: HashedPostState,
    ) -> Result<(B256, TrieUpdates), StateRootError>;

    /// Calculates the state root for provided [`HashedPostState`] using cached intermediate nodes.
    fn overlay_root_from_nodes(
        provider: P,
        block_number: u64,
        input: TrieInput,
    ) -> Result<B256, StateRootError>;

    /// Calculates the state root and trie updates for provided [`HashedPostState`] using
    /// cached intermediate nodes.
    fn overlay_root_from_nodes_with_updates(
        provider: P,
        block_number: u64,
        input: TrieInput,
    ) -> Result<(B256, TrieUpdates), StateRootError>;
}

impl<P> DatabaseStateRoot<P>
    for StateRoot<ProofsTrieCursorFactory<P>, ProofsHashedAccountCursorFactory<P>>
where
    P: ProofsProviderRO + Clone,
{
    fn overlay_root(
        provider: P,
        block_number: u64,
        post_state: HashedPostState,
    ) -> Result<B256, StateRootError> {
        let prefix_sets = post_state.construct_prefix_sets().freeze();
        let state_sorted = post_state.into_sorted();
        StateRoot::new(
            ProofsTrieCursorFactory::new(provider.clone(), block_number),
            HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ),
        )
        .with_prefix_sets(prefix_sets)
        .root()
    }

    fn overlay_root_with_updates(
        provider: P,
        block_number: u64,
        post_state: HashedPostState,
    ) -> Result<(B256, TrieUpdates), StateRootError> {
        let prefix_sets = post_state.construct_prefix_sets().freeze();
        let state_sorted = post_state.into_sorted();
        StateRoot::new(
            ProofsTrieCursorFactory::new(provider.clone(), block_number),
            HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ),
        )
        .with_prefix_sets(prefix_sets)
        .root_with_updates()
    }

    fn overlay_root_from_nodes(
        provider: P,
        block_number: u64,
        input: TrieInput,
    ) -> Result<B256, StateRootError> {
        let state_sorted = input.state.into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        StateRoot::new(
            InMemoryTrieCursorFactory::new(
                ProofsTrieCursorFactory::new(provider.clone(), block_number),
                &nodes_sorted,
            ),
            HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root()
    }

    fn overlay_root_from_nodes_with_updates(
        provider: P,
        block_number: u64,
        input: TrieInput,
    ) -> Result<(B256, TrieUpdates), StateRootError> {
        let state_sorted = input.state.into_sorted();
        let nodes_sorted = input.nodes.into_sorted();
        StateRoot::new(
            InMemoryTrieCursorFactory::new(
                ProofsTrieCursorFactory::new(provider.clone(), block_number),
                &nodes_sorted,
            ),
            HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ),
        )
        .with_prefix_sets(input.prefix_sets.freeze())
        .root_with_updates()
    }
}

/// Extends [`StorageRoot`] with operations specific for working with
/// [`ProofsStore`](crate::ProofsStore).
pub trait DatabaseStorageRoot<P> {
    /// Calculates the storage root for provided [`HashedStorage`].
    fn overlay_root(
        provider: P,
        block_number: u64,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> Result<B256, StorageRootError>;
}

impl<P> DatabaseStorageRoot<P>
    for StorageRoot<ProofsTrieCursorFactory<P>, ProofsHashedAccountCursorFactory<P>>
where
    P: ProofsProviderRO + Clone,
{
    fn overlay_root(
        provider: P,
        block_number: u64,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> Result<B256, StorageRootError> {
        let prefix_set = hashed_storage.construct_prefix_set().freeze();
        let state_sorted =
            HashedPostState::from_hashed_storage(keccak256(address), hashed_storage).into_sorted();
        StorageRoot::new(
            ProofsTrieCursorFactory::new(provider.clone(), block_number),
            HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ),
            address,
            prefix_set,
            TrieRootMetrics::new(TrieType::Custom("alethia_historical_proofs_storage")),
        )
        .root()
    }
}

/// Extends [`TrieWitness`] with operations specific for working with
/// [`ProofsStore`](crate::ProofsStore).
pub trait DatabaseTrieWitness<P> {
    /// Creates a new [`TrieWitness`] instance from [`ProofsStore`](crate::ProofsStore).
    fn from_provider(provider: P, block_number: u64) -> Self;

    /// Generates the trie witness for the target state based on [`TrieInput`].
    fn overlay_witness(
        provider: P,
        block_number: u64,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> Result<B256Map<Bytes>, TrieWitnessError>;
}

impl<P> DatabaseTrieWitness<P>
    for TrieWitness<ProofsTrieCursorFactory<P>, ProofsHashedAccountCursorFactory<P>>
where
    P: ProofsProviderRO + Clone,
{
    fn from_provider(provider: P, block_number: u64) -> Self {
        Self::new(
            ProofsTrieCursorFactory::new(provider.clone(), block_number),
            ProofsHashedAccountCursorFactory::new(provider, block_number),
        )
    }

    fn overlay_witness(
        provider: P,
        block_number: u64,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> Result<B256Map<Bytes>, TrieWitnessError> {
        let nodes_sorted = input.nodes.into_sorted();
        let state_sorted = input.state.into_sorted();
        Self::from_provider(provider.clone(), block_number)
            .with_trie_cursor_factory(InMemoryTrieCursorFactory::new(
                ProofsTrieCursorFactory::new(provider.clone(), block_number),
                &nodes_sorted,
            ))
            .with_hashed_cursor_factory(HashedPostStateCursorFactory::new(
                ProofsHashedAccountCursorFactory::new(provider, block_number),
                &state_sorted,
            ))
            .with_prefix_sets_mut(input.prefix_sets)
            .with_execution_witness_mode(mode)
            .always_include_root_node()
            .compute(target)
    }
}
