use std::{collections::HashMap, time::Duration};

use alethia_reth_db::model::{
    BatchToLastBlock, STORED_L1_HEAD_ORIGIN_KEY, StoredL1HeadOriginTable, StoredL1OriginTable,
};
use eyre::{Context, eyre};
use reth::{
    network::{NetworkInfo, NetworkProtocols, Peers, protocol::IntoRlpxSubProtocol},
    tasks::TaskExecutor,
};
use reth_db_api::{
    cursor::DbCursorRO,
    transaction::{DbTx, DbTxMut},
};
use reth_ethereum::EthPrimitives;
use reth_network_api::{PeerId, test_utils::PeersHandleProvider};
use reth_provider::{
    BlockNumReader, DatabaseProviderFactory,
    providers::{BlockchainProvider, ProviderNodeTypes},
};
use tokio::{sync::mpsc, time::timeout};
use tracing::{debug, trace, warn};

use crate::taiko_aux::{
    connection::TaikoAuxPeerRequest,
    handlers::{ProtocolEvent, ProtocolState, TaikoAuxProtocolHandler},
    message::{AuxBatchLastBlockEntry, AuxL1OriginEntry, AuxRangeRequest},
    provider::RethTaikoAuxProvider,
};

/// Runtime configuration for auxiliary table sync.
#[derive(Clone, Copy, Debug)]
pub struct TaikoAuxSyncConfig {
    /// Maximum number of active `taiko_aux` sessions.
    pub max_active_connections: u64,
    /// Number of rows requested per range request.
    pub request_batch_size: u64,
    /// Timeout applied to each peer request.
    pub request_timeout: Duration,
    /// Poll interval while beacon sync is in progress.
    pub syncing_poll_interval: Duration,
    /// Poll interval while the node is not syncing.
    pub idle_poll_interval: Duration,
}

impl Default for TaikoAuxSyncConfig {
    /// Returns production defaults tuned for low-overhead background backfill.
    fn default() -> Self {
        Self {
            max_active_connections: 8,
            request_batch_size: 1024,
            request_timeout: Duration::from_secs(5),
            syncing_poll_interval: Duration::from_millis(500),
            idle_poll_interval: Duration::from_secs(10),
        }
    }
}

/// Internal storage abstraction used by sync logic for production and tests.
trait TaikoAuxLocalStore {
    /// Returns the local canonical chain head block number.
    fn best_block_number(&self) -> eyre::Result<u64>;
    /// Returns first missing key to request from `StoredL1OriginTable`.
    fn next_l1_origin_start(&self) -> eyre::Result<u64>;
    /// Returns first missing key to request from `BatchToLastBlock`.
    fn next_batch_start(&self) -> eyre::Result<u64>;
    /// Persists fetched `StoredL1OriginTable` rows.
    fn persist_l1_origins(&self, rows: Vec<AuxL1OriginEntry>) -> eyre::Result<()>;
    /// Persists fetched `BatchToLastBlock` rows.
    fn persist_batch_rows(&self, rows: Vec<AuxBatchLastBlockEntry>) -> eyre::Result<()>;
    /// Persists fetched head pointer from `StoredL1HeadOriginTable`.
    fn set_head_l1_origin(&self, head: u64) -> eyre::Result<()>;
}

impl<P> TaikoAuxLocalStore for BlockchainProvider<P>
where
    P: ProviderNodeTypes<Primitives = EthPrimitives>,
{
    /// Reads current local best block number from canonical chain provider.
    fn best_block_number(&self) -> eyre::Result<u64> {
        Ok(BlockNumReader::best_block_number(self)?)
    }

    /// Computes the next L1 origin key to fetch from peers.
    fn next_l1_origin_start(&self) -> eyre::Result<u64> {
        blockchain_next_l1_origin_start(self)
    }

    /// Computes the next batch key to fetch from peers.
    fn next_batch_start(&self) -> eyre::Result<u64> {
        blockchain_next_batch_start(self)
    }

    /// Inserts fetched L1 origin rows into local auxiliary table.
    fn persist_l1_origins(&self, rows: Vec<AuxL1OriginEntry>) -> eyre::Result<()> {
        blockchain_persist_l1_origins(self, rows)
    }

    /// Inserts fetched batch mapping rows into local auxiliary table.
    fn persist_batch_rows(&self, rows: Vec<AuxBatchLastBlockEntry>) -> eyre::Result<()> {
        blockchain_persist_batch_rows(self, rows)
    }

    /// Updates local head L1 origin pointer.
    fn set_head_l1_origin(&self, head: u64) -> eyre::Result<()> {
        blockchain_set_head_l1_origin(self, head)
    }
}

