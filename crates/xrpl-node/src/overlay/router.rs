//! Message router — dispatches incoming peer messages to subsystems.

use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::peer::connection::PeerId;
use crate::peer::message::PeerMessage;

/// Incoming message with source peer ID.
pub type InboundMessage = (PeerId, PeerMessage);

/// Routes incoming peer messages to the correct handler subsystem.
pub struct OverlayRouter {
    /// Incoming messages from all peer connections.
    inbound_rx: mpsc::Receiver<InboundMessage>,
    /// Transaction messages → mempool.
    pub tx_tx: mpsc::Sender<InboundMessage>,
    /// Consensus messages (proposals, validations) → consensus engine.
    pub consensus_tx: mpsc::Sender<InboundMessage>,
    /// Ledger sync messages → SHAMap sync.
    pub sync_tx: mpsc::Sender<InboundMessage>,
    /// Status changes → ledger follower.
    pub status_tx: mpsc::Sender<InboundMessage>,
}

/// Channels returned when creating the router, for subsystems to receive from.
pub struct RouterChannels {
    pub tx_rx: mpsc::Receiver<InboundMessage>,
    pub consensus_rx: mpsc::Receiver<InboundMessage>,
    pub sync_rx: mpsc::Receiver<InboundMessage>,
    pub status_rx: mpsc::Receiver<InboundMessage>,
}

impl OverlayRouter {
    /// Create a new router with its channel endpoints.
    ///
    /// Returns `(router, inbound_tx, channels)`:
    /// - `router` — run this to dispatch messages
    /// - `inbound_tx` — give this to PeerManager for peer connections
    /// - `channels` — receivers for each subsystem
    pub fn new(buffer: usize) -> (Self, mpsc::Sender<InboundMessage>, RouterChannels) {
        let (inbound_tx, inbound_rx) = mpsc::channel(buffer);
        let (tx_tx, tx_rx) = mpsc::channel(buffer);
        let (consensus_tx, consensus_rx) = mpsc::channel(buffer);
        let (sync_tx, sync_rx) = mpsc::channel(buffer);
        let (status_tx, status_rx) = mpsc::channel(buffer);

        let router = Self {
            inbound_rx,
            tx_tx,
            consensus_tx,
            sync_tx,
            status_tx,
        };

        let channels = RouterChannels {
            tx_rx,
            consensus_rx,
            sync_rx,
            status_rx,
        };

        (router, inbound_tx, channels)
    }

    /// Run the router loop, dispatching messages until the inbound channel closes.
    pub async fn run(mut self) {
        debug!("overlay router started");

        while let Some((peer_id, msg)) = self.inbound_rx.recv().await {
            let peer_hex = hex::encode(&peer_id.0[..8]);
            trace!(peer = %peer_hex, msg = msg.name(), "routing");

            let result = match &msg {
                // Transactions → mempool
                PeerMessage::Transaction(_) | PeerMessage::Transactions(_) => {
                    self.tx_tx.send((peer_id, msg)).await
                }

                // Consensus messages → consensus engine
                // SECURITY(7.3): Validation messages are forwarded without cryptographic
                // signature verification. The TmValidation protobuf contains a serialized
                // validation blob with a signature field, but we do not verify it here.
                // Full Ed25519/Secp256k1 verification should be added before trusting
                // validation data for consensus decisions.
                PeerMessage::Validation(_) => {
                    warn!(peer = %peer_hex, "forwarding UNVERIFIED validation — signature not checked");
                    self.consensus_tx.send((peer_id, msg)).await
                }
                PeerMessage::ProposeSet(_)
                | PeerMessage::HaveTransactionSet(_) => {
                    self.consensus_tx.send((peer_id, msg)).await
                }

                // Ledger data → SHAMap sync
                PeerMessage::GetLedger(_)
                | PeerMessage::LedgerData(_)
                | PeerMessage::GetObjects(_)
                | PeerMessage::ProofPathRequest(_)
                | PeerMessage::ProofPathResponse(_)
                | PeerMessage::ReplayDeltaRequest(_)
                | PeerMessage::ReplayDeltaResponse(_)
                | PeerMessage::HaveTransactions(_) => {
                    self.sync_tx.send((peer_id, msg)).await
                }

                // Status & peer management → ledger follower
                PeerMessage::StatusChange(_)
                | PeerMessage::Endpoints(_)
                | PeerMessage::Manifests(_)
                | PeerMessage::ValidatorList(_)
                | PeerMessage::ValidatorListCollection(_) => {
                    self.status_tx.send((peer_id, msg)).await
                }

                // Cluster, squelch, ping — handled locally or ignored
                PeerMessage::Cluster(_) | PeerMessage::Squelch(_) => {
                    trace!(peer = %peer_hex, msg = msg.name(), "ignored");
                    Ok(())
                }

                // Ping should never reach here (handled in connection)
                PeerMessage::Ping(_) => {
                    trace!(peer = %peer_hex, "ping reached router (unexpected)");
                    Ok(())
                }
            };

            if let Err(e) = result {
                debug!(peer = %peer_hex, "router send failed: {e}");
            }
        }

        debug!("overlay router stopped");
    }
}
