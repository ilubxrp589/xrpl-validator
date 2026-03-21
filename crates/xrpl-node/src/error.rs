/// Errors from the XRPL validator node.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("peer protocol error: {0}")]
    PeerProtocol(String),

    #[error("handshake failed: {0}")]
    HandshakeFailed(String),

    #[error("connection error: {0}")]
    Connection(String),

    #[error("message decode error: {0}")]
    MessageDecode(String),

    #[error("transaction validation failed: {0}")]
    TransactionInvalid(String),

    #[error("consensus error: {0}")]
    Consensus(String),

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ledger error: {0}")]
    Ledger(#[from] xrpl_ledger::LedgerError),

    #[error("codec error: {0}")]
    Codec(#[from] xrpl_core::CoreError),
}
