//! Shared helpers for the freenet-mobile host tests.
//!
//! Every test gets its own temp dir tree and its own reserved loopback port so
//! the suite can run in parallel inside one process.

#![allow(dead_code)]

use std::path::Path;
use std::sync::{Mutex, Once};
use std::time::Duration;

use freenet_mobile::{
    ContractUpdateListener, FreenetNode, MobileError, MobileProfile, NodeMode, NodeStatus,
};
use freenet_stdlib::prelude::Parameters;
use tokio::sync::Notify;

/// Name of the workspace test contract (a JSON todo list) used for PUT/GET/UPDATE.
pub const TEST_CONTRACT: &str = "test-contract-integration";

/// Install a quiet test logger once per process. `RUST_LOG` still overrides.
pub fn init_test_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::filter::{EnvFilter, LevelFilter};
        let filter = EnvFilter::builder()
            .with_default_directive(LevelFilter::WARN.into())
            .from_env_lossy();
        // Another subscriber (e.g. from a harness) may already be installed.
        if tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init()
            .is_err()
        {
            eprintln!("test logger already installed");
        }
    });
}

/// Reserve a loopback port. It stays bound until [`release_port`] so parallel
/// tests never pick the same one.
pub fn reserve_port() -> u16 {
    freenet::test_utils::reserve_local_port().expect("reserve loopback port")
}

/// Release a port reserved with [`reserve_port`] so the node can bind it.
pub fn release_port(port: u16) {
    freenet::test_utils::release_local_port(port);
}

/// A local-mode profile rooted at `root`.
pub fn local_profile(root: &Path, ws_port: u16) -> MobileProfile {
    MobileProfile {
        mode: NodeMode::Local,
        data_dir: root.join("data").to_string_lossy().into_owned(),
        config_dir: root.join("config").to_string_lossy().into_owned(),
        log_dir: root.join("logs").to_string_lossy().into_owned(),
        ws_port,
        network_port: None,
        gateways: Vec::new(),
        log_to_stderr: true,
    }
}

/// A network-mode profile that never joins anything: it points at one gateway
/// entry that will not answer. An explicit gateway list means the public index
/// is not fetched, so no request leaves the machine.
pub fn network_profile(
    root: &Path,
    ws_port: u16,
    network_port: u16,
    gateway: String,
) -> MobileProfile {
    MobileProfile {
        mode: NodeMode::Network,
        data_dir: root.join("data").to_string_lossy().into_owned(),
        config_dir: root.join("config").to_string_lossy().into_owned(),
        log_dir: root.join("logs").to_string_lossy().into_owned(),
        ws_port,
        network_port: Some(network_port),
        gateways: vec![gateway],
        log_to_stderr: true,
    }
}

/// A `--gateway`-style JSON entry for a loopback UDP port nobody listens on,
/// with a freshly generated public key saved under `dir`.
pub fn unreachable_gateway(dir: &Path, port: u16) -> String {
    let key = freenet::dev_tool::TransportKeypair::new();
    let pub_path = dir.join("unreachable-gateway.pub");
    key.public()
        .save(&pub_path)
        .expect("save gateway public key");
    serde_json::json!({
        "address": format!("127.0.0.1:{port}"),
        "public_key": pub_path,
        "location": 0.5,
    })
    .to_string()
}

/// Build a node for `profile` and start it, releasing the reserved ws port
/// right before the bind.
pub async fn start_node(profile: MobileProfile) -> Result<FreenetNode, MobileError> {
    let node = FreenetNode::new_plain(profile.clone())?;
    release_port(profile.ws_port);
    node.start().await?;
    Ok(node)
}

/// Compile (once per process) and load the test contract, returning the raw
/// wasm and the parameters the way a host would hand them to `put`.
pub fn test_contract() -> (Vec<u8>, Vec<u8>) {
    freenet::test_utils::ensure_contract_compiled(TEST_CONTRACT).expect("compile test contract");
    let container =
        freenet::test_utils::load_contract(TEST_CONTRACT, Parameters::from(Vec::<u8>::new()))
            .expect("load test contract");
    let params = container.params();
    (container.data().to_vec(), params.as_ref().to_vec())
}

/// Initial state accepted by the test contract: an empty todo list.
pub fn empty_todo_list() -> Vec<u8> {
    freenet::test_utils::create_empty_todo_list()
}

/// A delta accepted by the test contract: add one task.
pub fn add_task_delta(id: u64, title: &str) -> Vec<u8> {
    format!(
        r#"{{"Add":{{"id":{id},"title":"{title}","description":"from freenet-mobile","completed":false,"priority":3}}}}"#
    )
    .into_bytes()
}

/// Records every callback so tests can assert on them.
#[derive(Default)]
pub struct RecordingListener {
    pub updates: Mutex<Vec<Update>>,
    pub statuses: Mutex<Vec<NodeStatus>>,
    pub notify: Notify,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub key: String,
    pub state: Option<Vec<u8>>,
    pub delta: Option<Vec<u8>>,
}

impl RecordingListener {
    /// Wait until at least `n` updates have been recorded, or fail after `within`.
    pub async fn wait_for_updates(&self, n: usize, within: Duration) -> Vec<Update> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            {
                let updates = self.updates.lock().expect("updates lock");
                if updates.len() >= n {
                    return updates.clone();
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "expected {n} update notification(s) within {within:?}"
            );
            // Wake on the next notification or at the deadline; the loop head
            // decides which it was.
            let _woke = tokio::time::timeout(remaining, self.notify.notified()).await;
        }
    }
}

impl ContractUpdateListener for RecordingListener {
    fn on_update(&self, key: String, state: Option<Vec<u8>>, delta: Option<Vec<u8>>) {
        self.updates
            .lock()
            .expect("updates lock")
            .push(Update { key, state, delta });
        self.notify.notify_waiters();
    }

    fn on_status(&self, status: NodeStatus) {
        self.statuses.lock().expect("statuses lock").push(status);
    }
}

/// Number of threads in this process (macOS via `ps -M`, Linux via procfs).
pub fn thread_count() -> usize {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Threads:") {
                return rest.trim().parse().expect("Threads: value");
            }
        }
    }
    let out = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .expect("run ps -M");
    let text = String::from_utf8_lossy(&out.stdout);
    // One header line, then one line per thread.
    text.lines().count().saturating_sub(1)
}

/// Number of open file descriptors in this process.
pub fn open_fd_count() -> usize {
    let dir = if Path::new("/proc/self/fd").exists() {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    std::fs::read_dir(dir).expect("list fds").count()
}

/// Sample thread and fd counts for up to two seconds and return the lowest
/// pair seen, so threads still winding down do not count as leaks.
pub fn settled_resource_counts() -> (usize, usize) {
    let mut best = (usize::MAX, usize::MAX);
    for _ in 0..20 {
        let sample = (thread_count(), open_fd_count());
        best = (best.0.min(sample.0), best.1.min(sample.1));
        std::thread::sleep(Duration::from_millis(100));
    }
    best
}
