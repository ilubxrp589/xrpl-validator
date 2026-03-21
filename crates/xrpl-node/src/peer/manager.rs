//! PeerManager — connection pool, peer discovery, and lifecycle.

use std::net::SocketAddr;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use super::connection::{PeerHandle, PeerId, PeerInfo};
use super::handshake;
use super::identity::NodeIdentity;
use super::message::PeerMessage;
use crate::config::NodeConfig;

/// Manages the pool of peer connections.
pub struct PeerManager {
    /// Active peer connections.
    peers: Arc<DashMap<PeerId, PeerHandle>>,
    /// Our node identity.
    identity: Arc<NodeIdentity>,
    /// Node configuration.
    config: Arc<NodeConfig>,
    /// Channel for inbound messages from all peers → overlay router.
    inbound_tx: mpsc::Sender<(PeerId, PeerMessage)>,
    /// Shutdown signal.
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl PeerManager {
    /// Create a new peer manager.
    pub fn new(
        identity: Arc<NodeIdentity>,
        config: Arc<NodeConfig>,
        inbound_tx: mpsc::Sender<(PeerId, PeerMessage)>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            peers: Arc::new(DashMap::new()),
            identity,
            config,
            inbound_tx,
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Connect to all configured seed peers.
    pub async fn connect_to_seeds(&self) {
        for addr in &self.config.seed_peers {
            info!(addr = %addr, "connecting to seed peer");
            if let Err(e) = self.connect_to(addr).await {
                warn!(addr = %addr, "seed peer connection failed: {e}");
            }
        }
    }

    /// Connect to a specific peer address.
    pub async fn connect_to(&self, addr: &str) -> Result<PeerHandle, crate::NodeError> {
        let network_id = if self.config.seed_peers.iter().any(|s| s.contains("altnet") || s.contains("testnet")) {
            handshake::NETWORK_ID_TESTNET
        } else {
            handshake::NETWORK_ID_MAINNET
        };

        let result = handshake::outbound_handshake(addr, &self.identity, network_id).await?;

        let socket_addr: SocketAddr = addr
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));

        let handle = super::connection::spawn_connection(
            result.stream,
            result.peer_public_key,
            socket_addr,
            result.remaining_bytes,
            self.inbound_tx.clone(),
            self.shutdown_rx.clone(),
        );

        let peer_id = *handle.peer_id();
        self.peers.insert(peer_id, handle.clone());

        info!(
            peer = %hex::encode(&peer_id.0[..8]),
            addr = %addr,
            total = self.peers.len(),
            "peer connected"
        );

        Ok(handle)
    }

    /// Send a message to all connected peers.
    pub async fn broadcast(&self, msg: PeerMessage) {
        for entry in self.peers.iter() {
            let peer_hex = hex::encode(&entry.key().0[..8]);
            if let Err(e) = entry.value().send(msg.clone()).await {
                debug!(peer = %peer_hex, "broadcast send failed: {e}");
            }
        }
    }

    /// Send a message to all peers except the specified one.
    pub async fn broadcast_except(&self, msg: PeerMessage, exclude: &PeerId) {
        for entry in self.peers.iter() {
            if entry.key() == exclude {
                continue;
            }
            let peer_hex = hex::encode(&entry.key().0[..8]);
            if let Err(e) = entry.value().send(msg.clone()).await {
                debug!(peer = %peer_hex, "broadcast send failed: {e}");
            }
        }
    }

    /// Send a message to a specific peer.
    pub async fn send_to(&self, peer_id: &PeerId, msg: PeerMessage) -> Result<(), crate::NodeError> {
        if let Some(handle) = self.peers.get(peer_id) {
            handle.send(msg).await
        } else {
            Err(crate::NodeError::Connection(format!(
                "peer not found: {}",
                hex::encode(&peer_id.0[..8])
            )))
        }
    }

    /// Number of connected peers.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Get info about all connected peers.
    pub fn connected_peers(&self) -> Vec<PeerInfo> {
        self.peers.iter().map(|e| e.value().info().clone()).collect()
    }

    /// Remove a disconnected peer.
    pub fn remove_peer(&self, peer_id: &PeerId) {
        if self.peers.remove(peer_id).is_some() {
            debug!(
                peer = %hex::encode(&peer_id.0[..8]),
                total = self.peers.len(),
                "peer removed"
            );
        }
    }

    /// Signal all peer connections to shut down.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        info!(peers = self.peers.len(), "shutting down all peer connections");
    }
}