/// Installs `taiko_aux` subprotocol and starts auxiliary table sync task.
pub fn install_taiko_aux_subprotocol<P, N>(
    provider: BlockchainProvider<P>,
    network: N,
    task_executor: TaskExecutor,
    config: TaikoAuxSyncConfig,
) -> eyre::Result<()>
where
    P: ProviderNodeTypes<Primitives = EthPrimitives>,
    N: NetworkInfo + Peers + PeersHandleProvider + NetworkProtocols + Clone + Unpin + 'static,
{
    debug!(target: "reth::taiko::cli", ?config, "Installing taiko_aux subprotocol");

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let protocol_provider = RethTaikoAuxProvider::new(provider.clone());

    network.add_rlpx_sub_protocol(
        TaikoAuxProtocolHandler {
            provider: protocol_provider,
            peers_handle: network.peers_handle().clone(),
            max_active_connections: config.max_active_connections,
            state: ProtocolState::new(events_tx),
        }
        .into_rlpx_sub_protocol(),
    );

    task_executor.spawn(run_taiko_aux_sync(provider, network, events_rx, config));
    Ok(())
}

/// Background worker that discovers peers and incrementally backfills aux tables.
async fn run_taiko_aux_sync<S, N>(
    store: S,
    network: N,
    mut events_rx: mpsc::UnboundedReceiver<ProtocolEvent>,
    config: TaikoAuxSyncConfig,
) where
    S: TaikoAuxLocalStore,
    N: NetworkInfo + Clone + Unpin + 'static,
{
    // Tracks active peer command channels as connections are established/removed.
    let mut peers: HashMap<PeerId, mpsc::UnboundedSender<TaikoAuxPeerRequest>> = HashMap::new();

    loop {
        let poll_interval = if network.is_syncing() {
            config.syncing_poll_interval
        } else {
            config.idle_poll_interval
        };

        tokio::select! {
            maybe_event = events_rx.recv() => {
                match maybe_event {
                    Some(event) => match event {
                        ProtocolEvent::Established { direction, peer_id, to_connection } => {
                            trace!(target: "taiko_aux::sync", %peer_id, ?direction, "taiko_aux peer connected");
                            peers.insert(peer_id, to_connection);
                        }
                        ProtocolEvent::MaxActiveConnectionsExceeded { num_active } => {
                            trace!(target: "taiko_aux::sync", num_active, "max active taiko_aux connections reached");
                        }
                    },
                    None => {
                        warn!(target: "taiko_aux::sync", "taiko_aux protocol event channel closed");
                        break;
                    }
                }
            }
            _ = tokio::time::sleep(poll_interval) => {
                if peers.is_empty() {
                    continue;
                }

                if let Err(error) = sync_from_available_peer(&store, &mut peers, config).await {
                    trace!(target: "taiko_aux::sync", %error, "taiko_aux sync iteration failed");
                }
            }
        }
    }
}

/// Attempts sync against connected peers until one successful pass completes.
async fn sync_from_available_peer<S>(
    store: &S,
    peers: &mut HashMap<PeerId, mpsc::UnboundedSender<TaikoAuxPeerRequest>>,
    config: TaikoAuxSyncConfig,
) -> eyre::Result<()>
where
    S: TaikoAuxLocalStore,
{
    let peer_ids: Vec<_> = peers.keys().copied().collect();

    for peer_id in peer_ids {
        let Some(peer) = peers.get(&peer_id).cloned() else {
            continue;
        };

        match sync_from_peer(store, &peer, config).await {
            Ok(()) => {
                return Ok(());
            }
            Err(error) => {
                trace!(target: "taiko_aux::sync", %peer_id, %error, "peer sync failed, removing peer");
                peers.remove(&peer_id);
            }
        }
    }

    Ok(())
}

