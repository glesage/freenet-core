//! Node lifecycle: start, stop, status.

use std::convert::Infallible;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use freenet::ShutdownHandle;
use freenet::local_node::{Executor, NodeConfig};
use freenet::server::serve_client_api;
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;

use crate::api::{ContractUpdateListener, NodeStatus};
use crate::client::{ClientHandle, ListenerSlot};
use crate::error::MobileError;
use crate::profile::{MobileProfile, NodeMode};

/// Time allowed for the client API socket to come up after the node starts.
const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Time allowed for a network node's event loop to exit after `shutdown()`.
const NETWORK_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// The one tokio runtime every node built by this crate runs on.
///
/// `freenet` adopts the ambient runtime when there is one and keeps global
/// handles to it, so all nodes in a process must share a single runtime that
/// is never dropped. Two worker threads: phones have few cores and the node's
/// heavy work runs on tokio's blocking pool anyway.
pub(crate) fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("freenet-mobile")
            .enable_all()
            .build()
            .expect("build the freenet-mobile tokio runtime")
    })
}

enum ModeHandle {
    Local {
        task: JoinHandle<anyhow::Result<()>>,
    },
    Network {
        shutdown: ShutdownHandle,
        task: JoinHandle<anyhow::Result<Infallible>>,
    },
}

struct Running {
    mode: ModeHandle,
    client: ClientHandle,
}

/// An embedded Freenet node. Construct with [`FreenetNode::new_plain`] (Rust)
/// or the UniFFI constructor, then [`start`](Self::start).
#[derive(uniffi::Object)]
pub struct FreenetNode {
    profile: MobileProfile,
    running: tokio::sync::Mutex<Option<Running>>,
    status: RwLock<NodeStatus>,
    listener: ListenerSlot,
}

impl FreenetNode {
    /// Plain Rust constructor (the FFI one wraps this in an `Arc`).
    pub fn new_plain(profile: MobileProfile) -> Result<Self, MobileError> {
        // Validate eagerly so a bad profile fails at construction, not at start.
        profile.config_args()?;
        Ok(Self {
            profile,
            running: tokio::sync::Mutex::new(None),
            status: RwLock::new(NodeStatus::Stopped),
            listener: Arc::new(RwLock::new(None)),
        })
    }

    fn set_status(&self, status: NodeStatus) {
        if let Ok(mut slot) = self.status.write() {
            *slot = status.clone();
        }
        let listener = self.listener.read().ok().and_then(|slot| slot.clone());
        if let Some(listener) = listener {
            listener.on_status(status);
        }
    }

    pub(crate) fn status_impl(&self) -> NodeStatus {
        self.status
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|_| NodeStatus::Failed {
                message: "status lock poisoned".into(),
            })
    }

    pub(crate) fn set_update_listener_impl(&self, listener: Arc<dyn ContractUpdateListener>) {
        if let Ok(mut slot) = self.listener.write() {
            *slot = Some(listener);
        }
    }

    /// Start the node described by the profile. Errors if already running.
    pub(crate) async fn start_impl(&self) -> Result<(), MobileError> {
        let mut running = self.running.lock().await;
        if running.is_some() {
            return Err(MobileError::InvalidState("node is already running".into()));
        }
        self.set_status(NodeStatus::Starting);
        match self.launch().await {
            Ok(r) => {
                *running = Some(r);
                self.set_status(NodeStatus::Running);
                Ok(())
            }
            Err(e) => {
                self.set_status(NodeStatus::Failed {
                    message: e.to_string(),
                });
                Err(e)
            }
        }
    }

    /// Launch the node on the crate runtime, then connect the in-process
    /// client. A node whose client fails to connect is stopped again before
    /// the error is returned.
    async fn launch(&self) -> Result<Running, MobileError> {
        let handle = runtime().handle();
        let mode = handle.spawn(start_mode(self.profile.clone())).await??;
        match ClientHandle::connect(
            handle,
            self.profile.ws_port,
            self.listener.clone(),
            CLIENT_CONNECT_TIMEOUT,
        )
        .await
        {
            Ok(client) => Ok(Running { mode, client }),
            Err(e) => {
                stop_mode(handle, mode).await;
                Err(e)
            }
        }
    }

    /// Stop the node. A no-op when it is not running.
    pub(crate) async fn stop_impl(&self) -> Result<(), MobileError> {
        let mut running = self.running.lock().await;
        let Some(Running { mode, client }) = running.take() else {
            return Ok(());
        };
        self.set_status(NodeStatus::Stopping);
        client.shutdown().await;
        stop_mode(runtime().handle(), mode).await;
        self.set_status(NodeStatus::Stopped);
        Ok(())
    }

    /// Run `f` against the live client, or fail if the node is not running.
    pub(crate) async fn with_client<T>(
        &self,
        f: impl AsyncFnOnce(&ClientHandle) -> Result<T, MobileError>,
    ) -> Result<T, MobileError> {
        let running = self.running.lock().await;
        let Some(Running { client, .. }) = running.as_ref() else {
            return Err(MobileError::InvalidState("node is not running".into()));
        };
        f(client).await
    }
}

/// Build and launch the node for `profile` on the current (crate) runtime.
async fn start_mode(profile: MobileProfile) -> Result<ModeHandle, MobileError> {
    let cfg = profile.build_config().await?;
    match profile.mode {
        NodeMode::Local => {
            let ws_api = cfg.ws_api.clone();
            let executor = Executor::from_config_local(Arc::new(cfg))
                .await
                .map_err(|e| MobileError::Startup(format!("local executor: {e}")))?;
            let task = tokio::spawn(freenet::run_local_node(executor, ws_api));
            Ok(ModeHandle::Local { task })
        }
        NodeMode::Network => {
            let clients = serve_client_api(cfg.ws_api.clone())
                .await
                .map_err(|e| MobileError::Startup(format!("client API: {e}")))?;
            let node = NodeConfig::new(cfg)
                .await
                .map_err(|e| MobileError::Startup(format!("node config: {e}")))?
                .build(clients)
                .await
                .map_err(|e| MobileError::Startup(format!("node build: {e}")))?;
            let shutdown = node.shutdown_handle();
            let task = tokio::spawn(async move { node.run().await });
            Ok(ModeHandle::Network { shutdown, task })
        }
    }
}

async fn stop_mode(handle: &Handle, mode: ModeHandle) {
    match mode {
        ModeHandle::Local { task } => {
            // The local request loop only returns on error; cancelling it drops
            // the executor and with it the store locks.
            task.abort();
            if let Err(e) = task.await
                && !e.is_cancelled()
            {
                tracing::warn!(%e, "local node task ended abnormally");
            }
        }
        ModeHandle::Network { shutdown, task } => {
            // Three-phase drain (stop admitting, finish in-flight, disconnect),
            // driven on the node's own runtime.
            if let Err(e) = handle.spawn(async move { shutdown.shutdown().await }).await {
                tracing::warn!(%e, "shutdown task ended abnormally");
            }
            match tokio::time::timeout(NETWORK_STOP_TIMEOUT, task).await {
                Ok(Ok(Ok(never))) => match never {},
                Ok(Ok(Err(cause))) => tracing::info!(%cause, "node event loop exited"),
                Ok(Err(join)) => tracing::warn!(%join, "node task ended abnormally"),
                Err(_) => tracing::warn!("node did not exit within {NETWORK_STOP_TIMEOUT:?}"),
            }
        }
    }
}
