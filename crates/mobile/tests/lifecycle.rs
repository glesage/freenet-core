//! Start/stop lifecycle of the embedded node, both modes.

mod common;

use std::time::Duration;

use common::*;
use freenet_mobile::{FreenetNode, MobileError, NodeStatus};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_node_starts_and_stops() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), reserve_port())).await?;
    assert_eq!(node.status(), NodeStatus::Running);
    // The node lays its stores out under the explicit data dir, nowhere else.
    assert!(root.path().join("data").join("db").join("local").is_dir());
    // Node queries are a network-mode feature; the local loop rejects them.
    assert!(node.connected_peers().await.is_err());
    node.stop().await?;
    assert_eq!(node.status(), NodeStatus::Stopped);
    // Stopping twice is a no-op, not an error.
    node.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starting_twice_is_rejected() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), reserve_port())).await?;
    let err = node.start().await.expect_err("second start must fail");
    assert!(matches!(err, MobileError::InvalidState(_)), "{err}");
    assert_eq!(node.status(), NodeStatus::Running);
    node.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_fail_cleanly_when_stopped() {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = FreenetNode::new_plain(local_profile(root.path(), 7509)).expect("build node");
    let err = node
        .get("6Sf2buCM1LzU5EhscNvjeNqPYbQbtvKSkzC6EFUy8Jjh".into(), false)
        .await
        .expect_err("get on a stopped node must fail");
    assert!(matches!(err, MobileError::InvalidState(_)), "{err}");
}

/// Restarting against the same directories must work: the first run's store
/// locks (redb) have to be released by `stop`, and the second run must reopen
/// the same data.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_node_restarts_on_the_same_dirs() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let port = reserve_port();
    let profile = local_profile(root.path(), port);
    let node = FreenetNode::new_plain(profile)?;
    release_port(port);

    let (wasm, params) = test_contract();
    node.start().await?;
    let key = node
        .put(wasm.clone(), params.clone(), empty_todo_list(), false)
        .await?;
    node.stop().await?;

    node.start().await?;
    let got = tokio::time::timeout(Duration::from_secs(30), node.get(key.clone(), false))
        .await
        .expect("get after restart must not hang")?;
    assert_eq!(got.key, key);
    assert_eq!(got.state, empty_todo_list());
    node.stop().await
}

/// Network mode with an explicit, unreachable gateway: the node must come up
/// (transport bound, client API serving) and shut down cleanly without ever
/// having joined anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_node_starts_and_stops_without_joining() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let ws_port = reserve_port();
    let net_port = reserve_port();
    let gw_port = reserve_port();
    let gateway = unreachable_gateway(root.path(), gw_port);
    let node = FreenetNode::new_plain(network_profile(root.path(), ws_port, net_port, gateway))?;
    release_port(ws_port);
    release_port(net_port);
    node.start().await?;
    assert_eq!(node.status(), NodeStatus::Running);
    assert!(root.path().join("data").join("db").is_dir());
    // Nobody answers at the gateway, so the peer count stays at zero and a
    // bounded wait times out instead of hanging.
    assert_eq!(node.connected_peers().await?, 0);
    let err = node
        .wait_for_peers(1, 2)
        .await
        .expect_err("no peer can appear behind an unreachable gateway");
    assert!(matches!(err, MobileError::Timeout(_)), "{err}");
    tokio::time::timeout(Duration::from_secs(45), node.stop())
        .await
        .expect("network stop must complete")?;
    assert_eq!(node.status(), NodeStatus::Stopped);
    release_port(gw_port);
    Ok(())
}

async fn run_cycles(cycles: usize) -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let port = reserve_port();
    let node = FreenetNode::new_plain(local_profile(root.path(), port))?;
    release_port(port);

    // First cycle warms every lazily created resource (runtime threads, epoch
    // ticker, compiled wasm caches); measure after it.
    node.start().await?;
    node.stop().await?;
    let (threads_before, fds_before) = settled_resource_counts();

    for i in 0..cycles {
        node.start()
            .await
            .unwrap_or_else(|e| panic!("cycle {i}: start failed: {e}"));
        assert_eq!(node.status(), NodeStatus::Running);
        node.stop()
            .await
            .unwrap_or_else(|e| panic!("cycle {i}: stop failed: {e}"));
        assert_eq!(node.status(), NodeStatus::Stopped);
    }

    let (threads_after, fds_after) = settled_resource_counts();
    // Blocking-pool threads come and go; a leak would grow with `cycles`.
    assert!(
        threads_after <= threads_before + 4,
        "thread count grew from {threads_before} to {threads_after} over {cycles} cycles"
    );
    assert!(
        fds_after <= fds_before + 8,
        "open fds grew from {fds_before} to {fds_after} over {cycles} cycles"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twenty_five_start_stop_cycles_do_not_leak() -> Result<(), MobileError> {
    run_cycles(25).await
}

/// Phase 1 exit criterion. Slow; run with `cargo test -p freenet-mobile -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow: 100 start/stop cycles"]
async fn one_hundred_start_stop_cycles_do_not_leak() -> Result<(), MobileError> {
    run_cycles(100).await
}

/// A `config.toml` persisted by another app container (iOS reinstall) names
/// paths that no longer exist. The profile wins: the node must still start,
/// and the rewritten file must name the profile's data dir.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_config_from_a_moved_container_is_replaced() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let profile = local_profile(root.path(), reserve_port());
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        "mode = \"local\"\ndata_dir = \"/nonexistent/old-container/data\"\n\
         transport_keypair = \"/nonexistent/old-container/data/secrets/local/transport_keypair\"\n",
    )
    .expect("write stale config");

    let node = start_node(profile.clone()).await?;
    assert_eq!(node.status(), NodeStatus::Running);
    let rewritten = std::fs::read_to_string(config_dir.join("config.toml")).expect("config.toml");
    assert!(
        rewritten.contains(&profile.data_dir),
        "rewritten config must name the profile's data dir:\n{rewritten}"
    );
    assert!(
        !rewritten.contains("old-container"),
        "stale paths must be gone:\n{rewritten}"
    );
    node.stop().await
}
