//! Start/stop lifecycle of the embedded node, both modes.

mod common;

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use freenet::config::{ConfigArgs, ConfigPathsArgs, NetworkArgs, SecretArgs, WebsocketApiArgs};
use freenet::local_node::NodeConfig;
use freenet::server::serve_client_api;
use freenet_mobile::{FreenetNode, MobileError, NodeStatus};
use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
};
use freenet_stdlib::prelude::ContractInstanceId;
use tokio_tungstenite::connect_async;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_node_starts_and_stops() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let port = reserve_port();
    let node = FreenetNode::new_plain(local_profile(root.path(), Some(port)))?;
    // Registered before start so every transition is observed.
    let listener = Arc::new(RecordingListener::default());
    node.set_update_listener(listener.clone());
    release_port(port);
    node.start().await?;
    assert_eq!(node.status(), NodeStatus::Running);
    // The node lays its stores out under the explicit data dir, nowhere else.
    assert!(root.path().join("data").join("db").join("local").is_dir());
    // Node queries are a network-mode feature; the local loop rejects them.
    assert!(node.connected_peers().await.is_err());
    node.stop().await?;
    assert_eq!(node.status(), NodeStatus::Stopped);
    // Stopping twice is a no-op, not an error, and emits no status.
    node.stop().await?;
    assert_eq!(
        listener.statuses.lock().expect("statuses lock").clone(),
        vec![
            NodeStatus::Starting,
            NodeStatus::Running,
            NodeStatus::Stopping,
            NodeStatus::Stopped,
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starting_twice_is_rejected() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = start_node(local_profile(root.path(), Some(reserve_port()))).await?;
    let err = node.start().await.expect_err("second start must fail");
    assert!(matches!(err, MobileError::InvalidState(_)), "{err}");
    assert_eq!(node.status(), NodeStatus::Running);
    node.stop().await
}

/// `ws_port: None` must not surface a live port before start, must bind and
/// report a real, connectable one once running, and must forget it again
/// after stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unset_ws_port_binds_an_ephemeral_port_and_reports_it() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = FreenetNode::new_plain(local_profile(root.path(), None))?;
    assert_eq!(node.api_port(), None, "no port before start");
    node.start().await?;
    let port = node.api_port().expect("running node reports its port");
    assert_ne!(port, 0);
    // A second, independent connection proves the number is real.
    let url = format!("ws://127.0.0.1:{port}/v1/contract/command?encodingProtocol=native");
    connect_async(&url)
        .await
        .expect("connect to the reported port");
    node.stop().await?;
    assert_eq!(node.api_port(), None, "no port after stop");
    Ok(())
}

/// Two nodes in the same process must never be handed the same ephemeral
/// port — each reserves and releases its own listener independently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_ephemeral_nodes_in_one_process_get_distinct_ports() -> Result<(), MobileError> {
    init_test_logging();
    let root_a = tempfile::tempdir().expect("tempdir a");
    let root_b = tempfile::tempdir().expect("tempdir b");
    let node_a = FreenetNode::new_plain(local_profile(root_a.path(), None))?;
    let node_b = FreenetNode::new_plain(local_profile(root_b.path(), None))?;
    node_a.start().await?;
    node_b.start().await?;
    let port_a = node_a.api_port().expect("node a reports its port");
    let port_b = node_b.api_port().expect("node b reports its port");
    assert_ne!(port_a, port_b, "two ephemeral nodes must not share a port");
    node_a.stop().await?;
    node_b.stop().await?;
    Ok(())
}

/// A profile with a fixed `ws_port` must be honoured exactly, and reported
/// back through `api_port` while running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_ws_port_is_honoured_and_reported() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let port = reserve_port();
    let node = start_node(local_profile(root.path(), Some(port))).await?;
    assert_eq!(node.api_port(), Some(port));
    node.stop().await
}

