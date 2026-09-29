//! The node lifecycle, driven the way Swift and Kotlin drive it: exported
//! async functions polled by a foreign executor, never by Tokio.

use std::path::Path;

use freenet_mobile::fixtures::operation_fixtures;
use freenet_mobile::{MobileNode, NodeMode, NodeSettings, NodeState};
use futures::executor::block_on;

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
    assert_eq!(get("op.cancel.timed_out"), "Timeout");
    assert_eq!(get("op.cancel.read_after"), "0102030405060708");
    block_on(node.clone().stop()).unwrap();
}
