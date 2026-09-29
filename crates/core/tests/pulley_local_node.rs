//! End-to-end contract round-trip through a local-mode node running on
//! wasmtime's Pulley interpreter.
//!
//! Runtime config plumbing and direct Pulley targeting are covered by engine
//! tests; this file checks the local node's execution path.
//! A round-trip alone cannot distinguish Pulley from the JIT, since both
//! backends return the same contract state.

#![cfg(feature = "pulley")]

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use freenet::config::{ConfigArgs, ConfigPathsArgs, WebsocketApiArgs};
use freenet::local_node::{Executor, OperationMode};
use freenet::test_utils::{
    create_empty_todo_list, load_contract, make_get, make_put, release_local_port,
    reserve_local_port,
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

/// Publish a contract through a local-mode node using the Pulley interpreter
/// and verify that its state round-trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contract_round_trips_on_the_interpreter() -> anyhow::Result<()> {
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
