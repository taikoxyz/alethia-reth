//! End-to-end op-reth `OpProofsExEx` ingestion + historical query.
//!
//! Test location rationale: this binary crate already depends on both
//! `reth-optimism-exex` and `reth-optimism-trie` for wiring the sidecar into
//! the node, so placing the test next to the wiring code avoids pulling the
//! op-reth ExEx crate into `crates/rpc/` just for tests.
//!
//! The test drives a synthetic canonical commit through the full
//! [`OpProofsExEx::run`] loop and verifies that:
//!
//! 1. The sidecar commits the new block — observable via a `FinishedHeight` event on the ExEx
//!    events channel. Because op-reth's handler emits `FinishedHeight` only after the sidecar
//!    write, receiving it proves the commit-then-event ordering invariant.
//! 2. Historical state queries via [`OpProofsStateProviderRef`] return different values at block 0
//!    (baseline) and block 1 (post-commit), demonstrating versioning.
//!
//! Unit tests inside `reth-optimism-exex` already exercise the individual
//! handler methods in isolation; this test covers the spawn /
//! notification-stream / event-emission plumbing that those unit tests skip,
//! plus our downstream assumption that
//! `OpProofsStateProviderRef::new(latest, provider_ro, block_number)` returns
//! block-versioned state via its `StateProvider::storage` impl.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use alloy_eips::{NumHash, eip1898::BlockWithParent};
use alloy_primitives::{Address, B256, U256, keccak256, map::B256Map};
use reth_ethereum_primitives::Block;
use reth_execution_types::{Chain, ExecutionOutcome};
use reth_exex::{ExExEvent, ExExNotification};
use reth_optimism_exex::OpProofsExEx;
use reth_optimism_trie::{
    BlockStateDiff, MdbxProofsStorage, OpProofsProviderRO, OpProofsProviderRw, OpProofsStorage,
    OpProofsStore, provider::OpProofsStateProviderRef,
};
use reth_primitives_traits::{Account, RecoveredBlock};
use reth_storage_api::{StateProvider, noop::NoopProvider};
use reth_trie::{
    HashedPostStateSorted, HashedStorageSorted, LazyTrieData, updates::TrieUpdatesSorted,
};

/// Deterministic 32-byte value (repeats `byte` 32 times).
fn b256(byte: u8) -> B256 {
    B256::new([byte; 32])
}

/// Deterministic hash for a block number: block 0 -> 0x00.., block 1 -> 0x01..
fn hash_for_num(num: u64) -> B256 {
    b256(num as u8)
}

/// Build a single-block [`RecoveredBlock`] with deterministic hashes.
fn mk_block(num: u64) -> RecoveredBlock<Block> {
    let mut b: RecoveredBlock<Block> = Default::default();
    b.set_block_number(num);
    b.set_hash(hash_for_num(num));
    b.set_parent_hash(hash_for_num(num.saturating_sub(1)));
    b
}

/// Build a [`HashedPostStateSorted`] with one account + one storage slot.
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

/// Build a [`Chain`] wrapping one block with the provided
/// [`HashedPostStateSorted`] packaged as a ready [`LazyTrieData`] — exercising
/// op-reth's fast path (no EVM execution required).
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

/// End-to-end: drive a synthetic canonical commit through the op-reth
/// [`OpProofsExEx::run`] loop and verify the sidecar serves block-versioned
/// historical state.
#[tokio::test(flavor = "multi_thread")]
async fn op_proofs_exex_serves_historical_state_after_ingest() {
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
    //    We commit the account + slot at block 0 via `store_trie_updates`
    //    and set the earliest block pointer to genesis so op-reth's
    //    `ensure_initialized` is satisfied.
    // ------------------------------------------------------------------
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(MdbxProofsStorage::new(dir.path()).expect("open mdbx env"));
    let proofs: OpProofsStorage<Arc<MdbxProofsStorage>> = store.clone();

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
    //    `best_block_number == 0` and a block-1 notification takes the real-time
    //    fast path (is_sequential && is_near_tip).
    // ------------------------------------------------------------------
    let (ctx, mut handle) =
        reth_exex_test_utils::test_exex_context().await.expect("exex test context");

    let exex = OpProofsExEx::builder(ctx, proofs.clone())
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
    // 4. Wait for `FinishedHeight(1)`. Because op-reth's handler emits this AFTER persisting the
    //    block, receiving it guarantees the sidecar commit is observable below.
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

    let provider_ro_0 = proofs.provider_ro().expect("provider_ro @0");
    let at_0 = OpProofsStateProviderRef::new(noop_latest(), provider_ro_0, 0);
    let provider_ro_1 = proofs.provider_ro().expect("provider_ro @1");
    let at_1 = OpProofsStateProviderRef::new(noop_latest(), provider_ro_1, 1);

    let v0 = at_0.storage(address, slot_key).expect("read slot @0");
    let v1 = at_1.storage(address, slot_key).expect("read slot @1");

    assert_eq!(v0, Some(baseline_value), "block 0 should see the baseline value");
    assert_eq!(v1, Some(updated_value), "block 1 should see the updated value");
    assert_ne!(v0, v1, "historical slot value must differ between block 0 and block 1");

    // ------------------------------------------------------------------
    // 6. Cleanly close the notification stream. Dropping the sender makes the run loop's
    //    `try_next()` return None, resolving `run()` with Ok(()). If the ExEx future doesn't
    //    resolve within 5s we abort — the prune/sync background tasks live on the ctx's task
    //    executor and are torn down when the runtime drops.
    // ------------------------------------------------------------------
    drop(handle.notifications_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), exex_task).await;
}
