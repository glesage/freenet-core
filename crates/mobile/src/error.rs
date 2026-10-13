//! Typed errors returned to Swift and Kotlin.

use freenet_stdlib::client_api::{
    ClientError, ContractError as RequestContractError, ErrorKind, RequestError,
};
use serde::{Deserialize, Serialize};

/// What kind of failure a client request met. Each platform maps the same
/// node reply to the same kind, which the protocol fixtures check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ClientErrorKind {
    /// The network holds no copy of the contract.
    NotFound,
    /// The node has no code for the contract, so it cannot run it.
    MissingContract,
    /// The contract rejected the request, for example an invalid update.
    ContractRejected,
    /// A delegate call failed.
    DelegateError,
    /// No reply arrived within the request's time limit. The outcome is
    /// uncertain: the node may still apply the request.
    Timeout,
    /// The connection to the node closed.
    Disconnected,
    /// The node is not reachable or is shutting down.
    NodeUnavailable,
    /// The node has no peers yet, so it cannot reach the network.
    NoPeers,
    /// A reply could not be decoded.
    Deserialization,
    /// The node reported that the operation failed.
    OperationFailed,
    /// Anything else, with the node's message.
    Other,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MobileError {
    #[error("invalid settings: {reason}")]
    InvalidSettings { reason: String },
    #[error("storage error: {reason}")]
    Storage { reason: String },
    #[error("the node is not running")]
    NotRunning,
    #[error("the node failed to start: {reason}")]
    StartFailed { reason: String },
    #[error("no gateway is known: {reason}")]
    NoGateways { reason: String },
    #[error("timed out waiting for {what}")]
    Timeout { what: String },
    /// `detail` rather than `message`: Kotlin exceptions already have one.
    #[error("{kind:?}: {detail}")]
    Client {
        kind: ClientErrorKind,
        detail: String,
    },
    #[error("bundle rejected: {reason}")]
    Bundle { reason: String },
    #[error("the {backend} Wasm backend is not available on this target")]
    BackendUnavailable { backend: String },
    #[error("internal error: {reason}")]
    Internal { reason: String },
}

impl MobileError {
    pub(crate) fn storage(err: impl std::fmt::Display) -> Self {
        Self::Storage {
            reason: err.to_string(),
        }
    }

    pub(crate) fn internal(err: impl std::fmt::Display) -> Self {
        Self::Internal {
            reason: err.to_string(),
        }
    }

    pub(crate) fn client(kind: ClientErrorKind, detail: impl Into<String>) -> Self {
        Self::Client {
            kind,
            detail: detail.into(),
        }
    }

    /// The client error kind, when this is a client error.
    pub fn client_kind(&self) -> Option<ClientErrorKind> {
        match self {
            Self::Client { kind, .. } => Some(*kind),
            Self::Timeout { .. } => Some(ClientErrorKind::Timeout),
            _ => None,
        }
    }
}

/// The kind's name as the fixtures record it, such as `NotFound`.
#[uniffi::export]
pub fn client_error_kind_name(kind: ClientErrorKind) -> String {
    format!("{kind:?}")
}

/// Map a node reply error to its kind.
pub(crate) fn classify_client_error(err: &ClientError) -> ClientErrorKind {
    match err.kind() {
        ErrorKind::RequestError(RequestError::ContractError(contract)) => match contract {
            RequestContractError::MissingContract { .. } => ClientErrorKind::MissingContract,
            _ => ClientErrorKind::ContractRejected,
        },
        ErrorKind::RequestError(RequestError::DelegateError(_)) => ClientErrorKind::DelegateError,
        ErrorKind::RequestError(RequestError::Timeout) => ClientErrorKind::Timeout,
        ErrorKind::RequestError(RequestError::Disconnect)
        | ErrorKind::Disconnect
        | ErrorKind::ChannelClosed
        | ErrorKind::TransportProtocolDisconnect => ClientErrorKind::Disconnected,
        ErrorKind::NodeUnavailable | ErrorKind::Shutdown => ClientErrorKind::NodeUnavailable,
        ErrorKind::EmptyRing | ErrorKind::PeerNotJoined => ClientErrorKind::NoPeers,
        ErrorKind::DeserializationError { .. } => ClientErrorKind::Deserialization,
        ErrorKind::OperationError { .. } | ErrorKind::FailedOperation => {
            ClientErrorKind::OperationFailed
        }
        _ => ClientErrorKind::Other,
    }
}
