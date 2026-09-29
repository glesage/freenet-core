//! The embedded node and its lifecycle.
//!
//! One coordinator runs every start and stop, so two never overlap. Start
//! binds a loopback port for the client API (the preferred port when it is
//! free), builds the node inside the process runtime and reports the resolved
//! port. Stop is explicit: it drains in-flight requests, shuts the node down
//! and waits for its tasks to release the port and store locks.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use freenet::ShutdownHandle;
use freenet::local_node::NodeConfig;
use freenet_stdlib::client_api::{ClientRequest, HostResponse, NodeQuery, QueryResponse, WebApi};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::error::{ClientErrorKind, MobileError};
use crate::runtime;
use crate::settings::{
    Layout, NodeMode, NodeSettings, WasmBackendChoice, bind_loopback, config_args,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Stopped,
    Starting,
    Running,
    Stopping,
    /// The last start failed, or the node exited without a stop call.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, uniffi::Record)]
pub struct NodeStatus {
    pub state: NodeState,
    pub ws_port: Option<u16>,
    pub connected_peers: u32,
    /// Increases on every successful start. Callbacks from an older session
    /// belong to a node that has since stopped.
    pub session: u64,
    pub last_error: Option<String>,
}

/// Milliseconds spent in each start phase.
#[derive(Debug, Clone, PartialEq, Serialize, uniffi::Record)]
pub struct StartTimings {
    pub layout_ms: f64,
    pub config_ms: f64,
    pub client_api_ms: f64,
    pub node_build_ms: f64,
    pub api_ready_ms: f64,
    pub total_ms: f64,
    /// Start attempts, above 1 when a just-stopped node still held its store.
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, uniffi::Record)]
pub struct NodeInfo {
    pub ws_port: u16,
    /// `http://127.0.0.1:<port>`: web apps load from here.
    pub http_base: String,
    /// The client API WebSocket URL, with the native encoding selected.
    pub ws_url: String,
    pub mode: NodeMode,
    pub wasm_backend: WasmBackendChoice,
    pub session: u64,
    pub data_dir: String,
    /// A stale persisted config was removed before this start.
    pub discarded_config: bool,
    pub timings: StartTimings,
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum NodeEvent {
    StateChanged {
        status: NodeStatus,
    },
    PeersChanged {
        count: u32,
    },
    /// The node stopped without a stop call.
    Exited {
        reason: String,
    },
}

/// Receives node events on a runtime thread. Implementations hand them to the
/// platform's own executor.
#[uniffi::export(with_foreign)]
pub trait NodeEventListener: Send + Sync {
    fn on_event(&self, event: NodeEvent);
}

struct Running {
    info: NodeInfo,
    shutdown: ShutdownHandle,
    run_task: JoinHandle<()>,
    monitor: Option<JoinHandle<()>>,
}

#[derive(uniffi::Object)]
pub struct MobileNode {
    settings: NodeSettings,
    coordinator: tokio::sync::Mutex<Option<Running>>,
    status: Mutex<NodeStatus>,
    info: Mutex<Option<NodeInfo>>,
    listener: Mutex<Option<Arc<dyn NodeEventListener>>>,
    sessions: AtomicU64,
}

pub(crate) fn ws_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}/v1/contract/command?encodingProtocol=native")
}

fn elapsed_ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// Open a client API connection with the native (bincode) encoding.
pub(crate) async fn connect_native(port: u16) -> Result<WebApi, MobileError> {
    let (stream, _) = tokio_tungstenite::connect_async(ws_url(port))
        .await
        .map_err(|e| MobileError::client(ClientErrorKind::Disconnected, e.to_string()))?;
    Ok(WebApi::start(stream))
}

/// Ask the node how many peers it has open connections to.
pub(crate) async fn query_connected_peers(api: &mut WebApi) -> Result<u32, MobileError> {
    api.send(ClientRequest::NodeQueries(NodeQuery::ConnectedPeers))
        .await
        .map_err(|e| MobileError::client(ClientErrorKind::Disconnected, e.to_string()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, api.recv()).await {
            Ok(Ok(HostResponse::QueryResponse(QueryResponse::ConnectedPeers { peers }))) => {
                return Ok(peers.len() as u32);
            }
            Ok(Ok(_other)) => continue,
            Ok(Err(e)) => {
                let kind = crate::error::classify_client_error(&e);
                return Err(MobileError::client(kind, e.to_string()));
            }
            Err(_) => {
                return Err(MobileError::Timeout {
                    what: "the connected-peers query".into(),
                });
            }
        }
    }
}

