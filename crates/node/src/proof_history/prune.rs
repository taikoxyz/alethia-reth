//! Persisted-finality-aware pruning for V2 proof-history storage.

use alloy_consensus::BlockHeader;
use alloy_eips::{BlockNumHash, eip1898::BlockWithParent};
use alloy_primitives::B256;
use reth::providers::DatabaseProviderFactory;
use reth_optimism_trie::{
    OpProofsProviderRO, OpProofsProviderRw, OpProofsStore, api::ProofWindowRange,
};
use reth_storage_api::{ChainStateBlockReader, HeaderProvider};

/// Maximum number of proof-history blocks advanced by one periodic prune transaction.
const FINALITY_PRUNE_BATCH_SIZE: u64 = 50;

/// Result of one persisted-finality-aware proof-history prune tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FinalityPruneOutcome {
    /// Persisted finality does not yet identify a safe retention boundary.
    MissingFinality,
    /// The finalized retention boundary does not require a safe forward move.
    UpToDate,
    /// Stored endpoints or target headers do not match one pinned canonical snapshot.
    CanonicalMismatch,
    /// One transaction advanced the retained proof-history boundary.
    Pruned {
        /// Previously retained earliest block.
        from: BlockNumHash,
        /// Newly retained earliest canonical block.
        to: BlockNumHash,
    },
}

/// Hash and parent-hash facts read for one header from a pinned canonical snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CanonicalPruneHeader {
    /// Canonical hash at the requested block number.
    hash: B256,
    /// Parent hash committed by that canonical header.
    parent_hash: B256,
}

impl CanonicalPruneHeader {
    /// Captures the header facts needed to validate a prune boundary.
    const fn new(hash: B256, parent_hash: B256) -> Self {
        Self { hash, parent_hash }
    }
}

/// Prunes V2 proof history only behind the main database's persisted finalized head.
#[derive(Debug)]
pub(super) struct FinalityProofHistoryPruner<Storage, Provider> {
    /// Proof-history store whose single write transaction owns each tick's retained bounds.
    storage: Storage,
    /// Main provider factory used to pin all canonical and finality reads for a tick.
    provider: Provider,
    /// Number of finalized blocks retained behind the persisted finalized head.
    window: u64,
}

impl<Storage, Provider> FinalityProofHistoryPruner<Storage, Provider> {
    /// Creates a one-pass pruner for the configured finalized retention window.
    pub(super) const fn new(storage: Storage, provider: Provider, window: u64) -> Self {
        Self { storage, provider, window }
    }
}

impl<Storage, Provider> FinalityProofHistoryPruner<Storage, Provider>
where
    Storage: OpProofsStore,
    Provider: DatabaseProviderFactory,
    Provider::Provider: ChainStateBlockReader + HeaderProvider,
{
    /// Executes at most one 50-block prune transaction.
    ///
    /// The proof write transaction is opened first and supplies both retained endpoints. One
    /// subsequently opened main-database read transaction supplies persisted finality and every
    /// canonical header. Retryable absence or canonical disagreement drops the proof transaction
    /// without committing.
    pub(super) fn run_once(&self) -> eyre::Result<FinalityPruneOutcome> {
        let proof_rw = self.storage.provider_rw()?;
        let proof_window = proof_rw.get_proof_window()?;
        let canonical_snapshot = self.provider.database_provider_ro()?;
        prune_with_pinned_snapshot(proof_rw, proof_window, &canonical_snapshot, self.window)
    }
}

/// Applies one prune decision using canonical facts from the supplied pinned main-DB provider.
fn prune_with_pinned_snapshot<RW, Provider>(
    proof_rw: RW,
    proof_window: ProofWindowRange,
    canonical_snapshot: &Provider,
    window: u64,
) -> eyre::Result<FinalityPruneOutcome>
where
    RW: OpProofsProviderRw,
    Provider: ChainStateBlockReader + HeaderProvider,
{
    prune_with_canonical_readers(
        proof_rw,
        proof_window,
        window,
        || Ok(canonical_snapshot.last_finalized_block_number()?),
        |number| canonical_prune_header(canonical_snapshot, number),
    )
}

