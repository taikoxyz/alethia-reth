use std::{
    fmt,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use reth::network::protocol::{ConnectionHandler, OnNotSupported, ProtocolHandler};
use reth_eth_wire::{
    capability::SharedCapabilities, multiplex::ProtocolConnection, protocol::Protocol,
};
use reth_network_api::{Direction, PeerId, test_utils::PeersHandle};
use tokio::sync::mpsc;
use tracing::trace;

use crate::taiko_aux::{
    connection::{TaikoAuxPeerRequest, TaikoAuxProtocolConnection},
    message::TaikoAuxProtocolMessage,
    provider::TaikoAuxProtocolProvider,
};

/// Events emitted by the `taiko_aux` protocol.
#[derive(Debug)]
pub enum ProtocolEvent {
    /// Connection established with a peer.
    Established {
        /// Inbound or outbound connection direction.
        direction: Direction,
        /// Peer identifier associated with this connection.
        peer_id: PeerId,
        /// Sender used by sync worker to issue requests over this session.
        to_connection: mpsc::UnboundedSender<TaikoAuxPeerRequest>,
    },
    /// New connection rejected due to configured limit.
    MaxActiveConnectionsExceeded {
        /// Number of active sessions at the moment of rejection.
        num_active: u64,
    },
}

/// Shared protocol state.
#[derive(Clone, Debug)]
pub struct ProtocolState {
    /// Sender used to forward protocol lifecycle events to sync worker.
    pub events_sender: mpsc::UnboundedSender<ProtocolEvent>,
    /// Global count of active `taiko_aux` sessions.
    pub active_connections: Arc<AtomicU64>,
}

impl ProtocolState {
    /// Creates shared protocol state for one installed subprotocol instance.
    pub fn new(events_sender: mpsc::UnboundedSender<ProtocolEvent>) -> Self {
        Self { events_sender, active_connections: Arc::default() }
    }

    /// Returns the current number of active `taiko_aux` sessions.
    pub fn active_connections(&self) -> u64 {
        self.active_connections.load(Ordering::Relaxed)
    }
}

/// Handler for incoming and outgoing `taiko_aux` protocol sessions.
#[derive(Clone)]
pub struct TaikoAuxProtocolHandler<P> {
    /// Data source serving inbound requests from remote peers.
    pub provider: P,
    /// Handle used for peer reputation updates.
    pub peers_handle: PeersHandle,
    /// Configured upper bound for concurrent sessions.
    pub max_active_connections: u64,
    /// Shared mutable state across session handlers.
    pub state: ProtocolState,
}

impl<P> fmt::Debug for TaikoAuxProtocolHandler<P> {
    /// Formats debug output while omitting provider internals.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaikoAuxProtocolHandler")
            .field("peers_handle", &self.peers_handle)
            .field("max_active_connections", &self.max_active_connections)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<P> ProtocolHandler for TaikoAuxProtocolHandler<P>
where
    P: TaikoAuxProtocolProvider + Clone + Unpin + 'static,
{
    type ConnectionHandler = Self;

    /// Accepts inbound connections when session count is below the configured limit.
    fn on_incoming(&self, socket_addr: SocketAddr) -> Option<Self::ConnectionHandler> {
        let num_active = self.state.active_connections();
        if num_active >= self.max_active_connections {
            trace!(
                target: "taiko_aux::net",
                num_active,
                max_connections = self.max_active_connections,
                %socket_addr,
                "ignoring incoming connection, max active reached"
            );
            let _ = self
                .state
                .events_sender
                .send(ProtocolEvent::MaxActiveConnectionsExceeded { num_active });
            None
        } else {
            Some(self.clone())
        }
    }

    /// Accepts outbound connections when session count is below the configured limit.
    fn on_outgoing(
        &self,
        socket_addr: SocketAddr,
        peer_id: PeerId,
    ) -> Option<Self::ConnectionHandler> {
        let num_active = self.state.active_connections();
        if num_active >= self.max_active_connections {
            trace!(
                target: "taiko_aux::net",
                num_active,
                max_connections = self.max_active_connections,
                %socket_addr,
                %peer_id,
                "ignoring outgoing connection, max active reached"
            );
            let _ = self
                .state
                .events_sender
                .send(ProtocolEvent::MaxActiveConnectionsExceeded { num_active });
            None
        } else {
            Some(self.clone())
        }
    }
}

impl<P> ConnectionHandler for TaikoAuxProtocolHandler<P>
where
    P: TaikoAuxProtocolProvider + Clone + Unpin + 'static,
{
    type Connection = TaikoAuxProtocolConnection<P>;

    /// Returns protocol metadata for `taiko_aux`.
    fn protocol(&self) -> Protocol {
        TaikoAuxProtocolMessage::protocol()
    }

    /// Keeps base peer connection alive when remote does not support this subprotocol.
    fn on_unsupported_by_peer(
        self,
        _supported: &SharedCapabilities,
        _direction: Direction,
        _peer_id: PeerId,
    ) -> OnNotSupported {
        OnNotSupported::KeepAlive
    }

    /// Creates a per-peer connection state machine and emits an establishment event.
    fn into_connection(
        self,
        direction: Direction,
        peer_id: PeerId,
        conn: ProtocolConnection,
    ) -> Self::Connection {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = self.state.events_sender.send(ProtocolEvent::Established {
            direction,
            peer_id,
            to_connection: tx,
        });

        self.state.active_connections.fetch_add(1, Ordering::Relaxed);

        TaikoAuxProtocolConnection::new(
            self.provider.clone(),
            self.peers_handle,
            peer_id,
            conn,
            rx,
            self.state.active_connections,
        )
    }
}
