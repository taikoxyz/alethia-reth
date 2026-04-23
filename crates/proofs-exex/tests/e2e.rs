//! End-to-end sidecar ingestion + query.
//!
//! Drives a synthetic canonical commit through the full [`ProofsExEx::run`] loop and
//! verifies that:
//!
//! 1. The sidecar commits the new block (observable via the `FinishedHeight` event), which —
//!    because [`ProofsExEx::handle_notification`] emits `FinishedHeight` only after the sidecar
//!    write — proves the commit-then-event ordering invariant.
//! 2. Historical state queries via [`ProofsStateProviderRef`] return different values at block 0
//!    (baseline) and block 1 (post-commit), demonstrating versioning.
//!
//! The unit tests in `crates/proofs-exex/src/lib.rs` already exercise the individual
//! handler methods in isolation; this test covers the spawn / notification-stream /
//! event-emission plumbing that those unit tests skip.

use std::{sync::Arc, time::Duration};

use alethia_reth_proofs_exex::ProofsExEx;
use alethia_reth_proofs_trie::{
    BlockStateDiff, MdbxProofsStorage, ProofsProviderRO, ProofsProviderRw, ProofsStateProviderRef,
    ProofsStorage, ProofsStore,
};
use alloy_eips::{NumHash, eip1898::BlockWithParent};
use alloy_primitives::{Address, B256, U256, keccak256, map::B256Map};
use reth_db::test_utils::tempdir_path;
use reth_ethereum_primitives::Block;
use reth_execution_types::{Chain, ExecutionOutcome};
use reth_exex::{ExExEvent, ExExNotification};
use reth_primitives_traits::{Account, RecoveredBlock};
use reth_provider::StateProvider;
use reth_storage_api::noop::NoopProvider;
use reth_trie::{
    HashedPostStateSorted, HashedStorageSorted, LazyTrieData, updates::TrieUpdatesSorted,
};
use std::collections::BTreeMap;

/// Deterministic 32-byte value (repeats `byte` 32 times) used to fabricate block hashes.
fn b256(byte: u8) -> B256 {
    B256::new([byte; 32])
}

/// Deterministic hash for a block number: block 0 -> 0x00.., block 1 -> 0x01..
fn hash_for_num(num: u64) -> B256 {
    b256(num as u8)
}

/// Build a single-block `RecoveredBlock` with deterministic hashes.
fn mk_block(num: u64) -> RecoveredBlock<Block> {
    let mut b: RecoveredBlock<Block> = Default::default();
    b.set_block_number(num);
    b.set_hash(hash_for_num(num));
    b.set_parent_hash(hash_for_num(num.saturating_sub(1)));
    b
}

/// Build a `HashedPostStateSorted` containing one account and one storage slot with
/// the given hashed-storage value.
fn post_state_with_slot(
    hashed_addr: B256,
    hashed_slot: B256,
    slot_value: U256,
) -> HashedPostStateSorted {
    let account = Account { nonce: 1, balance: U256::from(1_000u64), bytecode_hash: None };

    let storage =
        HashedStorageSorted { storage_slots: vec![(hashed_slot, slot_value)], wiped: false };

    let mut storages: B256Map<HashedStorageSorted> = Default::default();
    storages.insert(hashed_addr, storage);

    HashedPostStateSorted::new(vec![(hashed_addr, Some(account))], storages)
}

/// Build a `Chain` wrapping one block with the provided `HashedPostStateSorted` packaged
/// as a ready `LazyTrieData` — exercising the ExEx's fast path.
fn mk_chain_with_state(
    num: u64,
    hashed_state: HashedPostStateSorted,
) -> Chain<reth_ethereum_primitives::EthPrimitives> {
    let block = mk_block(num);
    let mut trie_data = BTreeMap::new();
    trie_data.insert(
        num,
        LazyTrieData::ready(Arc::new(hashed_state), Arc::new(TrieUpdatesSorted::default())),
    );

    let execution_outcome: ExecutionOutcome<reth_ethereum_primitives::Receipt> = ExecutionOutcome {
        bundle: Default::default(),
        receipts: Vec::new(),
        requests: Vec::new(),
        first_block: num,
    };

    Chain::new(vec![block], execution_outcome, trie_data)
}

