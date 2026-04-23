//! Background sync catchup loop and batch-processing helpers for [`ProofsExEx`].
//!
//! When the ExEx is far behind the node tip, it drains blocks from the provider in
//! fixed-size batches rather than processing each notification inline. The logic here
//! is the counterpart of the real-time path in [`ProofsExEx::handle_chain_committed`]
//! — mirror op-reth's structure so invariants such as batch sizes and idle sleep
//! durations remain aligned.
//!
//! [`ProofsExEx`]: crate::ProofsExEx
//! [`ProofsExEx::handle_chain_committed`]: crate::ProofsExEx::handle_chain_committed

use std::time::Duration;

use alethia_reth_proofs_trie::{LiveTrieCollector, ProofsProviderRO, ProofsStorage, ProofsStore};
use reth_node_api::FullNodeComponents;
use reth_provider::{BlockReader, TransactionVariant};
use tokio::{sync::watch, task, time};
use tracing::{debug, error};

use crate::{SYNC_BLOCKS_BATCH_SIZE, SYNC_IDLE_SLEEP_SECS};

/// Background sync loop that processes blocks up to the target.
///
/// Ticks whenever `sync_target_rx` receives a new tip and drains the gap in
/// [`SYNC_BLOCKS_BATCH_SIZE`]-sized batches, yielding between batches so other tasks
/// (notifications, pruner, metrics) get a fair share of the runtime.
pub(crate) async fn sync_loop<Node, Storage>(
    mut sync_target_rx: watch::Receiver<u64>,
    storage: ProofsStorage<Storage>,
    provider: Node::Provider,
    collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
) where
    Node: FullNodeComponents,
    Storage: ProofsStore + Clone + 'static,
{
    debug!(target: "alethia::exex::proofs", "Starting proofs storage sync loop");

    loop {
        let target = *sync_target_rx.borrow_and_update();
        let latest = match storage.provider_ro().and_then(|p| p.get_latest_block_number()) {
            Ok(Some((n, _))) => n,
            Ok(None) => {
                error!(
                    target: "alethia::exex::proofs",
                    "No blocks stored in proofs storage during sync loop"
                );
                continue;
            }
            Err(e) => {
                error!(target: "alethia::exex::proofs", error = ?e, "Failed to get latest block");
                continue;
            }
        };

        if latest >= target {
            time::sleep(Duration::from_secs(SYNC_IDLE_SLEEP_SECS)).await;
            continue;
        }

        // Process one batch.
        if let Err(e) = process_batch::<Node, Storage>(
            latest,
            target,
            &provider,
            collector,
            SYNC_BLOCKS_BATCH_SIZE,
        ) {
            error!(target: "alethia::exex::proofs", error = ?e, "Batch processing failed");
        }

        // Yield to allow other tasks to run.
        debug!(
            target: "alethia::exex::proofs",
            latest_stored = latest,
            target,
            "Batch processed, yielding"
        );
        task::yield_now().await;
    }
}

/// Process a batch of blocks from `start` to `target` (up to `batch_size`).
pub(crate) fn process_batch<Node, Storage>(
    start: u64,
    target: u64,
    provider: &Node::Provider,
    collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
    batch_size: usize,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Storage: ProofsStore + Clone + 'static,
{
    let end = (start + batch_size as u64).min(target);
    debug!(
        target: "alethia::exex::proofs",
        start,
        end,
        "Processing proofs storage sync batch"
    );

    for block_num in (start + 1)..=end {
        let block = provider
            .recovered_block(block_num.into(), TransactionVariant::NoHash)?
            .ok_or_else(|| eyre::eyre!("Missing block {}", block_num))?;

        collector.execute_and_store_block_updates(&block)?;
    }

    Ok(())
}
