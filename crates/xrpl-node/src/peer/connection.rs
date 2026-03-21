//! Individual peer connection — async read/write loops with a handle.

use std::net::SocketAddr;
use std::time::Instant;

use futures::SinkExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use tracing::{debug, trace, warn};
use xrpl_core::types::Hash256;

use super::codec::MessageCodec;
use super::message::PeerMessage;
use super::protocol;

/// Unique identifier for a peer (SHA-256 of their public key).
pub type PeerId = Hash256;

/// Information about a connected peer.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub peer_id: PeerId,
    pub public_key: Vec<u8>,
    pub address: SocketAddr,
    pub connected_at: Instant,
}

/// Handle for sending messages to a peer and querying its info.
///
/// Cloneable and cheap — all clones share the same underlying channel.
#[derive(Clone)]
pub struct PeerHandle {
    info: PeerInfo,
    outbound_tx: mpsc::Sender<PeerMessage>,
}

impl PeerHandle {
    /// Send a message to this peer.
    pub async fn send(&self, msg: PeerMessage) -> Result<(), crate::NodeError> {
        self.outbound_tx
            .send(msg)
            .await
            .map_err(|_| crate::NodeError::Connection("peer channel closed".to_string()))
    }

    /// The peer's unique ID.
    pub fn peer_id(&self) -> &PeerId {
        &self.info.peer_id
    }

    /// The peer's address.
    pub fn address(&self) -> SocketAddr {
        self.info.address
    }

    /// The peer's info.
    pub fn info(&self) -> &PeerInfo {
        &self.info
    }
}

/// Spawn async read/write loops for a peer connection.
///
/// Returns a `PeerHandle` for sending messages and a join handle for the
/// connection's lifetime. The connection runs until the stream closes,
/// an error occurs, or the shutdown signal fires.
pub fn spawn_connection(
    stream: tokio_openssl::SslStream<TcpStream>,
    peer_public_key: Vec<u8>,
    address: SocketAddr,
    inbound_tx: mpsc::Sender<(PeerId, PeerMessage)>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> PeerHandle {
    // Compute peer ID from their public key
    let peer_id = {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(&peer_public_key);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&hash);
        Hash256(bytes)
    };

    let info = PeerInfo {
        peer_id,
        public_key: peer_public_key,
        address,
        connected_at: Instant::now(),
    };

    let (outbound_tx, outbound_rx) = mpsc::channel::<PeerMessage>(256);

    let handle = PeerHandle {
        info: info.clone(),
        outbound_tx,
    };

    // Spawn the connection task
    let peer_id_clone = peer_id;
    tokio::spawn(async move {
        run_connection(
            stream,
            info,
            inbound_tx,
            outbound_rx,
            shutdown_rx,
        )
        .await;
        debug!(peer = %hex::encode(peer_id_clone.0), "peer disconnected");
    });

    handle
}

async fn run_connection(
    stream: tokio_openssl::SslStream<TcpStream>,
    info: PeerInfo,
    inbound_tx: mpsc::Sender<(PeerId, PeerMessage)>,
    mut outbound_rx: mpsc::Receiver<PeerMessage>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    use futures::StreamExt;

    let peer_id = info.peer_id;
    let peer_hex = hex::encode(&peer_id.0[..8]);
    let mut framed = Framed::new(stream, MessageCodec);

    loop {
        tokio::select! {
            // Inbound: read from peer
            frame = framed.next() => {
                match frame {
                    Some(Ok(msg)) => {
                        // Handle ping internally
                        if let PeerMessage::Ping(ref ping) = msg {
                            if ping.r#type == protocol::tm_ping::PingType::PtPing as i32 {
                                let pong = PeerMessage::Ping(protocol::TmPing {
                                    r#type: protocol::tm_ping::PingType::PtPong as i32,
                                    seq: ping.seq,
                                    ping_time: ping.ping_time,
                                    net_time: None,
                                });
                                if let Err(e) = framed.send(pong).await {
                                    warn!(peer = %peer_hex, "failed to send pong: {e}");
                                    break;
                                }
                                trace!(peer = %peer_hex, seq = ?ping.seq, "pong sent");
                                continue;
                            }
                        }

                        trace!(peer = %peer_hex, msg = msg.name(), "received");

                        // Forward to overlay router
                        if inbound_tx.send((peer_id, msg)).await.is_err() {
                            debug!(peer = %peer_hex, "overlay channel closed, disconnecting");
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        warn!(peer = %peer_hex, "read error: {e}");
                        break;
                    }
                    None => {
                        debug!(peer = %peer_hex, "stream ended");
                        break;
                    }
                }
            }

            // Outbound: send to peer
            msg = outbound_rx.recv() => {
                match msg {
                    Some(msg) => {
                        trace!(peer = %peer_hex, msg = msg.name(), "sending");
                        if let Err(e) = framed.send(msg).await {
                            warn!(peer = %peer_hex, "write error: {e}");
                            break;
                        }
                    }
                    None => {
                        debug!(peer = %peer_hex, "outbound channel closed");
                        break;
                    }
                }
            }

            // Shutdown signal
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    debug!(peer = %peer_hex, "shutdown signal received");
                    break;
                }
            }
        }
    }
}
