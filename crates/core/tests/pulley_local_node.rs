//! End-to-end contract round-trip through a local-mode node running on
//! wasmtime's Pulley interpreter.
//!
//! Coverage gap from the mobile phase-1 test audit: every existing Pulley
//! test operates on a raw `Runtime`/`Engine`
//! (`wasm_runtime::tests::pulley_conformance`, the epoch and out-of-bounds
//! tests in `wasmtime_engine.rs`). `RuntimeConfig::from_node_config` copying
//! `Config::use_pulley` was only ever checked as a pure struct copy
//! (`wasm_runtime::runtime::node_config_plumbing_tests::
//! from_node_config_copies_use_pulley`) — nothing built a real `Executor`
//! from a `Config` with the switch on and ran a contract through it.
//!
//! Both backends compute byte-identical answers for this contract (that is
//! what `pulley_conformance::cranelift_and_pulley_agree_on_contract_vectors`
//! already proves), so a PUT/GET round-trip alone cannot tell a dropped
//! `use_pulley` copy from a correctly-wired one — it would pass either way.
//! `interpreter_target_is_actually_selected` closes that gap by asserting on
//! the backend directly, the same way `pulley_conformance::
//! pulley_profile_targets_the_interpreter` does for a raw engine.

#![cfg(feature = "pulley")]

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use freenet::config::{ConfigArgs, ConfigPathsArgs, WebsocketApiArgs};
use freenet::local_node::{Executor, OperationMode};
use freenet::test_utils::{
    create_empty_todo_list, ensure_contract_compiled, load_contract, make_get, make_put,
    release_local_port, reserve_local_port,
};
use freenet_stdlib::{
    client_api::{ContractResponse, HostResponse, WebApi},
    prelude::*,
};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;

const TEST_CONTRACT: &str = "test-contract-integration";

/// A local-mode `ConfigArgs` with every path explicit under `dir`, mirroring
/// `MobileProfile::config_args` (the shape a real mobile embedding builds).
fn local_config_args(dir: &std::path::Path, ws_port: u16) -> ConfigArgs {
    ConfigArgs {
        mode: Some(OperationMode::Local),
        ws_api: WebsocketApiArgs {
            address: Some(Ipv4Addr::LOCALHOST.into()),
            ws_api_port: Some(ws_port),
            ..Default::default()
        },
        config_paths: ConfigPathsArgs {
            config_dir: Some(dir.to_path_buf()),
            data_dir: Some(dir.to_path_buf()),
            log_dir: Some(dir.to_path_buf()),
        },
        ..Default::default()
    }
}

async fn connect_ws(port: u16, within: Duration) -> anyhow::Result<WebApi> {
    let url = format!("ws://127.0.0.1:{port}/v1/contract/command?encodingProtocol=native");
    let deadline = Instant::now() + within;
    loop {
        match connect_async(&url).await {
            Ok((stream, _)) => return Ok(WebApi::start(stream)),
            Err(e) if Instant::now() >= deadline => {
                anyhow::bail!("WS API on port {port} did not come up within {within:?}: {e}")
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Publish the workspace test contract on a local-mode node built with
/// `use_pulley: true`, read it back, and assert the state round-trips —
/// proving the interpreter switch reaches a real `Executor`, not just
/// `RuntimeConfig`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contract_round_trips_on_the_interpreter() -> anyhow::Result<()> {
    ensure_contract_compiled(TEST_CONTRACT)?;
    let contract = load_contract(TEST_CONTRACT, Parameters::from(Vec::<u8>::new()))?;
    let contract_key = contract.key();
    let initial_state = WrappedState::from(create_empty_todo_list());

    let dir = tempfile::tempdir()?;
    let ws_port = reserve_local_port()?;
    let mut cfg = local_config_args(dir.path(), ws_port).build().await?;
    cfg.use_pulley = true;
    let ws_api = cfg.ws_api.clone();

    let executor = Executor::from_config_local(Arc::new(cfg)).await?;
    let run = tokio::spawn(freenet::run_local_node(executor, ws_api));
    release_local_port(ws_port);

    let mut client = connect_ws(ws_port, Duration::from_secs(30)).await?;
    make_put(&mut client, initial_state.clone(), contract, false).await?;
    let put_key = loop {
        match timeout(Duration::from_secs(10), client.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key }))) => {
                break key;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => anyhow::bail!("client error awaiting PUT: {e}"),
            Err(_) => anyhow::bail!("timed out awaiting PUT response"),
        }
    };
    assert_eq!(put_key, contract_key, "PUT acknowledged a different key");

    make_get(&mut client, contract_key, false, false).await?;
    let got_state = loop {
        match timeout(Duration::from_secs(10), client.recv()).await {
            Ok(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                state, ..
            }))) => break state,
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => anyhow::bail!("client error awaiting GET: {e}"),
            Err(_) => anyhow::bail!("timed out awaiting GET response"),
        }
    };
    assert_eq!(
        got_state.as_ref(),
        initial_state.as_ref(),
        "GET returned different state than PUT installed"
    );

    drop(client);
    run.abort();
    Ok(())
}

/// Companion to the round-trip above: builds an engine straight from the same
/// kind of `Config` (`use_pulley: true`) and checks its serialized module
/// names the `pulley64` target, the way
/// `pulley_conformance::pulley_profile_targets_the_interpreter` does for a
/// raw `RuntimeConfig`. Exists because the round-trip alone cannot
/// distinguish the interpreter from the JIT: both compute the same answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interpreter_target_is_actually_selected() -> anyhow::Result<()> {
    use freenet::test_utils::engine_backend_for_config;

    let dir = tempfile::tempdir()?;
    let ws_port = reserve_local_port()?;
    let mut cfg = local_config_args(dir.path(), ws_port).build().await?;
    release_local_port(ws_port);
    cfg.use_pulley = true;

    let engine = engine_backend_for_config(&cfg)?;
    let module = wasmtime::Module::new(&engine, b"(module)").expect("trivial module compiles");
    let bytes = module.serialize().expect("module serializes");
    let mentions_pulley = bytes.windows(b"pulley64".len()).any(|w| w == b"pulley64");
    assert!(
        mentions_pulley,
        "a Config with use_pulley: true must select the pulley64 target"
    );
    Ok(())
}