/// Runs a full aux-table sync pass against a specific peer.
async fn sync_from_peer<S>(
    store: &S,
    peer: &mpsc::UnboundedSender<TaikoAuxPeerRequest>,
    config: TaikoAuxSyncConfig,
) -> eyre::Result<()>
where
    S: TaikoAuxLocalStore,
{
    let canonical_head = store.best_block_number().wrap_err("failed to fetch local best block")?;

    sync_l1_origins(store, peer, config, canonical_head).await?;
    sync_batch_last_blocks(store, peer, config, canonical_head).await?;
    sync_head_l1_origin(store, peer, config, canonical_head).await?;

    Ok(())
}

/// Backfills `StoredL1OriginTable` rows up to the local canonical head.
async fn sync_l1_origins<S>(
    store: &S,
    peer: &mpsc::UnboundedSender<TaikoAuxPeerRequest>,
    config: TaikoAuxSyncConfig,
    canonical_head: u64,
) -> eyre::Result<()>
where
    S: TaikoAuxLocalStore,
{
    let mut start = store.next_l1_origin_start()?;

    loop {
        let request = AuxRangeRequest { start, limit: config.request_batch_size };
        let rows = request_l1_origins(peer, request, config.request_timeout).await?;

        if rows.is_empty() {
            break;
        }

        let mut accepted = Vec::with_capacity(rows.len());
        let mut next_start = start;
        for row in rows {
            if row.block_number > canonical_head {
                break;
            }
            next_start = row.block_number.saturating_add(1);
            accepted.push(row);
        }

        if accepted.is_empty() {
            break;
        }

        let accepted_len = accepted.len();
        store.persist_l1_origins(accepted)?;
        start = next_start;

        if accepted_len < config.request_batch_size as usize {
            break;
        }
    }

    Ok(())
}

/// Backfills `BatchToLastBlock` rows up to the local canonical head.
async fn sync_batch_last_blocks<S>(
    store: &S,
    peer: &mpsc::UnboundedSender<TaikoAuxPeerRequest>,
    config: TaikoAuxSyncConfig,
    canonical_head: u64,
) -> eyre::Result<()>
where
    S: TaikoAuxLocalStore,
{
    let mut start = store.next_batch_start()?;

    loop {
        let request = AuxRangeRequest { start, limit: config.request_batch_size };
        let rows = request_batch_last_blocks(peer, request, config.request_timeout).await?;

        if rows.is_empty() {
            break;
        }

        let mut accepted = Vec::with_capacity(rows.len());
        let mut next_start = start;
        for row in rows {
            if row.block_number > canonical_head {
                break;
            }
            next_start = row.batch_id.saturating_add(1);
            accepted.push(row);
        }

        if accepted.is_empty() {
            break;
        }

        let accepted_len = accepted.len();
        store.persist_batch_rows(accepted)?;
        start = next_start;

        if accepted_len < config.request_batch_size as usize {
            break;
        }
    }

    Ok(())
}

/// Backfills head L1 origin pointer when peer value is canonical-head safe.
async fn sync_head_l1_origin<S>(
    store: &S,
    peer: &mpsc::UnboundedSender<TaikoAuxPeerRequest>,
    config: TaikoAuxSyncConfig,
    canonical_head: u64,
) -> eyre::Result<()>
where
    S: TaikoAuxLocalStore,
{
    let head = request_head_l1_origin(peer, config.request_timeout).await?;
    let Some(head) = head else {
        return Ok(());
    };

    if head > canonical_head {
        return Ok(());
    }

    store.set_head_l1_origin(head)
}

/// Computes next missing key in local `StoredL1OriginTable`.
fn blockchain_next_l1_origin_start<P>(provider: &BlockchainProvider<P>) -> eyre::Result<u64>
where
    P: ProviderNodeTypes<Primitives = EthPrimitives>,
{
    let db_provider = provider.database_provider_ro()?;
    let mut cursor = db_provider.tx_ref().cursor_read::<StoredL1OriginTable>()?;
    let next = cursor.last()?.map(|(key, _)| key.saturating_add(1)).unwrap_or(0);
    Ok(next)
}

