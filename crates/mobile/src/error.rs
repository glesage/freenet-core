//! Error type crossing the FFI boundary.

/// Everything the mobile API can fail with. Variants carry a message; UniFFI
/// exposes them as a flat error enum whose description is that message.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum MobileError {
    /// The profile could not be turned into a node configuration.
    #[error("config: {0}")]
    Config(String),
    /// The node failed to start (stores, sockets, gateway index).
    #[error("startup: {0}")]
    Startup(String),
    /// The node answered a request with an error.
    #[error("request: {0}")]
    Request(String),
    /// No answer arrived in time.
    #[error("timeout: {0}")]
    Timeout(String),
    /// The requested contract is not known to the node (or the network).
    #[error("not found: {0}")]
    NotFound(String),
    /// The call is not valid in the node's current state (e.g. `get` while stopped).
    #[error("invalid state: {0}")]
    InvalidState(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl From<tokio::task::JoinError> for MobileError {
    fn from(e: tokio::task::JoinError) -> Self {
        MobileError::Other(format!("runtime task failed: {e}"))
    }
}