/// End-to-end: drive a synthetic canonical commit through the ExEx run loop and verify
/// the sidecar serves block-versioned historical state.
#[tokio::test(flavor = "multi_thread")]
async fn sidecar_serves_historical_state_after_exex_ingest() {
    // ------------------------------------------------------------------
    // Fixed, known inputs.
    // ------------------------------------------------------------------
    let address = Address::from([0xAB; 20]);
    let slot_key = B256::from([0xCD; 32]);
    let hashed_addr = keccak256(address.0);
    let hashed_slot = keccak256(slot_key);

    let baseline_value = U256::from(0x1111_u64);
    let updated_value = U256::from(0x2222_u64);

    // ------------------------------------------------------------------
    // 1. Open an MDBX-backed sidecar and seed the block-0 baseline.
    //
    //    We commit the account + slot at block 0 via a regular `store_trie_updates`
    //    (no anchor/init needed) and set the earliest block pointer to genesis so
    //    `ensure_initialized` is satisfied.
    // ------------------------------------------------------------------
    let dir = tempdir_path();
    let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("open mdbx env"));
    let proofs: ProofsStorage<Arc<MdbxProofsStorage>> = store.clone().into();

    {
        let rw = proofs.provider_rw().expect("provider_rw");
        rw.set_earliest_block_number(0, hash_for_num(0)).expect("set earliest");
        rw.store_trie_updates(
            BlockWithParent::new(B256::ZERO, NumHash::new(0, hash_for_num(0))),
            BlockStateDiff {
                sorted_trie_updates: TrieUpdatesSorted::default(),
                sorted_post_state: post_state_with_slot(hashed_addr, hashed_slot, baseline_value),
            },
        )
        .expect("seed block 0");
        rw.commit().expect("commit baseline");
    }

    // ------------------------------------------------------------------
    // 2. Build the ExEx against a reth_exex_test_utils context and spawn the run loop.
    //
    //    The test context inserts only the genesis block on the host node side, so
    //    `best_block_number == 0` and a block-1 notification will take the real-time
    //    fast path (is_sequential && is_near_tip).
    // ------------------------------------------------------------------
    let (ctx, mut handle) =
        reth_exex_test_utils::test_exex_context().await.expect("exex test context");

    let exex = ProofsExEx::builder(ctx, proofs.clone())
        .with_proofs_history_window(10_000)
        .with_proofs_history_prune_interval(Duration::from_secs(3600))
        .with_verification_interval(0)
        .build();

    let exex_task = tokio::spawn(async move { exex.run().await });

    // ------------------------------------------------------------------
    // 3. Send a ChainCommitted notification for block 1 with the updated slot value.
    // ------------------------------------------------------------------
    let block1_state = post_state_with_slot(hashed_addr, hashed_slot, updated_value);
    let block1_chain = mk_chain_with_state(1, block1_state);
    handle
        .notifications_tx
        .send(ExExNotification::ChainCommitted { new: Arc::new(block1_chain) })
        .await
        .expect("send ChainCommitted");

    // ------------------------------------------------------------------
    // 4. Wait for the `FinishedHeight(1)` event. Because the ExEx emits this AFTER persisting the
    //    block to the sidecar (see [`ProofsExEx::handle_notification`]), receiving it guarantees
    //    the sidecar commit is observable below.
    // ------------------------------------------------------------------
    let event = tokio::time::timeout(Duration::from_secs(10), handle.events_rx.recv())
        .await
        .expect("FinishedHeight within 10s")
        .expect("events channel alive");

    match event {
        ExExEvent::FinishedHeight(num_hash) => {
            assert_eq!(num_hash.number, 1, "FinishedHeight should be the committed block number");
            assert_eq!(
                num_hash.hash,
                hash_for_num(1),
                "FinishedHeight hash should match the committed tip"
            );
        }
    }

    // Sanity: the sidecar's latest tracked block now reflects the commit.
    let latest = proofs
        .provider_ro()
        .expect("provider_ro")
        .get_latest_block_number()
        .expect("latest")
        .expect("at least one block stored");
    assert_eq!(latest.0, 1, "sidecar tip should be block 1 after FinishedHeight");

    // ------------------------------------------------------------------
    // 5. Query the sidecar at both block heights and assert the slot value differs — proving
    //    historical state is correctly versioned.
    // ------------------------------------------------------------------
    let noop_latest = || -> Box<dyn StateProvider + Send> { Box::new(NoopProvider::default()) };

    let at_0 = ProofsStateProviderRef::new(
        noop_latest(),
        proofs.provider_ro().expect("provider_ro @0"),
        0,
    );
    let at_1 = ProofsStateProviderRef::new(
        noop_latest(),
        proofs.provider_ro().expect("provider_ro @1"),
        1,
    );

    let v0 = at_0.storage(address, slot_key).expect("read slot @0");
    let v1 = at_1.storage(address, slot_key).expect("read slot @1");

    assert_eq!(v0, Some(baseline_value), "block 0 should see the baseline value");
    assert_eq!(v1, Some(updated_value), "block 1 should see the updated value");
    assert_ne!(v0, v1, "historical slot value must differ between block 0 and block 1");

    // ------------------------------------------------------------------
    // 6. Cleanly close the notification stream. Dropping the sender makes the run loop's
    //    `try_next()` return None, which causes `run()` to resolve Ok(()).
    // ------------------------------------------------------------------
    drop(handle.notifications_tx);
    // The ExEx future should resolve quickly now; if it doesn't, abort to avoid
    // hanging the test on the still-spawned prune/sync background tasks (they live
    // on the ctx's task executor and are torn down when the runtime drops).
    let _ = tokio::time::timeout(Duration::from_secs(5), &mut { exex_task }).await;
}
