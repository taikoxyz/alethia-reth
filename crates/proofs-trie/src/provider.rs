//! Provider for external proofs storage.

use crate::{
    api::ProofsProviderRO,
    error::ProofsStorageError,
    proof::{
        DatabaseProof, DatabaseStateRoot, DatabaseStorageProof, DatabaseStorageRoot,
        DatabaseTrieWitness,
    },
};
use alloy_primitives::keccak256;
use derive_more::Constructor;
use reth_primitives_traits::{Account, Bytecode};
use reth_provider::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, ProviderError,
    ProviderResult, StateProofProvider, StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_revm::{
    db::BundleState,
    primitives::{Address, B256, Bytes, StorageValue, alloy_primitives::BlockNumber},
};
use reth_trie::{
    StateRoot, StorageRoot,
    hashed_cursor::HashedCursor,
    proof::{self, Proof},
    witness::TrieWitness,
};
use reth_trie_common::{
    AccountProof, ExecutionWitnessMode, HashedPostState, HashedStorage, KeccakKeyHasher,
    MultiProof, MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
    updates::TrieUpdates,
};
use std::fmt::Debug;

/// State provider for external proofs storage.
#[derive(Constructor)]
pub struct ProofsStateProviderRef<'a, P> {
    /// Historical state provider used for non-state (block hash / bytecode) lookups.
    latest: Box<dyn StateProvider + Send + 'a>,

    /// Proofs storage provider for versioned state lookups.
    provider: P,

    /// Max block number that can be used for state lookups.
    block_number: BlockNumber,
}

impl<P> Debug for ProofsStateProviderRef<'_, P>
where
    P: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofsStateProviderRef")
            .field("provider", &self.provider)
            .field("block_number", &self.block_number)
            .finish()
    }
}

impl From<ProofsStorageError> for ProviderError {
    fn from(error: ProofsStorageError) -> Self {
        Self::other(error)
    }
}

impl<P> BlockHashReader for ProofsStateProviderRef<'_, P> {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.latest.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.latest.canonical_hashes_range(start, end)
    }
}

impl<P> StateRootProvider for ProofsStateProviderRef<'_, P>
where
    P: ProofsProviderRO + Clone,
{
    fn state_root(&self, state: HashedPostState) -> ProviderResult<B256> {
        Ok(StateRoot::overlay_root(self.provider.clone(), self.block_number, state)?)
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        Ok(StateRoot::overlay_root_from_nodes(self.provider.clone(), self.block_number, input)?)
    }

    fn state_root_with_updates(
        &self,
        state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok(StateRoot::overlay_root_with_updates(self.provider.clone(), self.block_number, state)?)
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok(StateRoot::overlay_root_from_nodes_with_updates(
            self.provider.clone(),
            self.block_number,
            input,
        )?)
    }
}

impl<P> StorageRootProvider for ProofsStateProviderRef<'_, P>
where
    P: ProofsProviderRO + Clone,
{
    fn storage_root(&self, address: Address, storage: HashedStorage) -> ProviderResult<B256> {
        StorageRoot::overlay_root(self.provider.clone(), self.block_number, address, storage)
            .map_err(|err| ProviderError::Database(err.into()))
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        proof::StorageProof::overlay_storage_proof(
            self.provider.clone(),
            self.block_number,
            address,
            slot,
            storage,
        )
        .map_err(ProviderError::from)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        proof::StorageProof::overlay_storage_multiproof(
            self.provider.clone(),
            self.block_number,
            address,
            slots,
            storage,
        )
        .map_err(ProviderError::from)
    }
}

impl<P> StateProofProvider for ProofsStateProviderRef<'_, P>
where
    P: ProofsProviderRO + Clone,
{
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Proof::overlay_account_proof(
            self.provider.clone(),
            self.block_number,
            input,
            address,
            slots,
        )
        .map_err(ProviderError::from)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Proof::overlay_multiproof(self.provider.clone(), self.block_number, input, targets)
            .map_err(ProviderError::from)
    }

    fn witness(
        &self,
        input: TrieInput,
        target: HashedPostState,
        mode: ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        TrieWitness::overlay_witness(self.provider.clone(), self.block_number, input, target, mode)
            .map_err(ProviderError::from)
            .map(|hm| hm.into_values().collect())
    }
}

impl<P> HashedPostStateProvider for ProofsStateProviderRef<'_, P> {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<P> AccountReader for ProofsStateProviderRef<'_, P>
where
    P: ProofsProviderRO,
{
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let hashed_key = keccak256(address.0);
        Ok(self
            .provider
            .account_hashed_cursor(self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, account)| (key == hashed_key).then_some(account)))
    }
}

impl<P> StateProvider for ProofsStateProviderRef<'_, P>
where
    P: ProofsProviderRO + Clone,
{
    fn storage(&self, address: Address, storage_key: B256) -> ProviderResult<Option<StorageValue>> {
        let hashed_key = keccak256(storage_key);
        Ok(self
            .provider
            .storage_hashed_cursor(keccak256(address.0), self.block_number)
            .map_err(Into::<ProviderError>::into)?
            .seek(hashed_key)
            .map_err(Into::<ProviderError>::into)?
            .and_then(|(key, storage)| (key == hashed_key).then_some(storage)))
    }
}

impl<P> BytecodeReader for ProofsStateProviderRef<'_, P> {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.latest.bytecode_by_hash(code_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryProofsStorage, api::ProofsStore};
    use reth_provider::noop::NoopProvider;

    #[test]
    fn test_proofs_state_provider_ref_debug() {
        let latest: Box<dyn StateProvider + Send> = Box::new(NoopProvider::default());
        let storage = InMemoryProofsStorage::new();
        let provider_ro = storage.provider_ro().unwrap();
        let block_number = 42u64;

        let provider = ProofsStateProviderRef::new(latest, provider_ro, block_number);

        assert_eq!(
            format!("{:?}", provider),
            "ProofsStateProviderRef { provider: InMemoryProofsProvider { inner: RwLock { data: InMemoryStorageInner { account_branches: {}, storage_branches: {}, hashed_accounts: {}, hashed_storages: {}, trie_updates: {}, post_states: {}, earliest_block: None, anchor_block: None } } }, block_number: 42 }"
        );
    }
}