/// Core returns an `anyhow` chain; a store still locked by a node that stopped
/// a moment ago shows up as redb's "already open" error.
fn is_store_locked(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("already open") || text.contains("databasealreadyopen")
}

enum LaunchError {
    StoreLocked(String),
    Fatal(MobileError),
}

impl From<MobileError> for LaunchError {
    fn from(err: MobileError) -> Self {
        Self::Fatal(err)
    }
}

fn start_failure(stage: &str, err: anyhow::Error) -> LaunchError {
    if is_store_locked(&err) {
        return LaunchError::StoreLocked(format!("{stage}: {err:#}"));
    }
    let text = format!("{err:#}");
    if text.contains("without gateways") {
        return LaunchError::Fatal(MobileError::NoGateways { reason: text });
    }
    LaunchError::Fatal(MobileError::StartFailed {
        reason: format!("{stage}: {text}"),
    })
}

impl MobileNode {
    fn emit(&self, event: NodeEvent) {
        let listener = self.listener.lock().clone();
        if let Some(listener) = listener {
            listener.on_event(event);
        }
    }

    fn update_status(&self, change: impl FnOnce(&mut NodeStatus)) {
        let status = {
            let mut status = self.status.lock();
            change(&mut status);
            status.clone()
        };
        self.emit(NodeEvent::StateChanged { status });
    }

    async fn start_on_runtime(self: Arc<Self>) -> Result<NodeInfo, MobileError> {
        let mut slot = self.coordinator.lock().await;
        if let Some(running) = slot.as_ref() {
            if !running.run_task.is_finished() {
                return Ok(running.info.clone());
            }
        }
        if let Some(monitor) = slot.take().and_then(|exited| exited.monitor) {
            monitor.abort();
        }
        self.update_status(|s| {
            s.state = NodeState::Starting;
            s.last_error = None;
        });
        match self.clone().launch().await {
            Ok(mut running) => {
                let info = running.info.clone();
                *self.info.lock() = Some(info.clone());
                self.update_status(|s| {
                    s.state = NodeState::Running;
                    s.ws_port = Some(info.ws_port);
                    s.session = info.session;
                    s.connected_peers = 0;
                });
                // After the status carries the new session, which the monitor
                // checks to know it is still current.
                running.monitor = Some(tokio::spawn(monitor_peers(
                    Arc::downgrade(&self),
                    info.ws_port,
                    info.session,
                )));
                *slot = Some(running);
                tracing::info!(
                    port = info.ws_port,
                    session = info.session,
                    "freenet-mobile node running"
                );
                Ok(info)
            }
            Err(err) => {
                self.update_status(|s| {
                    s.state = NodeState::Failed;
                    s.ws_port = None;
                    s.last_error = Some(err.to_string());
                });
                Err(err)
            }
        }
    }

    async fn launch(self: Arc<Self>) -> Result<Running, MobileError> {
        let started = Instant::now();
        let layout = Layout::prepare(&self.settings)?;
        let layout_ms = elapsed_ms(started);
        let mut attempts = 0;
        let mut backoff = Duration::from_millis(25);
        loop {
            attempts += 1;
            match self
                .clone()
                .try_launch(&layout, started, layout_ms, attempts)
                .await
            {
                Ok(running) => return Ok(running),
                // Core releases the store within seconds of a stop (#4401):
                // retry quickly at first, then every half second, for about
                // ten seconds in all.
                Err(LaunchError::StoreLocked(reason)) if attempts < 28 => {
                    tracing::info!(
                        attempts,
                        "store still locked by the previous node: {reason}"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_millis(500));
                }
                Err(LaunchError::StoreLocked(reason)) => {
                    return Err(MobileError::StartFailed { reason });
                }
                Err(LaunchError::Fatal(err)) => return Err(err),
            }
        }
    }

