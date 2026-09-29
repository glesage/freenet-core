//! The UniFFI surface: what Swift and Kotlin see.
//!
//! All async methods only await channels and join handles, so they run
//! correctly on whichever executor the binding layer polls them from; the
//! node's own work happens on the crate runtime (see `node::runtime`).

use std::path::Path;
use std::sync::Arc;

use freenet_stdlib::prelude::*;

use crate::error::MobileError;
use crate::node::FreenetNode;
use crate::profile::MobileProfile;

/// Lifecycle state of a [`FreenetNode`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NodeStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed { message: String },
}

/// A contract's current state as returned by [`FreenetNode::get`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GetResult {
    /// Base58 contract instance id.
    pub key: String,
    /// Raw state bytes; the application knows the encoding.
    pub state: Vec<u8>,
}

/// Host callbacks. Implemented by the app (Swift/Kotlin) or by Rust tests.
///
/// `on_update` is called from the crate runtime (the client actor task), never
/// from the caller's thread. `on_status` is called inline by `start`/`stop`,
/// on whichever executor polls them, before they return. Neither is guaranteed
/// to arrive on the app's main thread.
#[uniffi::export(with_foreign)]
pub trait ContractUpdateListener: Send + Sync {
    /// A contract this node subscribed to changed. `state` and/or `delta` are
    /// set depending on what the network delivered.
    fn on_update(&self, key: String, state: Option<Vec<u8>>, delta: Option<Vec<u8>>);
    /// The node moved to a new lifecycle state.
    fn on_status(&self, status: NodeStatus);
}

/// Install `freenet`'s tracing subscriber once for this process. `to_stderr`
/// sends log lines to stderr (the Xcode console) instead of files in `log_dir`.
/// Tests install their own subscriber and must not call this.
#[uniffi::export]
pub fn init_logging(log_dir: String, to_stderr: bool) {
    if to_stderr {
        // Read by freenet's subscriber setup; there is no API knob for it.
        // SAFETY: called once at app start before any node thread exists.
        unsafe { std::env::set_var("FREENET_LOG_TO_STDERR", "1") };
    }
    freenet::config::set_logger(None, None, Some(Path::new(&log_dir)));
}

fn parse_key(key: &str) -> Result<ContractInstanceId, MobileError> {
    key.parse::<ContractInstanceId>()
        .map_err(|e| MobileError::Request(format!("invalid contract id {key:?}: {e}")))
}

#[uniffi::export(async_runtime = "tokio")]
impl FreenetNode {
    /// Create a node for `profile`. Nothing runs until [`start`](Self::start).
    #[uniffi::constructor]
    pub fn new(profile: MobileProfile) -> Result<Arc<Self>, MobileError> {
        Ok(Arc::new(Self::new_plain(profile)?))
    }

    /// Start the node and connect the in-process client.
    pub async fn start(&self) -> Result<(), MobileError> {
        self.start_impl().await
    }

    /// Stop the node, releasing its stores. No-op if not running.
    pub async fn stop(&self) -> Result<(), MobileError> {
        self.stop_impl().await
    }

    /// Current lifecycle state.
    pub fn status(&self) -> NodeStatus {
        self.status_impl()
    }

    /// Loopback port of the running node's client API, or `None` when
    /// stopped (before start, or after stop). The URL to connect to is
    /// `ws://127.0.0.1:<port>/v1/contract/command?encodingProtocol=native`.
    pub fn api_port(&self) -> Option<u16> {
        self.api_port_impl()
    }

    /// Register (or replace) the host callbacks. May be called before start.
    pub fn set_update_listener(&self, listener: Arc<dyn ContractUpdateListener>) {
        self.set_update_listener_impl(listener);
    }

    /// Fetch a contract's state by base58 instance id, optionally subscribing
    /// to its updates (delivered through the listener).
    pub async fn get(&self, key: String, subscribe: bool) -> Result<GetResult, MobileError> {
        let id = parse_key(&key)?;
        self.with_client(async |client| client.get(id, subscribe).await)
            .await
    }

    /// Publish a contract (raw wasm + parameters) with an initial state.
    /// Returns the base58 instance id.
    pub async fn put(
        &self,
        wasm: Vec<u8>,
        params: Vec<u8>,
        state: Vec<u8>,
        subscribe: bool,
    ) -> Result<String, MobileError> {
        let contract = ContractContainer::Wasm(ContractWasmAPIVersion::V1(WrappedContract::new(
            Arc::new(ContractCode::from(wasm)),
            Parameters::from(params),
        )));
        self.with_client(async |client| client.put(contract, state, subscribe).await)
            .await
    }

    /// Apply a delta to a contract this node knows.
    pub async fn update_delta(&self, key: String, delta: Vec<u8>) -> Result<(), MobileError> {
        let id = parse_key(&key)?;
        self.with_client(async |client| client.update_delta(id, delta).await)
            .await
    }

    /// Subscribe to a contract's updates without fetching its state.
    pub async fn subscribe(&self, key: String) -> Result<(), MobileError> {
        let id = parse_key(&key)?;
        self.with_client(async |client| client.subscribe(id).await)
            .await
    }

    /// Peers this node is connected to. Network mode only; a local node answers
    /// with an error. Contract requests are rejected until this is at least one
    /// (the ring location comes from the first gateway connection).
    pub async fn connected_peers(&self) -> Result<u32, MobileError> {
        self.with_client(async |client| client.connected_peers().await)
            .await
    }

    /// Poll [`connected_peers`](Self::connected_peers) once a second until it
    /// reaches `min_peers` or `timeout_secs` elapse.
    pub async fn wait_for_peers(
        &self,
        min_peers: u32,
        timeout_secs: u64,
    ) -> Result<u32, MobileError> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            let peers = self.connected_peers().await?;
            if peers >= min_peers {
                return Ok(peers);
            }
            if std::time::Instant::now() >= deadline {
                return Err(MobileError::Timeout(format!(
                    "only {peers} peer(s) connected after {timeout_secs}s (wanted {min_peers})"
                )));
            }
            // Sleep on the crate runtime so this works from any executor.
            crate::node::runtime()
                .spawn(tokio::time::sleep(std::time::Duration::from_secs(1)))
                .await?;
        }
    }
}
