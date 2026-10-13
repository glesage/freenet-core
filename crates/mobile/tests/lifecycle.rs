//! The node lifecycle, driven the way Swift and Kotlin drive it: exported
//! async functions polled by a foreign executor, never by Tokio.

use std::path::Path;

use std::sync::Arc;
use std::time::{Duration, Instant};

use freenet_mobile::client::{ContractListener, ContractRef, UpdateKind, connect_client};
use freenet_mobile::fixtures::operation_fixtures;
use freenet_mobile::node::STOP_DEADLINE;
use freenet_mobile::{MobileNode, NodeMode, NodeSettings, NodeState};
use futures::executor::block_on;
use parking_lot::Mutex;

fn settings(root: &Path, mode: NodeMode) -> NodeSettings {
    NodeSettings {
        data_dir: root.join("data").display().to_string(),
        config_dir: root.join("config").display().to_string(),
        log_dir: root.join("logs").display().to_string(),
        cache_dir: None,
        mode,
        preferred_ws_port: None,
        network_port: None,
        gateways: vec![],
        wasm_backend: None,
        log_filter: None,
        module_cache_budget_bytes: None,
        max_hosting_storage_bytes: None,
        max_hosting_disk_bytes: None,
    }
}

#[test]
fn start_is_idempotent_and_stop_releases_the_store() {
    let root = tempfile::tempdir().unwrap();
    let node = MobileNode::new(settings(root.path(), NodeMode::Local)).unwrap();
    assert_eq!(node.status().state, NodeState::Stopped);

    let first = block_on(node.clone().start()).unwrap();
    assert!(first.ws_port > 0);
    assert_eq!(first.session, 1);
    assert_eq!(node.status().state, NodeState::Running);
    assert_eq!(node.status().ws_port, Some(first.ws_port));

    // A repeated start while running returns the same session.
    let again = block_on(node.clone().start()).unwrap();
    assert_eq!(again.session, first.session);
    assert_eq!(again.ws_port, first.ws_port);

    block_on(node.clone().stop()).unwrap();
    assert_eq!(node.status().state, NodeState::Stopped);
    assert_eq!(node.status().ws_port, None);
    // Stopping a stopped node does nothing.
    block_on(node.clone().stop()).unwrap();

    // The same data directory opens again in the same process.
    let second = block_on(node.clone().start()).unwrap();
    assert_eq!(second.session, first.session + 1);
    block_on(node.clone().stop()).unwrap();
}

#[test]
fn the_preferred_port_is_kept_across_restarts() {
    let root = tempfile::tempdir().unwrap();
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let mut s = settings(root.path(), NodeMode::Local);
    s.preferred_ws_port = Some(port);
    let node = MobileNode::new(s).unwrap();
    assert_eq!(block_on(node.clone().start()).unwrap().ws_port, port);
    block_on(node.clone().stop()).unwrap();
    assert_eq!(block_on(node.clone().start()).unwrap().ws_port, port);
    block_on(node.clone().stop()).unwrap();
}

#[test]
fn a_moved_container_discards_the_stale_config_and_starts() {
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("old-container");
    let node = MobileNode::new(settings(&old, NodeMode::Local)).unwrap();
    block_on(node.clone().start()).unwrap();
    block_on(node.clone().stop()).unwrap();
    drop(node);

    // iOS moves the container: same files, new absolute path.
    let new = root.path().join("new-container");
    std::fs::rename(&old, &new).unwrap();
    let node = MobileNode::new(settings(&new, NodeMode::Local)).unwrap();
    let info = block_on(node.clone().start()).unwrap();
    assert!(info.discarded_config);
    block_on(node.clone().stop()).unwrap();

    // The next start in place keeps the config it wrote.
    let info = block_on(node.clone().start()).unwrap();
    assert!(!info.discarded_config);
    block_on(node.clone().stop()).unwrap();
}

