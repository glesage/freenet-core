//! Read a contract from the public network and save its code, parameters and
//! state, for example River's website container.
//!
//! ```text
//! cargo run -p freenet-mobile --release --example fetch_contract -- \
//!     <instance id> <out prefix> [--dir <state dir>]
//! ```
//!
//! Writes `<prefix>.code.wasm`, `<prefix>.params`, `<prefix>.state` and
//! `<prefix>.json`. The run joins the network as an ordinary peer through the
//! public gateway index, reads the contract, and stops. It writes nothing to
//! the network. Timings go to `<prefix>.json`: they are desktop baselines for
//! the phone's startup and first-read measurements.

use std::path::PathBuf;
use std::time::Instant;

use freenet_mobile::client::connect_client;
use freenet_mobile::{MobileNode, NodeMode, NodeSettings};
use futures::executor::block_on;
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let id = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: fetch_contract <id> <prefix>"))?;
    let prefix = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("missing <prefix>"))?,
    );
    let mut dir = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--dir" => dir = args.next().map(PathBuf::from),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    let scratch = tempfile::tempdir()?;
    let root = dir.unwrap_or_else(|| scratch.path().to_path_buf());
    std::fs::create_dir_all(&root)?;
    let root = std::fs::canonicalize(root)?;

    let started = Instant::now();
    let node = MobileNode::new(NodeSettings {
        data_dir: root.join("data").display().to_string(),
        config_dir: root.join("config").display().to_string(),
        log_dir: root.join("logs").display().to_string(),
        cache_dir: None,
        mode: NodeMode::Network,
        preferred_ws_port: None,
        network_port: None,
        gateways: vec![],
        wasm_backend: None,
        log_filter: None,
        module_cache_budget_bytes: None,
        max_hosting_storage_bytes: None,
        max_hosting_disk_bytes: None,
    })?;
    let info = block_on(node.clone().start())?;
    let node_ready_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "node running on port {} after {node_ready_ms:.0} ms",
        info.ws_port
    );
    let peers = block_on(node.clone().wait_for_peers(1, 120_000))?;
    let first_peer_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!("{peers} peer(s) after {first_peer_ms:.0} ms");

    let client = block_on(connect_client(info.ws_port))?;
    let read_started = Instant::now();
    let mut attempt = 0;
    let got = loop {
        attempt += 1;
        match block_on(client.get(id.clone(), true, false, Some(60_000))) {
            Ok(got) => break got,
            Err(e) if attempt < 5 => {
                println!("read attempt {attempt} failed: {e}; retrying");
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
            Err(e) => {
                let _ = block_on(node.clone().stop());
                anyhow::bail!("reading {id} failed: {e}");
            }
        }
    };
    let read_ms = read_started.elapsed().as_secs_f64() * 1000.0;
    let code = got
        .code
        .clone()
        .ok_or_else(|| anyhow::anyhow!("the node returned no contract code"))?;
    let params = got
        .params
        .clone()
        .ok_or_else(|| anyhow::anyhow!("the node returned no contract parameters"))?;
    let with = |suffix: &str| {
        let mut name = prefix.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    std::fs::write(with(".code.wasm"), &code)?;
    std::fs::write(with(".params"), &params)?;
    std::fs::write(with(".state"), &got.state)?;
    let summary = json!({
        "instance_id": got.contract.instance_id,
        "code_hash": got.contract.code_hash,
        "state_bytes": got.state.len(),
        "code_bytes": code.len(),
        "node_ready_ms": node_ready_ms,
        "first_peer_ms": first_peer_ms,
        "read_ms": read_ms,
        "read_attempts": attempt,
        "build": freenet_mobile::build_info(),
    });
    std::fs::write(
        with(".json"),
        format!("{}\n", serde_json::to_string_pretty(&summary)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    block_on(node.clone().stop())?;
    Ok(())
}
