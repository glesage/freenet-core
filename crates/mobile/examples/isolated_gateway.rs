//! An isolated test network on this computer: one gateway that phones and
//! simulators join through a gateway override, so test writes never reach the
//! public network.
//!
//! ```text
//! cargo run -p freenet-mobile --release --example isolated_gateway -- \
//!     --dir <state dir> [--bind 127.0.0.1] [--udp-port 31337] [--ws-port 7600] \
//!     [--load <prefix>]... [--info <out.json>]
//! ```
//!
//! `--load <prefix>` stores a contract from `<prefix>.code.wasm`,
//! `<prefix>.params` and `<prefix>.state`, as `fetch_contract` writes them.
//! The gateway keeps its transport key in the state dir, so its public key
//! stays the same across runs. Use `--bind` with this computer's LAN address to
//! let a phone on the same network join.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use freenet::config::{ConfigArgs, ConfigPathsArgs, NetworkArgs, SecretArgs, WebsocketApiArgs};
use freenet::dev_tool::TransportKeypair;
use freenet::local_node::{NodeConfig, OperationMode};
use freenet::server::serve_client_api_with_listener;
use serde_json::json;

struct Args {
    dir: PathBuf,
    bind: IpAddr,
    udp_port: u16,
    ws_port: u16,
    load: Vec<PathBuf>,
    info: Option<PathBuf>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        dir: PathBuf::new(),
        bind: Ipv4Addr::LOCALHOST.into(),
        udp_port: 31337,
        ws_port: 7600,
        load: vec![],
        info: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--dir" => args.dir = PathBuf::from(value()?),
            "--bind" => args.bind = value()?.parse()?,
            "--udp-port" => args.udp_port = value()?.parse()?,
            "--ws-port" => args.ws_port = value()?.parse()?,
            "--load" => args.load.push(PathBuf::from(value()?)),
            "--info" => args.info = Some(PathBuf::from(value()?)),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    anyhow::ensure!(!args.dir.as_os_str().is_empty(), "--dir is required");
    Ok(args)
}

fn with_suffix(prefix: &Path, suffix: &str) -> PathBuf {
    let mut name = prefix.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let dir = std::fs::canonicalize({
        std::fs::create_dir_all(&args.dir)?;
        &args.dir
    })?;
    for sub in ["config", "data", "logs"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    let key_path = dir.join("gateway-transport.pem");
    let key = if key_path.exists() {
        TransportKeypair::load(&key_path)?
    } else {
        let key = TransportKeypair::new();
        key.save(&key_path)?;
        key
    };
    let public_key_hex = hex::encode(key.public().as_bytes());

    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, args.ws_port))?;
    listener.set_nonblocking(true)?;
    let mut config_args = ConfigArgs {
        mode: Some(OperationMode::Network),
        ws_api: WebsocketApiArgs {
            address: Some(Ipv4Addr::LOCALHOST.into()),
            ws_api_port: Some(args.ws_port),
            ..Default::default()
        },
        network_api: NetworkArgs {
            address: Some(args.bind),
            network_port: Some(args.udp_port),
            public_address: Some(args.bind),
            public_port: Some(args.udp_port),
            is_gateway: true,
            skip_load_from_network: true,
            gateways: Some(vec![]),
            location: Some(0.5),
            ignore_protocol_checking: true,
            ..Default::default()
        },
        config_paths: ConfigPathsArgs {
            config_dir: Some(dir.join("config")),
            data_dir: Some(dir.join("data")),
            log_dir: Some(dir.join("logs")),
        },
        secrets: SecretArgs {
            transport_keypair: Some(key_path.clone()),
            ..Default::default()
        },
        disable_auto_update: true,
        ..Default::default()
    };
    config_args.telemetry.enabled = false;
    let mut config = config_args.build().await?;
    config.ws_api.port = args.ws_port;
    let clients = serve_client_api_with_listener(config.ws_api.clone(), listener).await?;
    let node = NodeConfig::new(config).await?.build(clients).await?;
    let shutdown = node.shutdown_handle();
    let run = tokio::spawn(async move { node.run().await });

    // Wait for the client API, then store the requested contracts.
    let client = loop {
        match freenet_mobile::client::connect_client(args.ws_port).await {
            Ok(client) => break client,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    let mut loaded = Vec::new();
    for prefix in &args.load {
        let code = std::fs::read(with_suffix(prefix, ".code.wasm"))?;
        let params = std::fs::read(with_suffix(prefix, ".params"))?;
        let state = std::fs::read(with_suffix(prefix, ".state"))?;
        let put = client
            .put(code, params, state, false, Some(120_000))
            .await
            .map_err(|e| anyhow::anyhow!("storing {}: {e}", prefix.display()))?;
        println!(
            "stored {} as {}",
            prefix.display(),
            put.contract.instance_id
        );
        loaded.push(json!({
            "prefix": prefix.display().to_string(),
            "instance_id": put.contract.instance_id,
            "code_hash": put.contract.code_hash,
        }));
    }

    let udp_address = format!("{}:{}", args.bind, args.udp_port);
    let info = json!({
        "udp_address": udp_address,
        "public_key_hex": public_key_hex,
        "ws_port": args.ws_port,
        "contracts": loaded,
    });
    let text = serde_json::to_string_pretty(&info)?;
    if let Some(path) = &args.info {
        std::fs::write(path, format!("{text}\n"))?;
    }
    println!("{text}");
    println!("gateway running; Ctrl-C stops it");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        result = run => {
            anyhow::bail!("the gateway exited: {result:?}");
        }
    }
    shutdown.shutdown().await;
    Ok(())
}