    async fn try_launch(
        self: Arc<Self>,
        layout: &Layout,
        started: Instant,
        layout_ms: f64,
        attempts: u32,
    ) -> Result<Running, LaunchError> {
        let listener = bind_loopback(self.settings.preferred_ws_port)?;
        listener
            .set_nonblocking(true)
            .map_err(|e| MobileError::StartFailed {
                reason: e.to_string(),
            })?;
        let port = listener
            .local_addr()
            .map_err(|e| MobileError::StartFailed {
                reason: e.to_string(),
            })?
            .port();

        let phase = Instant::now();
        let args = config_args(&self.settings, layout, port)?;
        let mut config = args
            .build()
            .await
            .map_err(|e| start_failure("configuration", e))?;
        // The resolved port, never a persisted one.
        config.ws_api.address = Ipv4Addr::LOCALHOST.into();
        config.ws_api.port = port;
        config.ws_api.webapp_cache_dir = layout.webapp_cache.clone();
        let config_ms = elapsed_ms(phase);

        let phase = Instant::now();
        let clients =
            freenet::server::serve_client_api_with_listener(config.ws_api.clone(), listener)
                .await
                .map_err(|e| MobileError::StartFailed {
                    reason: format!("client API: {e}"),
                })?;
        let client_api_ms = elapsed_ms(phase);

        let phase = Instant::now();
        let node = NodeConfig::new(config)
            .await
            .map_err(|e| start_failure("node configuration", e))?
            .build(clients)
            .await
            .map_err(|e| start_failure("node build", e))?;
        let node_build_ms = elapsed_ms(phase);

        let session = self.sessions.fetch_add(1, Ordering::SeqCst) + 1;
        let shutdown = node.shutdown_handle();
        let weak = Arc::downgrade(&self);
        let run_task = tokio::spawn(async move {
            let result = node.run().await;
            if let Some(this) = weak.upgrade() {
                this.on_node_exit(session, result.err());
            }
        });

        let phase = Instant::now();
        if let Err(err) = wait_for_client_api(port, &run_task).await {
            shutdown.shutdown().await;
            run_task.abort();
            return Err(LaunchError::Fatal(err));
        }
        let api_ready_ms = elapsed_ms(phase);

        let info = NodeInfo {
            ws_port: port,
            http_base: format!("http://127.0.0.1:{port}"),
            ws_url: ws_url(port),
            mode: self.settings.mode,
            wasm_backend: self.settings.wasm_backend().into(),
            session,
            data_dir: layout.data.display().to_string(),
            discarded_config: layout.discarded_config,
            timings: StartTimings {
                layout_ms,
                config_ms,
                client_api_ms,
                node_build_ms,
                api_ready_ms,
                total_ms: elapsed_ms(started),
                attempts,
            },
        };
        Ok(Running {
            info,
            shutdown,
            run_task,
            monitor: None,
        })
    }

    fn on_node_exit(&self, session: u64, error: Option<anyhow::Error>) {
        let stopping = {
            let status = self.status.lock();
            status.session != session || status.state == NodeState::Stopping
        };
        if stopping {
            return;
        }
        let graceful = error
            .as_ref()
            .is_some_and(freenet::listener_exit_is_graceful);
        let reason = error
            .map(|e| format!("{e:#}"))
            .unwrap_or_else(|| "the node exited".into());
        tracing::warn!(
            graceful,
            "freenet-mobile node exited without a stop call: {reason}"
        );
        self.update_status(|s| {
            s.state = NodeState::Failed;
            s.ws_port = None;
            s.connected_peers = 0;
            s.last_error = Some(reason.clone());
        });
        self.emit(NodeEvent::Exited { reason });
    }

    async fn stop_on_runtime(self: Arc<Self>) -> Result<(), MobileError> {
        let mut slot = self.coordinator.lock().await;
        let Some(running) = slot.take() else {
            self.update_status(|s| {
                if s.state != NodeState::Failed {
                    s.state = NodeState::Stopped;
                }
                s.ws_port = None;
            });
            return Ok(());
        };
        self.update_status(|s| s.state = NodeState::Stopping);
        if let Some(monitor) = running.monitor {
            monitor.abort();
        }
        running.shutdown.shutdown().await;
        let mut run_task = running.run_task;
        if tokio::time::timeout(Duration::from_secs(10), &mut run_task)
            .await
            .is_err()
        {
            tracing::warn!("the node did not exit within 10 s of shutdown; aborting it");
            run_task.abort();
            let _ = run_task.await;
        }
        // The client API's listener closes a moment after the node's tasks
        // end. Wait for the port, so the next start gets it back and web apps
        // keep the same origin.
        wait_for_port_release(running.info.ws_port).await;
        *self.info.lock() = None;
        self.update_status(|s| {
            s.state = NodeState::Stopped;
            s.ws_port = None;
            s.connected_peers = 0;
        });
        tracing::info!("freenet-mobile node stopped");
        Ok(())
    }

    fn running_port(&self) -> Result<u16, MobileError> {
        let status = self.status.lock();
        match (status.state, status.ws_port) {
            (NodeState::Running, Some(port)) => Ok(port),
            _ => Err(MobileError::NotRunning),
        }
    }

    pub(crate) fn current_info(&self) -> Option<NodeInfo> {
        self.info.lock().clone()
    }
}