/// Computes next missing key in local `BatchToLastBlock` table.
fn blockchain_next_batch_start<P>(provider: &BlockchainProvider<P>) -> eyre::Result<u64>
where
    P: ProviderNodeTypes<Primitives = EthPrimitives>,
{
    let db_provider = provider.database_provider_ro()?;
    let mut cursor = db_provider.tx_ref().cursor_read::<BatchToLastBlock>()?;
    let next = cursor.last()?.map(|(key, _)| key.saturating_add(1)).unwrap_or(0);
    Ok(next)
}

/// Persists a chunk of L1 origin rows into local database.
fn blockchain_persist_l1_origins<P>(
    provider: &BlockchainProvider<P>,
    rows: Vec<AuxL1OriginEntry>,
) -> eyre::Result<()>
where
    P: ProviderNodeTypes<Primitives = EthPrimitives>,
{
    if rows.is_empty() {
        return Ok(());
    }

    let tx = provider.database_provider_rw()?.into_tx();
    for row in rows {
        let (block_number, value) = row.into_table_row();
        tx.put::<StoredL1OriginTable>(block_number, value)?;
    }
    tx.commit()?;
    Ok(())
}

/// Persists a chunk of batch mapping rows into local database.
fn blockchain_persist_batch_rows<P>(
    provider: &BlockchainProvider<P>,
    rows: Vec<AuxBatchLastBlockEntry>,
) -> eyre::Result<()>
where
    P: ProviderNodeTypes<Primitives = EthPrimitives>,
{
    if rows.is_empty() {
        return Ok(());
    }

    let tx = provider.database_provider_rw()?.into_tx();
    for row in rows {
        tx.put::<BatchToLastBlock>(row.batch_id, row.block_number)?;
    }
    tx.commit()?;
    Ok(())
}

/// Persists the head origin pointer into local database.
fn blockchain_set_head_l1_origin<P>(provider: &BlockchainProvider<P>, head: u64) -> eyre::Result<()>
where
    P: ProviderNodeTypes<Primitives = EthPrimitives>,
{
    let tx = provider.database_provider_rw()?.into_tx();
    tx.put::<StoredL1HeadOriginTable>(STORED_L1_HEAD_ORIGIN_KEY, head)?;
    tx.commit()?;
    Ok(())
}

/// Sends a `GetL1Origins` request and awaits peer response with timeout.
async fn request_l1_origins(
    peer: &mpsc::UnboundedSender<TaikoAuxPeerRequest>,
    request: AuxRangeRequest,
    request_timeout: Duration,
) -> eyre::Result<Vec<AuxL1OriginEntry>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    peer.send(TaikoAuxPeerRequest::GetL1Origins { request, tx })
        .map_err(|_| eyre!("failed to send l1 origins request"))?;
    timeout(request_timeout, rx)
        .await
        .wrap_err("timed out waiting for l1 origins response")?
        .map_err(|_| eyre!("l1 origins response channel dropped"))
}

/// Sends a `GetBatchLastBlocks` request and awaits peer response with timeout.
async fn request_batch_last_blocks(
    peer: &mpsc::UnboundedSender<TaikoAuxPeerRequest>,
    request: AuxRangeRequest,
    request_timeout: Duration,
) -> eyre::Result<Vec<AuxBatchLastBlockEntry>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    peer.send(TaikoAuxPeerRequest::GetBatchLastBlocks { request, tx })
        .map_err(|_| eyre!("failed to send batch last blocks request"))?;
    timeout(request_timeout, rx)
        .await
        .wrap_err("timed out waiting for batch last blocks response")?
        .map_err(|_| eyre!("batch last blocks response channel dropped"))
}