/// Computes the number of blocks exposed to automatic pruning at startup.
///
/// The finalized target is capped strictly below the stored latest block so the safety threshold
/// remains meaningful even when persisted finality is ahead of the proof window. Both stored
/// endpoints must match the supplied pinned canonical snapshot.
pub(super) fn startup_prune_exposure<Provider>(
    proof_window: ProofWindowRange,
    canonical_snapshot: &Provider,
    window: u64,
) -> eyre::Result<u64>
where
    Provider: ChainStateBlockReader + HeaderProvider,
{
    let Some(finalized) = canonical_snapshot.last_finalized_block_number()? else {
        return Ok(0);
    };
    let mut read_header = |number| canonical_prune_header(canonical_snapshot, number);
    if !canonical_endpoints_match(proof_window, &mut read_header)? {
        return Err(eyre::eyre!(
            "proof-history endpoints changed while calculating finalized startup prune exposure"
        ));
    }

    let finalized_target = finalized.saturating_sub(window);
    let capped_target = finalized_target.min(proof_window.latest.number.saturating_sub(1));
    Ok(capped_target.saturating_sub(proof_window.earliest.number))
}

/// Reads the hash facts for one numbered header from a pinned canonical provider.
fn canonical_prune_header<Provider>(
    canonical_snapshot: &Provider,
    number: u64,
) -> eyre::Result<Option<CanonicalPruneHeader>>
where
    Provider: HeaderProvider,
{
    Ok(canonical_snapshot
        .sealed_header(number)?
        .map(|header| CanonicalPruneHeader::new(header.hash(), header.parent_hash())))
}

/// Checks both stored proof endpoints against one logical canonical header reader.
fn canonical_endpoints_match<ReadHeader>(
    proof_window: ProofWindowRange,
    read_header: &mut ReadHeader,
) -> eyre::Result<bool>
where
    ReadHeader: FnMut(u64) -> eyre::Result<Option<CanonicalPruneHeader>>,
{
    let canonical_earliest = read_header(proof_window.earliest.number)?;
    let canonical_latest = read_header(proof_window.latest.number)?;
    Ok(canonical_earliest.map(|header| header.hash) == Some(proof_window.earliest.hash) &&
        canonical_latest.map(|header| header.hash) == Some(proof_window.latest.hash))
}