#[test]
fn operation_fixtures_run_against_a_local_node() {
    let root = tempfile::tempdir().unwrap();
    let node = MobileNode::new(settings(root.path(), NodeMode::Local)).unwrap();
    let info = block_on(node.clone().start()).unwrap();
    let values = block_on(operation_fixtures(info.ws_port)).unwrap();
    let get = |name: &str| {
        values
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .value
            .clone()
    };
    for value in &values {
        println!("{} = {}", value.name, value.value);
    }
    assert_eq!(get("op.get.state"), "010203");
    assert_eq!(get("op.get_after_update.state"), "0102030405");
    assert_eq!(&get("op.update.summary")[..8], "05000000");
    assert_eq!(get("op.cancel.timed_out"), "UncertainWrite");
    assert_eq!(get("op.cancel.read_after"), "0102030405060708");
    block_on(node.clone().stop()).unwrap();
}

fn config_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(config_files(&path));
        } else if path.file_name().is_some_and(|n| n == "config.toml") {
            found.push(path);
        }
    }
    found
}

#[test]
fn reports_stay_off_in_fresh_and_persisted_configs() {
    let root = tempfile::tempdir().unwrap();
    let node = MobileNode::new(settings(root.path(), NodeMode::Local)).unwrap();
    let fresh = block_on(node.clone().start()).unwrap();
    assert!(!fresh.telemetry_enabled);
    assert!(!fresh.otel_enabled);
    block_on(node.clone().stop()).unwrap();

    // A persisted config that turns both on is overridden.
    let files = config_files(&root.path().join("config"));
    assert!(!files.is_empty(), "the node persisted its config");
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let mut doc: toml::Table = toml::from_str(&text).unwrap();
        // Core flattens both report settings into the top level.
        for key in [
            "telemetry-enabled",
            "reference-ping-enabled",
            "otel-telemetry-enabled",
        ] {
            doc.insert(key.into(), true.into());
        }
        std::fs::write(&file, toml::to_string(&doc).unwrap()).unwrap();
    }
    let persisted = block_on(node.clone().start()).unwrap();
    assert!(!persisted.discarded_config);
    assert!(!persisted.telemetry_enabled);
    assert!(!persisted.otel_enabled);
    block_on(node.clone().stop()).unwrap();
}

struct Closed(Mutex<Vec<String>>);

impl ContractListener for Closed {
    fn on_update(&self, _contract: ContractRef, _kind: UpdateKind, _bytes: Vec<u8>) {}

    fn on_closed(&self, reason: String) {
        self.0.lock().push(reason);
    }
}

#[test]
fn stop_fits_its_deadline_frees_the_port_and_closes_clients() {
    let root = tempfile::tempdir().unwrap();
    let node = MobileNode::new(settings(root.path(), NodeMode::Local)).unwrap();
    let info = block_on(node.clone().start()).unwrap();
    let client = block_on(connect_client(info.ws_port)).unwrap();
    let wasm = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/backend-conformance.wasm"
    ))
    .unwrap();
    let put = block_on(client.put(wasm, vec![], vec![1, 2, 3], false, None)).unwrap();
    let listener = Arc::new(Closed(Mutex::new(Vec::new())));
    block_on(client.subscribe(put.contract.instance_id.clone(), listener.clone(), None)).unwrap();

    let started = Instant::now();
    block_on(node.clone().stop()).unwrap();
    let took = started.elapsed();
    assert!(
        took <= STOP_DEADLINE + Duration::from_millis(500),
        "stop took {took:?}"
    );
    assert_eq!(node.status().state, NodeState::Stopped);
    assert_eq!(node.status().last_error, None);
    std::net::TcpListener::bind(("127.0.0.1", info.ws_port)).expect("the port is free");

    let deadline = Instant::now() + Duration::from_secs(5);
    while listener.0.lock().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(listener.0.lock().len(), 1, "the subscription closed once");
    assert!(
        block_on(client.get(put.contract.instance_id.clone(), false, false, Some(1_000))).is_err(),
        "the old session's client is closed"
    );
}