/// Sends a `GetHeadL1Origin` request and awaits peer response with timeout.
async fn request_head_l1_origin(
    peer: &mpsc::UnboundedSender<TaikoAuxPeerRequest>,
    request_timeout: Duration,
) -> eyre::Result<Option<u64>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    peer.send(TaikoAuxPeerRequest::GetHeadL1Origin { tx })
        .map_err(|_| eyre!("failed to send head l1 origin request"))?;
    timeout(request_timeout, rx)
        .await
        .wrap_err("timed out waiting for head l1 origin response")?
        .map_err(|_| eyre!("head l1 origin response channel dropped"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use alloy_primitives::{B256, U256};
    use reth_network::{NetworkSyncUpdater, SyncState, test_utils::Testnet};
    use reth_network_api::test_utils::PeersHandleProvider;
    use reth_provider::test_utils::MockEthProvider;

    use crate::taiko_aux::{
        handlers::{ProtocolState, TaikoAuxProtocolHandler},
        provider::TaikoAuxProtocolProvider,
    };

    use super::*;

    #[derive(Clone, Debug)]
    struct MockRemoteAuxProvider {
        l1_rows: Vec<AuxL1OriginEntry>,
        batch_rows: Vec<AuxBatchLastBlockEntry>,
        head_l1_origin: Option<u64>,
    }

    impl TaikoAuxProtocolProvider for MockRemoteAuxProvider {
        fn l1_origins(
            &self,
            request: AuxRangeRequest,
        ) -> reth_storage_errors::ProviderResult<Vec<AuxL1OriginEntry>> {
            if request.limit == 0 {
                return Ok(Vec::new());
            }

            let mut rows = Vec::new();
            for row in &self.l1_rows {
                if row.block_number < request.start {
                    continue;
                }
                rows.push(row.clone());
                if rows.len() >= request.limit as usize {
                    break;
                }
            }
            Ok(rows)
        }

        fn head_l1_origin(&self) -> reth_storage_errors::ProviderResult<Option<u64>> {
            Ok(self.head_l1_origin)
        }

        fn batch_last_blocks(
            &self,
            request: AuxRangeRequest,
        ) -> reth_storage_errors::ProviderResult<Vec<AuxBatchLastBlockEntry>> {
            if request.limit == 0 {
                return Ok(Vec::new());
            }

            let mut rows = Vec::new();
            for row in &self.batch_rows {
                if row.batch_id < request.start {
                    continue;
                }
                rows.push(*row);
                if rows.len() >= request.limit as usize {
                    break;
                }
            }
            Ok(rows)
        }
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct NoopAuxProvider;

    impl TaikoAuxProtocolProvider for NoopAuxProvider {
        fn l1_origins(
            &self,
            _request: AuxRangeRequest,
        ) -> reth_storage_errors::ProviderResult<Vec<AuxL1OriginEntry>> {
            Ok(Vec::new())
        }

        fn head_l1_origin(&self) -> reth_storage_errors::ProviderResult<Option<u64>> {
            Ok(None)
        }

        fn batch_last_blocks(
            &self,
            _request: AuxRangeRequest,
        ) -> reth_storage_errors::ProviderResult<Vec<AuxBatchLastBlockEntry>> {
            Ok(Vec::new())
        }
    }

    #[derive(Clone, Debug)]
    struct InMemoryAuxStore {
        inner: Arc<Mutex<InMemoryAuxStoreState>>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct InMemoryAuxStoreState {
        best_block_number: u64,
        l1_origins: BTreeMap<u64, AuxL1OriginEntry>,
        batch_last_blocks: BTreeMap<u64, u64>,
        head_l1_origin: Option<u64>,
    }

    impl InMemoryAuxStore {
        fn new(best_block_number: u64) -> Self {
            Self {
                inner: Arc::new(Mutex::new(InMemoryAuxStoreState {
                    best_block_number,
                    l1_origins: BTreeMap::new(),
                    batch_last_blocks: BTreeMap::new(),
                    head_l1_origin: None,
                })),
            }
        }

        fn snapshot(&self) -> InMemoryAuxStoreState {
            self.inner.lock().unwrap().clone()
        }
    }

    impl TaikoAuxLocalStore for InMemoryAuxStore {
        fn best_block_number(&self) -> eyre::Result<u64> {
            Ok(self.inner.lock().unwrap().best_block_number)
        }

        fn next_l1_origin_start(&self) -> eyre::Result<u64> {
            let start = self
                .inner
                .lock()
                .unwrap()
                .l1_origins
                .last_key_value()
                .map(|(key, _)| key.saturating_add(1))
                .unwrap_or(0);
            Ok(start)
        }

        fn next_batch_start(&self) -> eyre::Result<u64> {
            let start = self
                .inner
                .lock()
                .unwrap()
                .batch_last_blocks
                .last_key_value()
                .map(|(key, _)| key.saturating_add(1))
                .unwrap_or(0);
            Ok(start)
        }

        fn persist_l1_origins(&self, rows: Vec<AuxL1OriginEntry>) -> eyre::Result<()> {
            if rows.is_empty() {
                return Ok(());
            }
            let mut inner = self.inner.lock().unwrap();
            for row in rows {
                inner.l1_origins.insert(row.block_number, row);
            }
            Ok(())
        }

        fn persist_batch_rows(&self, rows: Vec<AuxBatchLastBlockEntry>) -> eyre::Result<()> {
            if rows.is_empty() {
                return Ok(());
            }
            let mut inner = self.inner.lock().unwrap();
            for row in rows {
                inner.batch_last_blocks.insert(row.batch_id, row.block_number);
            }
            Ok(())
        }

        fn set_head_l1_origin(&self, head: u64) -> eyre::Result<()> {
            self.inner.lock().unwrap().head_l1_origin = Some(head);
            Ok(())
        }
    }

    fn sample_l1_origin(block_number: u64, marker: u8) -> AuxL1OriginEntry {
        AuxL1OriginEntry {
            block_number,
            block_id: U256::from(block_number),
            l2_block_hash: B256::from([marker; 32]),
            l1_block_height: U256::from(block_number + 100),
            l1_block_hash: B256::from([marker.wrapping_add(1); 32]),
            build_payload_args_id: [marker; 8],
            is_forced_inclusion: marker.is_multiple_of(2),
            signature: [marker; 65],
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backfills_aux_tables_from_peer_while_syncing() {
        reth_tracing::init_test_tracing();

        let mut net = Testnet::create_with(2, MockEthProvider::default()).await;

        let remote_provider = MockRemoteAuxProvider {
            l1_rows: vec![
                sample_l1_origin(1, 0x01),
                sample_l1_origin(2, 0x02),
                sample_l1_origin(4, 0x04),
            ],
            batch_rows: vec![
                AuxBatchLastBlockEntry { batch_id: 1, block_number: 2 },
                AuxBatchLastBlockEntry { batch_id: 2, block_number: 5 },
            ],
            head_l1_origin: Some(2),
        };

        let (peer0_tx, _peer0_rx) = mpsc::unbounded_channel();
        let peer0 = &mut net.peers_mut()[0];
        peer0.add_rlpx_sub_protocol(TaikoAuxProtocolHandler {
            provider: remote_provider,
            peers_handle: peer0.handle().peers_handle().clone(),
            max_active_connections: 100,
            state: ProtocolState::new(peer0_tx),
        });

        let (peer1_tx, peer1_rx) = mpsc::unbounded_channel();
        let peer1 = &mut net.peers_mut()[1];
        peer1.add_rlpx_sub_protocol(TaikoAuxProtocolHandler {
            provider: NoopAuxProvider,
            peers_handle: peer1.handle().peers_handle().clone(),
            max_active_connections: 100,
            state: ProtocolState::new(peer1_tx),
        });

        let handle = net.spawn();
        let network = handle.peers()[1].network().clone();
        network.update_sync_state(SyncState::Syncing);

        let store = InMemoryAuxStore::new(3);
        let config = TaikoAuxSyncConfig {
            max_active_connections: 16,
            request_batch_size: 16,
            request_timeout: Duration::from_secs(2),
            syncing_poll_interval: Duration::from_millis(25),
            idle_poll_interval: Duration::from_secs(60),
        };

        let sync_task = tokio::spawn(run_taiko_aux_sync(store.clone(), network, peer1_rx, config));

        handle.connect_peers().await;

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = store.snapshot();
                let l1_done = snapshot.l1_origins.keys().copied().collect::<Vec<_>>() == vec![1, 2];
                let batch_done = snapshot
                    .batch_last_blocks
                    .iter()
                    .map(|(key, value)| (*key, *value))
                    .collect::<Vec<_>>() ==
                    vec![(1, 2)];
                let head_done = snapshot.head_l1_origin == Some(2);
                if l1_done && batch_done && head_done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timed out waiting for aux table backfill");

        sync_task.abort();
        let _ = sync_task.await;

        let snapshot = store.snapshot();
        assert_eq!(snapshot.l1_origins.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(
            snapshot
                .batch_last_blocks
                .iter()
                .map(|(key, value)| (*key, *value))
                .collect::<Vec<_>>(),
            vec![(1, 2)]
        );
        assert_eq!(snapshot.head_l1_origin, Some(2));
    }
}