/// Resolves and commits one prune batch from one logical canonical snapshot.
///
/// The reader arguments are a narrow seam for testing retryable missing and mismatch states. In
/// production both readers borrow the same pinned main-database provider passed to
/// [`prune_with_pinned_snapshot`]. Returning before `commit` deliberately rolls back the proof RW
/// transaction.
fn prune_with_canonical_readers<RW, ReadFinalized, ReadHeader>(
    proof_rw: RW,
    proof_window: ProofWindowRange,
    window: u64,
    read_finalized: ReadFinalized,
    mut read_header: ReadHeader,
) -> eyre::Result<FinalityPruneOutcome>
where
    RW: OpProofsProviderRw,
    ReadFinalized: FnOnce() -> eyre::Result<Option<u64>>,
    ReadHeader: FnMut(u64) -> eyre::Result<Option<CanonicalPruneHeader>>,
{
    let Some(finalized) = read_finalized()? else {
        return Ok(FinalityPruneOutcome::MissingFinality);
    };

    if !canonical_endpoints_match(proof_window, &mut read_header)? {
        return Ok(FinalityPruneOutcome::CanonicalMismatch);
    }

    let desired = finalized.saturating_sub(window);
    if desired <= proof_window.earliest.number || desired >= proof_window.latest.number {
        return Ok(FinalityPruneOutcome::UpToDate);
    }

    let target =
        proof_window.earliest.number.saturating_add(FINALITY_PRUNE_BATCH_SIZE).min(desired);
    let Some(parent) = read_header(target - 1)? else {
        return Ok(FinalityPruneOutcome::CanonicalMismatch);
    };
    let Some(target_header) = read_header(target)? else {
        return Ok(FinalityPruneOutcome::CanonicalMismatch);
    };
    if target_header.parent_hash != parent.hash {
        return Ok(FinalityPruneOutcome::CanonicalMismatch);
    }

    let target_block = BlockNumHash::new(target, target_header.hash);
    proof_rw.prune_earliest_state(BlockWithParent::new(parent.hash, target_block))?;
    proof_rw.commit()?;
    Ok(FinalityPruneOutcome::Pruned { from: proof_window.earliest, to: target_block })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
    use alloy_primitives::{B256, U256};
    use reth::providers::{BlockWriter, DatabaseProviderFactory};
    use reth_db::Database;
    use reth_db_common::init::init_genesis;
    use reth_ethereum_primitives::{Block, BlockBody};
    use reth_optimism_trie::{
        BlockStateDiff, InitializationJob, MdbxProofsStorageV2, OpProofsProviderRO,
        OpProofsProviderRw, OpProofsStore, RethTrieStorageLayout,
    };
    use reth_primitives_traits::RecoveredBlock;
    use reth_provider::{
        ProviderFactory,
        test_utils::{MockNodeTypesWithDB, create_test_provider_factory},
    };
    use reth_storage_api::{ChainStateBlockWriter, StorageSettingsCache};
    use std::{path::PathBuf, sync::Arc};

    /// Real main-database and V2 proof-history fixture with deterministic empty blocks.
    struct PruneFixture {
        /// Canonical database factory used by the pruner.
        factory: ProviderFactory<MockNodeTypesWithDB>,
        /// Real V2 MDBX proof-history store.
        storage: Arc<MdbxProofsStorageV2>,
        /// Canonical hashes indexed by block number.
        blocks: Vec<BlockNumHash>,
        /// Kept path for diagnostics and MDBX lifetime clarity.
        _proof_path: PathBuf,
    }

    impl PruneFixture {
        /// Creates a canonical chain and a matching proof window spanning genesis through `latest`.
        fn new(latest: u64) -> Self {
            let factory = create_test_provider_factory();
            let genesis_hash = init_genesis(&factory).expect("genesis initializes");
            let mut blocks = vec![BlockNumHash::new(0, genesis_hash)];

            let provider = factory.provider_rw().expect("write provider opens");
            for number in 1..=latest {
                let parent_hash = blocks.last().expect("parent exists").hash;
                let block = RecoveredBlock::new_unhashed(
                    Block {
                        header: Header {
                            parent_hash,
                            number,
                            timestamp: number,
                            difficulty: U256::from(number),
                            ..Default::default()
                        },
                        body: BlockBody::default(),
                    },
                    Vec::new(),
                );
                provider.insert_block(&block).expect("canonical block inserts");
                blocks.push(BlockNumHash::new(number, block.hash()));
            }
            provider.commit().expect("canonical chain commits");

            let proof_path = tempfile::tempdir().expect("proof tempdir").keep();
            let storage =
                Arc::new(MdbxProofsStorageV2::new(&proof_path).expect("V2 proof storage opens"));
            let layout = if factory.cached_storage_settings().is_v2() {
                RethTrieStorageLayout::Packed
            } else {
                RethTrieStorageLayout::Legacy
            };
            InitializationJob::new(
                storage.clone(),
                factory.db_ref().tx().expect("main read transaction opens"),
                layout,
            )
            .run(0, genesis_hash)
            .expect("proof storage initializes at genesis");

            let proof_rw = storage.provider_rw().expect("proof write provider opens");
            for block in blocks.iter().copied().skip(1) {
                let parent = blocks[block.number.saturating_sub(1) as usize].hash;
                proof_rw
                    .store_trie_updates(
                        BlockWithParent::new(parent, block),
                        BlockStateDiff::default(),
                    )
                    .expect("empty proof update stores");
            }
            proof_rw.commit().expect("proof window commits");

            Self { factory, storage, blocks, _proof_path: proof_path }
        }

        /// Persists the finalized block number in the real canonical database.
        fn persist_finalized(&self, finalized: u64) {
            let provider = self.factory.provider_rw().expect("write provider opens");
            provider.save_finalized_block_number(finalized).expect("finality persists");
            provider.commit().expect("finality commits");
        }

        /// Reads the committed proof-history earliest block from a fresh transaction.
        fn earliest(&self) -> NumHash {
            self.storage
                .provider_ro()
                .expect("proof read provider opens")
                .get_proof_window()
                .expect("proof window exists")
                .earliest
        }

        /// Returns the canonical header facts needed by the deterministic reader seam.
        fn canonical_header(&self, number: u64) -> Option<CanonicalPruneHeader> {
            let block = *self.blocks.get(number as usize)?;
            let parent_hash = number
                .checked_sub(1)
                .and_then(|parent| self.blocks.get(parent as usize))
                .map_or(B256::ZERO, |parent| parent.hash);
            Some(CanonicalPruneHeader::new(block.hash, parent_hash))
        }

        /// Constructs the finality-aware pruner under test.
        fn pruner(
            &self,
            window: u64,
        ) -> FinalityProofHistoryPruner<
            Arc<MdbxProofsStorageV2>,
            ProviderFactory<MockNodeTypesWithDB>,
        > {
            FinalityProofHistoryPruner::new(self.storage.clone(), self.factory.clone(), window)
        }
    }

    #[test]
    fn prune_without_persisted_finality_is_noop() {
        let fixture = PruneFixture::new(60);
        let before = fixture.earliest();

        let outcome = fixture.pruner(10).run_once().expect("missing finality is retryable");

        assert_eq!(outcome, FinalityPruneOutcome::MissingFinality);
        assert_eq!(fixture.earliest(), before);
    }

    #[test]
    fn prune_target_uses_finalized_head_not_latest() {
        let fixture = PruneFixture::new(120);
        fixture.persist_finalized(40);

        let outcome = fixture.pruner(20).run_once().expect("prune tick succeeds");

        assert_eq!(fixture.earliest(), fixture.blocks[20]);
        assert_eq!(
            outcome,
            FinalityPruneOutcome::Pruned { from: fixture.blocks[0], to: fixture.blocks[20] }
        );
    }

    #[test]
    fn one_prune_tick_advances_earliest_by_at_most_fifty() {
        let fixture = PruneFixture::new(120);
        fixture.persist_finalized(119);

        fixture.pruner(0).run_once().expect("prune tick succeeds");

        assert_eq!(fixture.earliest(), fixture.blocks[50]);
    }

    #[test]
    fn prune_does_not_reach_or_cross_latest() {
        let fixture = PruneFixture::new(40);
        fixture.persist_finalized(60);
        let before = fixture.earliest();

        let outcome = fixture.pruner(0).run_once().expect("unsafe target is a no-op");

        assert_eq!(outcome, FinalityPruneOutcome::UpToDate);
        assert_eq!(fixture.earliest(), before);
    }

    #[test]
    fn noncanonical_earliest_aborts_without_commit() {
        let fixture = PruneFixture::new(80);
        let before = fixture.earliest();
        let proof_rw = fixture.storage.provider_rw().expect("proof write provider opens");
        let proof_window = proof_rw.get_proof_window().expect("proof window exists");

        let outcome = prune_with_canonical_readers(
            proof_rw,
            proof_window,
            10,
            || Ok(Some(70)),
            |number| {
                let mut header = fixture.canonical_header(number);
                if number == proof_window.earliest.number {
                    header = Some(CanonicalPruneHeader::new(B256::repeat_byte(0xEE), B256::ZERO));
                }
                Ok(header)
            },
        )
        .expect("canonical mismatch is retryable");

        assert_eq!(outcome, FinalityPruneOutcome::CanonicalMismatch);
        assert_eq!(fixture.earliest(), before);
    }

    #[test]
    fn noncanonical_latest_aborts_without_commit() {
        let fixture = PruneFixture::new(80);
        let before = fixture.earliest();
        let proof_rw = fixture.storage.provider_rw().expect("proof write provider opens");
        let proof_window = proof_rw.get_proof_window().expect("proof window exists");

        let outcome = prune_with_canonical_readers(
            proof_rw,
            proof_window,
            10,
            || Ok(Some(70)),
            |number| {
                let mut header = fixture.canonical_header(number);
                if number == proof_window.latest.number {
                    header = Some(CanonicalPruneHeader::new(
                        B256::repeat_byte(0xDD),
                        fixture.blocks[(number - 1) as usize].hash,
                    ));
                }
                Ok(header)
            },
        )
        .expect("canonical mismatch is retryable");

        assert_eq!(outcome, FinalityPruneOutcome::CanonicalMismatch);
        assert_eq!(fixture.earliest(), before);
    }

    #[test]
    fn missing_target_header_aborts_without_commit() {
        let fixture = PruneFixture::new(100);
        let before = fixture.earliest();
        let proof_rw = fixture.storage.provider_rw().expect("proof write provider opens");
        let proof_window = proof_rw.get_proof_window().expect("proof window exists");

        let outcome = prune_with_canonical_readers(
            proof_rw,
            proof_window,
            10,
            || Ok(Some(70)),
            |number| {
                if number == 50 { Ok(None) } else { Ok(fixture.canonical_header(number)) }
            },
        )
        .expect("missing target header is retryable");

        assert_eq!(outcome, FinalityPruneOutcome::CanonicalMismatch);
        assert_eq!(fixture.earliest(), before);
    }

    #[test]
    fn broken_target_parent_continuity_aborts_without_commit() {
        let fixture = PruneFixture::new(100);
        let before = fixture.earliest();
        let proof_rw = fixture.storage.provider_rw().expect("proof write provider opens");
        let proof_window = proof_rw.get_proof_window().expect("proof window exists");

        let outcome = prune_with_canonical_readers(
            proof_rw,
            proof_window,
            10,
            || Ok(Some(70)),
            |number| {
                if number == 50 {
                    Ok(Some(CanonicalPruneHeader::new(
                        fixture.blocks[50].hash,
                        B256::repeat_byte(0xCC),
                    )))
                } else {
                    Ok(fixture.canonical_header(number))
                }
            },
        )
        .expect("broken parent continuity is retryable");

        assert_eq!(outcome, FinalityPruneOutcome::CanonicalMismatch);
        assert_eq!(fixture.earliest(), before);
    }

    #[test]
    fn prune_uses_one_pinned_canonical_snapshot() {
        let fixture = PruneFixture::new(100);
        fixture.persist_finalized(80);
        let proof_rw = fixture.storage.provider_rw().expect("proof write provider opens");
        let proof_window = proof_rw.get_proof_window().expect("proof window exists");
        let canonical_snapshot =
            fixture.factory.database_provider_ro().expect("pinned canonical snapshot opens");

        // Move persisted finality after the snapshot is pinned. The prune must consistently use
        // the old snapshot (target 50), not reopen and observe the new target of zero.
        fixture.persist_finalized(10);
        let outcome = prune_with_pinned_snapshot(proof_rw, proof_window, &canonical_snapshot, 20)
            .expect("pinned-snapshot prune succeeds");

        assert_eq!(fixture.earliest(), fixture.blocks[50]);
        assert_eq!(
            outcome,
            FinalityPruneOutcome::Pruned { from: fixture.blocks[0], to: fixture.blocks[50] }
        );
    }

    #[test]
    fn startup_prune_exposure_uses_persisted_finality_not_latest() {
        let fixture = PruneFixture::new(100);
        fixture.persist_finalized(40);
        let proof_window = fixture
            .storage
            .provider_ro()
            .expect("proof read provider opens")
            .get_proof_window()
            .expect("proof window exists");
        let canonical_snapshot =
            fixture.factory.database_provider_ro().expect("canonical snapshot opens");

        let exposure = startup_prune_exposure(proof_window, &canonical_snapshot, 20)
            .expect("startup exposure resolves");

        assert_eq!(exposure, 20);
    }

    #[test]
    fn startup_prune_exposure_caps_target_below_latest() {
        let fixture = PruneFixture::new(40);
        fixture.persist_finalized(60);
        let proof_window = fixture
            .storage
            .provider_ro()
            .expect("proof read provider opens")
            .get_proof_window()
            .expect("proof window exists");
        let canonical_snapshot =
            fixture.factory.database_provider_ro().expect("canonical snapshot opens");

        let exposure = startup_prune_exposure(proof_window, &canonical_snapshot, 0)
            .expect("startup exposure resolves");

        assert_eq!(exposure, 39);
    }
}