/// A restart with `ws_port: None` must never reuse the port from the
/// previous run's persisted `config.toml` (core's `ConfigArgs::build` would
/// otherwise merge it back in when `ws_api_port` is `None`). Occupying the
/// old port with a plain listener before the second start proves the node
/// picked a genuinely different one rather than merely getting lucky.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_with_unset_port_does_not_reuse_the_persisted_one() -> Result<(), MobileError> {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = FreenetNode::new_plain(local_profile(root.path(), None))?;
    node.start().await?;
    let port_a = node.api_port().expect("first run reports its port");
    node.stop().await?;

    // Hold the old port so a merge-back regression cannot silently reuse it.
    // The local node's own server task is torn down by aborting it (see
    // `stop_mode` in node.rs); the abort is requested, not synchronously
    // waited for, so the socket the old server task held can still be
    // closing in the background for a few scheduler ticks after `stop`
    // returns. Retry briefly rather than racing that teardown.
    let _holder = bind_loopback_retrying(port_a, Duration::from_secs(5)).await;

    node.start().await?;
    let port_b = node.api_port().expect("second run must also report a port");
    assert_ne!(
        port_b, port_a,
        "a restart with ws_port: None must not merge the persisted port back"
    );
    node.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_fail_cleanly_when_stopped() {
    init_test_logging();
    let root = tempfile::tempdir().expect("tempdir");
    let node = FreenetNode::new_plain(local_profile(root.path(), Some(7509))).expect("build node");
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
    let profile = local_profile(root.path(), Some(port));
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
    // `None` here exercises the pre-bound-listener path: network mode hands
    // the reserved ephemeral listener straight to
    // `serve_client_api_with_listener` instead of dropping and rebinding.
    let net_port = reserve_port();
    let gw_port = reserve_port();
    let gateway = unreachable_gateway(root.path(), gw_port);
    let node = FreenetNode::new_plain(network_profile(root.path(), None, net_port, gateway))?;
    release_port(net_port);
    node.start().await?;
    assert_eq!(node.status(), NodeStatus::Running);
    assert!(
        node.api_port().is_some(),
        "a running network node must report its bound client API port"
    );
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
    let node = FreenetNode::new_plain(local_profile(root.path(), Some(port)))?;
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
    // Keep thresholds just above normal measurement noise so gradual leaks are
    // detected without requiring exact process-wide resource counts.
    assert!(
        threads_after <= threads_before + 1,
        "thread count grew from {threads_before} to {threads_after} over {cycles} cycles"
    );
    assert!(
        fds_after <= fds_before + 2,
        "open fds grew from {fds_before} to {fds_after} over {cycles} cycles"
    );
    Ok(())
}

const LEAK_CHECK_CHILD_ENV: &str = "FREENET_MOBILE_LEAK_CHECK_CHILD";

/// Re-exec the test binary with an exact filter so process-wide resource
/// counts are not affected by sibling tests running in the same process.
async fn run_cycles_isolated(cycles: usize, test_name: &str) -> Result<(), MobileError> {
    if std::env::var_os(LEAK_CHECK_CHILD_ENV).is_some() {
        return run_cycles(cycles).await;
    }
    let exe = std::env::current_exe().expect("test binary path");
    let owned_name = test_name.to_string();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(exe)
            .args([
                "--exact",
                "--test-threads=1",
                "--include-ignored",
                &owned_name,
            ])
            .env(LEAK_CHECK_CHILD_ENV, "1")
            .output()
    })
    .await
    .expect("spawn_blocking join")
    .expect("re-exec the test binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "child run of {test_name} failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Fail CLOSED on a rename: libtest exits 0 when its filter matches
    // nothing, so without this the whole leak check would silently become
    // vacuous the moment either function is renamed.
    assert!(
        stdout.contains("1 passed"),
        "the child must actually have run {test_name} — if this function was \
         renamed, update the name passed to run_cycles_isolated.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twenty_five_start_stop_cycles_do_not_leak() -> Result<(), MobileError> {
    run_cycles_isolated(25, "twenty_five_start_stop_cycles_do_not_leak").await
}

/// Slow leak check; run with `cargo test -p freenet-mobile -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow: 100 start/stop cycles"]
async fn one_hundred_start_stop_cycles_do_not_leak() -> Result<(), MobileError> {
    run_cycles_isolated(100, "one_hundred_start_stop_cycles_do_not_leak").await
}

/// A `config.toml` persisted by another app container (iOS reinstall) names a
/// transport keypair path under a data dir that no longer exists on this
/// device. `freenet` reads that path eagerly while parsing the persisted
/// file, before the profile's own data dir ever gets a chance to override it,
/// so without the discard this fails at startup rather than merely naming the
/// wrong directory. The fixture is a REAL config.toml from a real previous
/// run (not a hand-truncated string), so this exercises exactly what an app
/// container move produces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_survives_a_config_naming_a_dead_key_path() -> Result<(), MobileError> {
    init_test_logging();

    // A real previous run, whose container is about to "vanish".
    let old = tempfile::tempdir().expect("old container tempdir");
    let persisted_by_old_run = persisted_config_from_a_real_run(old.path()).await?;
    drop(old); // The container is gone: its transport_keypair path is now dead.

    // The new container reuses that exact file, the way iOS would restore it
    // from a backup, under directories of its own.
    let root = tempfile::tempdir().expect("tempdir");
    let profile = local_profile(root.path(), Some(reserve_port()));
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(config_dir.join("config.toml"), &persisted_by_old_run)
        .expect("write stale config");

    let node = start_node(profile).await?;
    assert_eq!(node.status(), NodeStatus::Running);
    node.stop().await
}

