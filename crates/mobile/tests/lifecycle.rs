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

/// The status code of a plain HTTP GET to the node, with extra header lines.
fn http_status(port: u16, path: &str, extra: &str) -> u16 {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).unwrap();
    std::str::from_utf8(&head[9..12]).unwrap().parse().unwrap()
}

#[test]
fn only_callers_with_the_session_secret_reach_the_node() {
    let root = tempfile::tempdir().unwrap();
    let node = MobileNode::new(settings(root.path(), NodeMode::Local)).unwrap();
    let info = block_on(node.clone().start()).unwrap();
    let port = info.ws_port;
    assert!(!info.embedder_secret.is_empty());

    // Another process: no secret, with or without an Origin.
    assert_eq!(http_status(port, "/", ""), 403);
    assert_eq!(http_status(port, "/", "Origin: http://127.0.0.1\r\n"), 403);
    let shell = "/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/";
    assert_eq!(
        http_status(port, shell, ""),
        403,
        "the shell page issues app tokens"
    );
    let upgrade = "Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nConnection: Upgrade\r\n";
    assert_eq!(
        http_status(
            port,
            "/v1/contract/command?encodingProtocol=native",
            upgrade
        ),
        403
    );
    // A made-up app token does not help.
    assert_eq!(
        http_status(
            port,
            "/v1/contract/command?encodingProtocol=native&authToken=made-up",
            upgrade
        ),
        403
    );

    // The host: the secret as a header (SDK) or as the web view's cookie.
    let header = format!("X-Freenet-Embedder: {}\r\n", info.embedder_secret);
    assert_ne!(http_status(port, "/", &header), 403);
    let cookie = format!(
        "Cookie: theme=dark; freenet-embedder={}\r\n",
        info.embedder_secret
    );
    assert_ne!(http_status(port, "/", &cookie), 403);
    let wrong = format!("X-Freenet-Embedder: {}x\r\n", info.embedder_secret);
    assert_eq!(http_status(port, "/", &wrong), 403);
    // The SDK client presents it by itself.
    block_on(connect_client(port)).unwrap();

    // The next session has a new secret; the old one stops working.
    block_on(node.clone().stop()).unwrap();
    let next = block_on(node.clone().start()).unwrap();
    assert_ne!(next.embedder_secret, info.embedder_secret);
    let old = format!("X-Freenet-Embedder: {}\r\n", info.embedder_secret);
    assert_eq!(http_status(next.ws_port, "/", &old), 403);
    block_on(node.clone().stop()).unwrap();
}
