use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

use alloy_primitives::bytes::BytesMut;
use futures::Stream;
use reth_eth_wire::multiplex::ProtocolConnection;
use reth_network_api::{PeerId, ReputationChangeKind, test_utils::PeersHandle};
use tokio::sync::{mpsc, oneshot};
use tracing::trace;

use crate::taiko_aux::{
    message::{
        AuxBatchLastBlockEntry, AuxL1OriginEntry, AuxRangeRequest, TaikoAuxMessage,
        TaikoAuxNodeType, TaikoAuxProtocolMessage,
    },
    provider::TaikoAuxProtocolProvider,
};

/// Request message sent by the sync task to a peer connection.
#[derive(Debug)]
pub enum TaikoAuxPeerRequest {
    /// Fetch `StoredL1OriginTable` rows.
    GetL1Origins {
        /// Requested key range.
        request: AuxRangeRequest,
        /// One-shot channel receiving fetched rows.
        tx: oneshot::Sender<Vec<AuxL1OriginEntry>>,
    },
    /// Fetch `StoredL1HeadOriginTable` value.
    GetHeadL1Origin {
        /// One-shot channel receiving the head origin pointer.
        tx: oneshot::Sender<Option<u64>>,
    },
    /// Fetch `BatchToLastBlock` rows.
    GetBatchLastBlocks {
        /// Requested key range.
        request: AuxRangeRequest,
        /// One-shot channel receiving fetched rows.
        tx: oneshot::Sender<Vec<AuxBatchLastBlockEntry>>,
    },
}

/// Connection handler for the `taiko_aux` protocol.
#[derive(Debug)]
pub struct TaikoAuxProtocolConnection<P> {
    /// Local provider used to serve inbound requests.
    provider: P,
    /// Peer manager handle for reputation updates.
    peers_handle: PeersHandle,
    /// Identifier of the remote peer bound to this stream.
    peer_id: PeerId,
    /// Underlying multiplexed RLPx subprotocol connection.
    conn: ProtocolConnection,
    /// Commands issued by the background sync worker.
    commands: mpsc::UnboundedReceiver<TaikoAuxPeerRequest>,
    /// Shared count of currently active `taiko_aux` sessions.
    active_connections: Arc<AtomicU64>,
    /// Indicates whether node type handshake message was already sent.
    node_type_sent: bool,
    /// Indicates that the stream has terminated.
    terminated: bool,
    /// Monotonic local request ID counter.
    next_id: u64,
    /// Outstanding requests keyed by request ID.
    inflight_requests: HashMap<u64, TaikoAuxPeerRequest>,
}

impl<P> TaikoAuxProtocolConnection<P> {
    /// Creates a new per-peer `taiko_aux` connection state machine.
    pub fn new(
        provider: P,
        peers_handle: PeersHandle,
        peer_id: PeerId,
        conn: ProtocolConnection,
        commands: mpsc::UnboundedReceiver<TaikoAuxPeerRequest>,
        active_connections: Arc<AtomicU64>,
    ) -> Self {
        Self {
            provider,
            peers_handle,
            peer_id,
            conn,
            commands,
            active_connections,
            node_type_sent: false,
            terminated: false,
            next_id: 0,
            inflight_requests: HashMap::default(),
        }
    }

    /// Returns the next outbound request ID.
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Reports malformed or unexpected peer messages to reputation system.
    fn report_bad_message(&self) {
        self.peers_handle.reputation_change(self.peer_id, ReputationChangeKind::BadMessage);
    }

    /// Converts a worker command into a wire message and tracks it as in-flight.
    fn on_command(&mut self, command: TaikoAuxPeerRequest) -> TaikoAuxProtocolMessage {
        let request_id = self.next_id();
        let message = match &command {
            TaikoAuxPeerRequest::GetL1Origins { request, .. } => {
                TaikoAuxProtocolMessage::get_l1_origins(request_id, *request)
            }
            TaikoAuxPeerRequest::GetHeadL1Origin { .. } => {
                TaikoAuxProtocolMessage::get_head_l1_origin(request_id)
            }
            TaikoAuxPeerRequest::GetBatchLastBlocks { request, .. } => {
                TaikoAuxProtocolMessage::get_batch_last_blocks(request_id, *request)
            }
        };
        self.inflight_requests.insert(request_id, command);
        message
    }
}