/// A `config.toml` naming only a different (but still-valid) data dir must
/// still be discarded, along with any cached `gateways.toml` sitting next to
/// it — otherwise a device that switches between local and network mode
/// could get stuck on a stale `skip_load_from_network` from the wrong mode.
/// Unlike the dead-key-path case above, nothing here would make `start()`
/// fail without the discard: the profile's own data dir always overrides the
/// persisted one. The only observable effect is whether the cached gateway
/// index survives, so that is what this test checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_from_another_data_dir_is_discarded_with_its_gateways() -> Result<(), MobileError>
{
    init_test_logging();

    // A real previous run under a different, still-existing data dir.
    let old = tempfile::tempdir().expect("old container tempdir");
    let persisted_by_old_run = persisted_config_from_a_real_run(old.path()).await?;

    let root = tempfile::tempdir().expect("tempdir");
    let profile = local_profile(root.path(), Some(reserve_port()));
    let config_dir = root.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(config_dir.join("config.toml"), &persisted_by_old_run)
        .expect("write stale config");
    std::fs::write(config_dir.join("gateways.toml"), "# cached gateway index\n")
        .expect("write cached gateways.toml");

    let node = start_node(profile).await?;
    assert_eq!(node.status(), NodeStatus::Running);
    assert!(
        !config_dir.join("gateways.toml").exists(),
        "a data-dir mismatch must discard the cached gateway index too"
    );
    node.stop().await
}