#[uniffi::export]
impl MobileNode {
    #[uniffi::constructor]
    pub fn new(settings: NodeSettings) -> Result<Arc<Self>, MobileError> {
        settings.validate()?;
        crate::logging::install(
            std::path::Path::new(&settings.log_dir),
            settings.log_filter.as_deref(),
        );
        Ok(Arc::new(Self {
            settings,
            coordinator: tokio::sync::Mutex::new(None),
            status: Mutex::new(NodeStatus {
                state: NodeState::Stopped,
                ws_port: None,
                connected_peers: 0,
                session: 0,
                last_error: None,
            }),
            info: Mutex::new(None),
            listener: Mutex::new(None),
            sessions: AtomicU64::new(0),
        }))
    }

    pub fn set_listener(&self, listener: Option<Arc<dyn NodeEventListener>>) {
        *self.listener.lock() = listener;
    }

    pub fn settings(&self) -> NodeSettings {
        self.settings.clone()
    }

    pub fn status(&self) -> NodeStatus {
        self.status.lock().clone()
    }

    /// The running node's details, or `None` when it is not running.
    pub fn info(&self) -> Option<NodeInfo> {
        self.current_info()
    }

    /// Start the node, or return the running node's details. Repeated calls
    /// while it runs return the same session.
    pub async fn start(self: Arc<Self>) -> Result<NodeInfo, MobileError> {
        runtime::run(self.start_on_runtime()).await
    }

    /// Stop the node. Stopping a stopped node does nothing.
    pub async fn stop(self: Arc<Self>) -> Result<(), MobileError> {
        runtime::run(self.stop_on_runtime()).await
    }

    /// The node's open peer connections, asked of the node now.
    pub async fn connected_peers(self: Arc<Self>) -> Result<u32, MobileError> {
        let port = self.running_port()?;
        runtime::run(async move {
            let mut api = connect_native(port).await?;
            query_connected_peers(&mut api).await
        })
        .await
    }

    /// Wait until the node has at least `min_peers` open peer connections.
    /// Network requests sent before the first connection fail with no peers.
    pub async fn wait_for_peers(
        self: Arc<Self>,
        min_peers: u32,
        timeout_ms: u64,
    ) -> Result<u32, MobileError> {
        let port = self.running_port()?;
        runtime::run(async move {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let mut api = connect_native(port).await?;
            loop {
                let peers = query_connected_peers(&mut api).await?;
                if peers >= min_peers {
                    return Ok(peers);
                }
                if Instant::now() >= deadline {
                    return Err(MobileError::Timeout {
                        what: format!("{min_peers} connected peer(s), have {peers}"),
                    });
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
    }

    /// Where the node serves the web app in website container `contract_id`.
    pub fn web_url(&self, contract_id: String) -> Result<String, MobileError> {
        let port = self.running_port()?;
        Ok(format!(
            "http://127.0.0.1:{port}/v1/contract/web/{contract_id}/"
        ))
    }
}

/// Wait up to 3 s until `port` can be bound again.
async fn wait_for_port_release(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tracing::warn!(port, "the client API port was still in use 3 s after stop");
}

/// Wait until the client API accepts a WebSocket connection.
async fn wait_for_client_api(port: u16, run_task: &JoinHandle<()>) -> Result<(), MobileError> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if run_task.is_finished() {
            return Err(MobileError::StartFailed {
                reason: "the node exited while starting".into(),
            });
        }
        match connect_native(port).await {
            Ok(api) => {
                drop(api);
                return Ok(());
            }
            Err(err) if Instant::now() >= deadline => {
                return Err(MobileError::StartFailed {
                    reason: format!("the client API did not accept connections: {err}"),
                });
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Track the peer count and report changes, until the node stops.
async fn monitor_peers(node: Weak<MobileNode>, port: u16, session: u64) {
    let mut api: Option<WebApi> = None;
    let mut last = None;
    loop {
        let interval = match node.upgrade() {
            None => return,
            Some(node) if node.status.lock().session != session => return,
            Some(node) if node.settings.mode == NodeMode::Local => Duration::from_secs(5),
            Some(_) => Duration::from_secs(1),
        };
        if api.is_none() {
            api = connect_native(port).await.ok();
        }
        let count = match api.as_mut() {
            Some(conn) => match query_connected_peers(conn).await {
                Ok(count) => Some(count),
                Err(_) => {
                    api = None;
                    None
                }
            },
            None => None,
        };
        if let (Some(count), Some(node)) = (count, node.upgrade()) {
            if last != Some(count) {
                last = Some(count);
                node.update_status(|s| s.connected_peers = count);
                node.emit(NodeEvent::PeersChanged { count });
            }
        }
        tokio::time::sleep(interval).await;
    }
}