impl<P> TaikoAuxProtocolConnection<P>
where
    P: TaikoAuxProtocolProvider + 'static,
{
    /// Processes an inbound protocol message and optionally returns an immediate response.
    fn on_taiko_aux_message(&mut self, msg: TaikoAuxProtocolMessage) -> OnTaikoAuxMessageOutcome {
        match msg.message {
            TaikoAuxMessage::NodeType(_) => {}
            TaikoAuxMessage::GetL1Origins(req) => {
                trace!(
                    target: "taiko_aux::net::connection",
                    peer_id=%self.peer_id,
                    request=?req.message,
                    "serving l1 origins"
                );
                let rows = match self.provider.l1_origins(req.message) {
                    Ok(rows) => rows,
                    Err(error) => {
                        trace!(
                            target: "taiko_aux::net::connection",
                            peer_id=%self.peer_id,
                            request=?req.message,
                            %error,
                            "error retrieving l1 origins"
                        );
                        Vec::new()
                    }
                };
                let response = TaikoAuxProtocolMessage::l1_origins(req.request_id, rows);
                return OnTaikoAuxMessageOutcome::Response(response.encoded());
            }
            TaikoAuxMessage::GetHeadL1Origin(req) => {
                trace!(
                    target: "taiko_aux::net::connection",
                    peer_id=%self.peer_id,
                    "serving head l1 origin"
                );
                let head = match self.provider.head_l1_origin() {
                    Ok(head) => head,
                    Err(error) => {
                        trace!(
                            target: "taiko_aux::net::connection",
                            peer_id=%self.peer_id,
                            %error,
                            "error retrieving head l1 origin"
                        );
                        None
                    }
                };
                let response = TaikoAuxProtocolMessage::head_l1_origin(req.request_id, head);
                return OnTaikoAuxMessageOutcome::Response(response.encoded());
            }
            TaikoAuxMessage::GetBatchLastBlocks(req) => {
                trace!(
                    target: "taiko_aux::net::connection",
                    peer_id=%self.peer_id,
                    request=?req.message,
                    "serving batch last blocks"
                );
                let rows = match self.provider.batch_last_blocks(req.message) {
                    Ok(rows) => rows,
                    Err(error) => {
                        trace!(
                            target: "taiko_aux::net::connection",
                            peer_id=%self.peer_id,
                            request=?req.message,
                            %error,
                            "error retrieving batch rows"
                        );
                        Vec::new()
                    }
                };
                let response = TaikoAuxProtocolMessage::batch_last_blocks(req.request_id, rows);
                return OnTaikoAuxMessageOutcome::Response(response.encoded());
            }
            TaikoAuxMessage::L1Origins(res) => {
                if let Some(TaikoAuxPeerRequest::GetL1Origins { tx, .. }) =
                    self.inflight_requests.remove(&res.request_id)
                {
                    let _ = tx.send(res.message);
                } else {
                    self.report_bad_message();
                }
            }
            TaikoAuxMessage::HeadL1Origin(res) => {
                if let Some(TaikoAuxPeerRequest::GetHeadL1Origin { tx }) =
                    self.inflight_requests.remove(&res.request_id)
                {
                    let _ = tx.send(res.message.into_option());
                } else {
                    self.report_bad_message();
                }
            }
            TaikoAuxMessage::BatchLastBlocks(res) => {
                if let Some(TaikoAuxPeerRequest::GetBatchLastBlocks { tx, .. }) =
                    self.inflight_requests.remove(&res.request_id)
                {
                    let _ = tx.send(res.message);
                } else {
                    self.report_bad_message();
                }
            }
        }
        OnTaikoAuxMessageOutcome::None
    }
}

impl<P> Drop for TaikoAuxProtocolConnection<P> {
    /// Decrements active session counter when the connection is dropped.
    fn drop(&mut self) {
        let _ = self
            .active_connections
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| Some(c.saturating_sub(1)));
    }
}

impl<P> Stream for TaikoAuxProtocolConnection<P>
where
    P: TaikoAuxProtocolProvider + Unpin + 'static,
{
    type Item = BytesMut;

    /// Polls worker commands and peer messages to drive the bidirectional protocol stream.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if this.terminated {
            return Poll::Ready(None);
        }

        if !this.node_type_sent {
            this.node_type_sent = true;
            return Poll::Ready(Some(
                TaikoAuxProtocolMessage::node_type(TaikoAuxNodeType::Full).encoded(),
            ));
        }

        'connection: loop {
            if let Poll::Ready(Some(command)) = this.commands.poll_recv(cx) {
                let message = this.on_command(command);
                let encoded = message.encoded();
                trace!(
                    target: "taiko_aux::net::connection",
                    peer_id=%this.peer_id,
                    message=?message.message_type,
                    encoded=alloy_primitives::hex::encode(&encoded),
                    "sending peer command"
                );
                return Poll::Ready(Some(encoded));
            }

            match Pin::new(&mut this.conn).poll_next(cx) {
                Poll::Ready(Some(next)) => {
                    let message = match TaikoAuxProtocolMessage::decode_message(&mut &next[..]) {
                        Ok(message) => message,
                        Err(error) => {
                            trace!(
                                target: "taiko_aux::net::connection",
                                peer_id=%this.peer_id,
                                %error,
                                "error decoding peer message"
                            );
                            this.report_bad_message();
                            continue;
                        }
                    };

                    trace!(
                        target: "taiko_aux::net::connection",
                        peer_id=%this.peer_id,
                        message=?message.message_type,
                        "processing message"
                    );

                    match this.on_taiko_aux_message(message) {
                        OnTaikoAuxMessageOutcome::Response(bytes) => {
                            return Poll::Ready(Some(bytes));
                        }
                        OnTaikoAuxMessageOutcome::None => continue,
                    }
                }
                Poll::Ready(None) => break 'connection,
                Poll::Pending => return Poll::Pending,
            }
        }

        this.terminated = true;
        Poll::Ready(None)
    }
}

/// Result of handling a decoded inbound `taiko_aux` message.
enum OnTaikoAuxMessageOutcome {
    /// A response must be sent immediately on the wire.
    Response(BytesMut),
    /// No outbound message is produced.
    None,
}