/// Exercise the network-mode path with a real gateway and joiner. The mobile
/// profile is joiner-only, so the gateway is built directly through
/// `NodeConfig`; the test verifies a contract crosses the UDP connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_peers_join_and_exchange_a_contract() -> Result<(), MobileError> {
    init_test_logging();
    freenet::test_utils::ensure_contract_compiled(TEST_CONTRACT)
        .map_err(|e| MobileError::Other(e.to_string()))?;

    let gw_dir = tempfile::tempdir().expect("gw tempdir");
    let gw_ws_port = reserve_port();
    let gw_net_port = reserve_port();
    let gw_keypair_path = gw_dir.path().join("private.pem");
    let gw_pub_path = gw_dir.path().join("public.pem");
    let gw_key = freenet::dev_tool::TransportKeypair::new();
    gw_key
        .save(&gw_keypair_path)
        .map_err(|e| MobileError::Other(format!("save gateway private key: {e}")))?;
    gw_key
        .public()
        .save(&gw_pub_path)
        .map_err(|e| MobileError::Other(format!("save gateway public key: {e}")))?;

    let gw_cfg = ConfigArgs {
        ws_api: WebsocketApiArgs {
            address: Some(Ipv4Addr::LOCALHOST.into()),
            ws_api_port: Some(gw_ws_port),
            ..Default::default()
        },
        network_api: NetworkArgs {
            public_address: Some(Ipv4Addr::LOCALHOST.into()),
            public_port: Some(gw_net_port),
            address: Some(Ipv4Addr::LOCALHOST.into()),
            network_port: Some(gw_net_port),
            is_gateway: true,
            skip_load_from_network: true,
            gateways: Some(vec![]),
            location: Some(0.5),
            ignore_protocol_checking: true,
            ..Default::default()
        },
        config_paths: ConfigPathsArgs {
            config_dir: Some(gw_dir.path().to_path_buf()),
            data_dir: Some(gw_dir.path().to_path_buf()),
            log_dir: Some(gw_dir.path().to_path_buf()),
        },
        secrets: SecretArgs {
            transport_keypair: Some(gw_keypair_path.clone()),
            ..Default::default()
        },
        ..Default::default()
    }
    .build()
    .await
    .map_err(|e| MobileError::Other(format!("build gateway config: {e}")))?;
    release_port(gw_ws_port);
    release_port(gw_net_port);

    let gw_ws_api = gw_cfg.ws_api.clone();
    let gw_node = NodeConfig::new(gw_cfg)
        .await
        .map_err(|e| MobileError::Other(format!("gateway node config: {e}")))?
        .build(
            serve_client_api(gw_ws_api.clone())
                .await
                .map_err(|e| MobileError::Other(format!("gateway client api: {e}")))?,
        )
        .await
        .map_err(|e| MobileError::Other(format!("build gateway node: {e}")))?;
    let gw_shutdown = gw_node.shutdown_handle();
    let gw_run = tokio::spawn(async move { gw_node.run().await });

    // The joiner: an ordinary mobile peer, pointed at the gateway above.
    let joiner_root = tempfile::tempdir().expect("joiner tempdir");
    let gateway_entry = serde_json::json!({
        "address": format!("127.0.0.1:{gw_net_port}"),
        "public_key": gw_pub_path,
        "location": 0.5,
    })
    .to_string();
    // `start_node` only releases the ws port; a network-mode peer also binds
    // its own network port, so both reservations must be released first (the
    // same pattern `network_node_starts_and_stops_without_joining` uses).
    let joiner_ws_port = reserve_port();
    let joiner_net_port = reserve_port();
    let joiner = FreenetNode::new_plain(network_profile(
        joiner_root.path(),
        Some(joiner_ws_port),
        joiner_net_port,
        gateway_entry,
    ))?;
    release_port(joiner_ws_port);
    release_port(joiner_net_port);
    joiner.start().await?;

    let peers = joiner.wait_for_peers(1, 30).await?;
    assert!(peers >= 1, "joiner must connect to the gateway");

    // Publish on the joiner, read back through the gateway's own client API —
    // proving the contract actually crossed the wire, not just that both
    // sides answer locally.
    let (wasm, params) = test_contract();
    let state = empty_todo_list();
    let key = joiner.put(wasm, params, state.clone(), false).await?;

    let mut gw_client = connect_gw_ws(gw_ws_api.port, Duration::from_secs(15)).await?;
    let instance_id: ContractInstanceId = key
        .parse()
        .map_err(|e| MobileError::Other(format!("parse key: {e}")))?;
    gw_client
        .send(ClientRequest::ContractOp(ContractRequest::Get {
            key: instance_id,
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        }))
        .await
        .map_err(|e| MobileError::Other(format!("gateway GET send: {e}")))?;
    let got_state = recv_get_state(&mut gw_client, Duration::from_secs(30)).await?;
    assert_eq!(
        got_state, state,
        "the gateway must serve the state the joiner published"
    );

    drop(gw_client);
    joiner.stop().await?;
    gw_shutdown.shutdown().await;
    if tokio::time::timeout(Duration::from_secs(30), gw_run)
        .await
        .is_err()
    {
        tracing::info!("gateway run loop did not exit within 30s of shutdown (cleanup only)");
    }
    Ok(())
}

async fn connect_gw_ws(port: u16, within: Duration) -> Result<WebApi, MobileError> {
    let url = format!("ws://127.0.0.1:{port}/v1/contract/command?encodingProtocol=native");
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match connect_async(&url).await {
            Ok((stream, _)) => return Ok(WebApi::start(stream)),
            Err(e) if tokio::time::Instant::now() >= deadline => {
                return Err(MobileError::Timeout(format!(
                    "gateway ws api on port {port} did not come up within {within:?}: {e}"
                )));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Bind a plain loopback listener on `port`, retrying on `AddrInUse` until
/// `within` elapses. Used right after stopping a node whose server task was
/// torn down by aborting it: the abort is requested, not synchronously
/// awaited to completion, so the socket can still be closing for a few
/// scheduler ticks after `stop()` returns.
async fn bind_loopback_retrying(port: u16, within: Duration) -> std::net::TcpListener {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return listener,
            Err(e) if tokio::time::Instant::now() >= deadline => {
                panic!("hold the previous run's port within {within:?}: {e}");
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

async fn recv_get_state(client: &mut WebApi, within: Duration) -> Result<Vec<u8>, MobileError> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, client.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                state, ..
            }))) => return Ok(state.as_ref().to_vec()),
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => return Err(MobileError::Request(e.to_string())),
            Err(_) => {
                return Err(MobileError::Timeout(format!(
                    "no GetResponse within {within:?}"
                )));
            }
        }
    }
}
