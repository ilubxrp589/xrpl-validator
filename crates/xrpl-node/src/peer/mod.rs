//! Peer-to-peer networking layer.
//!
//! Handles TLS connections, XRPL/2.0 handshake, message framing,
//! peer discovery, and connection management.

/// Generated protobuf types for the XRPL peer protocol.
///
/// Key message types:
/// - [`protocol::TmManifest`] — Validator ephemeral key announcement
/// - [`protocol::TmPing`] — Keepalive ping/pong
/// - [`protocol::TmTransaction`] — Transaction relay
/// - [`protocol::TmProposeSet`] — Consensus proposal
/// - [`protocol::TmValidation`] — Ledger validation vote
/// - [`protocol::TmStatusChange`] — Peer status update (ledger sequence, node state)
/// - [`protocol::TmGetLedger`] — Request ledger/SHAMap data
/// - [`protocol::TmLedgerData`] — Ledger/SHAMap data response
/// - [`protocol::TmHaveTransactionSet`] — Announce possession of a transaction set
/// - [`protocol::TmEndpoints`] — Peer discovery endpoints
/// - [`protocol::TmCluster`] — Cluster node status
/// - [`protocol::TmSquelch`] — Validator message squelching
pub mod codec;
pub mod connection;
pub mod handshake;
pub mod identity;
pub mod message;
#[allow(clippy::all)]
pub mod protocol;
