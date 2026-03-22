/// Errors from ledger storage and SHAMap operations.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("node not found: {0}")]
    NodeNotFound(String),

    #[error("storage error: {0}")]
    StorageError(String),

    #[error("deserialization error: {0}")]
    DeserializationError(String),

    #[error("invalid tree type: {0}")]
    InvalidTreeType(String),

    #[error("invalid ledger header: {0}")]
    InvalidHeader(String),

    #[error("invariant violation: {0}")]
    Invariant(String),

    #[error("codec error: {0}")]
    Codec(#[from] xrpl_core::CoreError),
}
