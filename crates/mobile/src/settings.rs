//! Node settings supplied by the host app, and the on-disk layout built from
//! them.
//!
//! The host supplies the data, config and log directories. Local mode and
//! network mode each get their own subdirectory, so their stores never mix. A
//! persisted `config.toml` whose data directory, mode or gateway source no
//! longer matches is discarded before the node reads it. iOS moves an app's
//! container on updates, and a stale absolute path in the file would stop the
//! node from starting. Core also only ever turns `skip_load_from_network` on
//! from the file, so a file written with gateway overrides would stop a later
//! start from fetching the public gateway index.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::MobileError;

/// How the embedded node reaches other peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum NodeMode {
    /// A single isolated node with no network traffic: its own gateway, with
    /// no peers. For fixtures and offline tests.
    Local,
    /// A peer that joins the network through gateways.
    Network,
}

impl NodeMode {
    pub(crate) fn dir_name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Network => "network",
        }
    }
}

/// The Wasm backend, mirrored from Core's `WasmBackend` for the bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum WasmBackendChoice {
    Cranelift,
    Pulley,
}

impl From<WasmBackendChoice> for freenet::config::WasmBackend {
    fn from(choice: WasmBackendChoice) -> Self {
        match choice {
            WasmBackendChoice::Cranelift => Self::Cranelift,
            WasmBackendChoice::Pulley => Self::Pulley,
        }
    }
}

impl From<freenet::config::WasmBackend> for WasmBackendChoice {
    fn from(backend: freenet::config::WasmBackend) -> Self {
        match backend {
            freenet::config::WasmBackend::Cranelift => Self::Cranelift,
            freenet::config::WasmBackend::Pulley => Self::Pulley,
        }
    }
}

/// A gateway to join through, in place of the public gateway index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct GatewayOverride {
    /// `ip:port` of the gateway's UDP listener. An IP literal, never a
    /// hostname, so starting offline needs no DNS.
    pub address: String,
    /// The gateway's 32-byte X25519 transport public key, as 64 hex digits.
    pub public_key_hex: String,
}

impl GatewayOverride {
    /// The same gateway in Core's `--gateway` form: `ip:port,hex-public-key`.
    pub fn to_core_arg(&self) -> Result<String, MobileError> {
        let address: SocketAddr =
            self.address
                .parse()
                .map_err(|e| MobileError::InvalidSettings {
                    reason: format!("gateway address {:?} is not ip:port: {e}", self.address),
                })?;
        let key = self.public_key_hex.trim();
        let valid_key = key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid_key {
            return Err(MobileError::InvalidSettings {
                reason: format!("gateway key for {address} must be 64 hex digits"),
            });
        }
        Ok(format!("{address},{}", key.to_ascii_lowercase()))
    }
}

/// Everything the host decides about the node. Paths must be absolute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
pub struct NodeSettings {
    /// Contract, delegate and secret stores live below this directory.
    pub data_dir: String,
    /// The node's persisted `config.toml` lives below this directory.
    pub config_dir: String,
    /// Log files go here.
    pub log_dir: String,
    /// Unpacked web apps go here. Defaults to a folder in `data_dir`.
    #[uniffi(default = None)]
    pub cache_dir: Option<String>,
    pub mode: NodeMode,
    /// Try this loopback port first for the client API, so web storage keeps
    /// the same origin across launches. Any free port is used when it is taken.
    #[uniffi(default = None)]
    pub preferred_ws_port: Option<u16>,
    /// UDP port for peer traffic. Any free port when unset.
    #[uniffi(default = None)]
    pub network_port: Option<u16>,
    /// Gateways to join through. Empty in network mode means the public
    /// gateway index.
    #[uniffi(default = [])]
    pub gateways: Vec<GatewayOverride>,
    /// Wasm backend. The target's default when unset.
    #[uniffi(default = None)]
    pub wasm_backend: Option<WasmBackendChoice>,
    /// A `tracing` filter such as `info` or `info,freenet=debug`.
    #[uniffi(default = None)]
    pub log_filter: Option<String>,
    /// Byte budget for compiled contract modules. 64 MiB when unset.
    #[uniffi(default = None)]
    pub module_cache_budget_bytes: Option<u64>,
    /// Byte budget for hosted contract state. 128 MiB when unset.
    #[uniffi(default = None)]
    pub max_hosting_storage_bytes: Option<u64>,
    /// Disk budget for everything the node stores. 1 GiB when unset.
    #[uniffi(default = None)]
    pub max_hosting_disk_bytes: Option<u64>,
}

pub(crate) const DEFAULT_MODULE_CACHE_BUDGET: u64 = 64 * 1024 * 1024;
pub(crate) const DEFAULT_HOSTING_STORAGE: u64 = 128 * 1024 * 1024;
pub(crate) const DEFAULT_HOSTING_DISK: u64 = 1024 * 1024 * 1024;
/// Seconds shutdown waits for in-flight client operations. iOS gives an app
/// about five seconds after it moves to the background.
pub(crate) const SHUTDOWN_DRAIN_SECS: u64 = 2;

impl NodeSettings {
    pub fn wasm_backend(&self) -> freenet::config::WasmBackend {
        self.wasm_backend
            .map(Into::into)
            .unwrap_or_else(freenet::config::WasmBackend::default_for_target)
    }

    pub(crate) fn validate(&self) -> Result<(), MobileError> {
        for (name, dir) in [
            ("data_dir", Some(&self.data_dir)),
            ("config_dir", Some(&self.config_dir)),
            ("log_dir", Some(&self.log_dir)),
            ("cache_dir", self.cache_dir.as_ref()),
        ] {
            if let Some(dir) = dir {
                if !Path::new(dir).is_absolute() {
                    return Err(MobileError::InvalidSettings {
                        reason: format!("{name} must be an absolute path, got {dir:?}"),
                    });
                }
            }
        }
        for gateway in &self.gateways {
            gateway.to_core_arg()?;
        }
        if self.mode == NodeMode::Local && !self.gateways.is_empty() {
            return Err(MobileError::InvalidSettings {
                reason: "local mode runs without gateways".into(),
            });
        }
        let backend = self.wasm_backend();
        if !backend.is_available() {
            return Err(MobileError::BackendUnavailable {
                backend: backend.to_string(),
            });
        }
        Ok(())
    }
}

/// The directories one mode uses.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub data: PathBuf,
    pub config: PathBuf,
    pub logs: PathBuf,
    pub webapp_cache: PathBuf,
    /// A persisted config was found stale and removed before this start.
    pub discarded_config: bool,
}

const MARKER_FILE: &str = "freenet-mobile.json";

/// What the node last started with, written next to `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LayoutMarker {
    format: u32,
    data_dir: PathBuf,
    mode: NodeMode,
    /// `local`, `index` for the public gateway index, or the override list.
    #[serde(default)]
    gateway_source: String,
}

fn gateway_source(settings: &NodeSettings) -> String {
    match settings.mode {
        NodeMode::Local => "local".into(),
        NodeMode::Network if settings.gateways.is_empty() => "index".into(),
        NodeMode::Network => {
            let mut entries: Vec<String> = settings
                .gateways
                .iter()
                .map(|g| format!("{},{}", g.address, g.public_key_hex.to_ascii_lowercase()))
                .collect();
            entries.sort();
            format!("overrides:{}", entries.join(";"))
        }
    }
}

impl Layout {
    pub(crate) fn prepare(settings: &NodeSettings) -> Result<Self, MobileError> {
        settings.validate()?;
        let mode_dir = settings.mode.dir_name();
        let data = Path::new(&settings.data_dir).join(mode_dir);
        let config = Path::new(&settings.config_dir).join(mode_dir);
        let logs = PathBuf::from(&settings.log_dir);
        let webapp_cache = settings
            .cache_dir
            .as_ref()
            .map(|dir| Path::new(dir).join(mode_dir))
            .unwrap_or_else(|| data.join("cache"))
            .join("webapp_cache");
        for dir in [&data, &config, &logs, &webapp_cache] {
            std::fs::create_dir_all(dir).map_err(|e| {
                MobileError::storage(format!("cannot create {}: {e}", dir.display()))
            })?;
        }
        let expected = LayoutMarker {
            format: 1,
            data_dir: data.clone(),
            mode: settings.mode,
            gateway_source: gateway_source(settings),
        };
        let discarded_config = discard_stale_config(&config, &expected)?;
        let marker = serde_json::to_vec_pretty(&expected).map_err(MobileError::internal)?;
        std::fs::write(config.join(MARKER_FILE), marker).map_err(MobileError::storage)?;
        Ok(Self {
            data,
            config,
            logs,
            webapp_cache,
            discarded_config,
        })
    }
}

/// Remove persisted node config written for another data directory or mode.
/// Returns whether anything was removed.
fn discard_stale_config(config_dir: &Path, expected: &LayoutMarker) -> Result<bool, MobileError> {
    let persisted = persisted_config_files(config_dir)?;
    if persisted.is_empty() {
        return Ok(false);
    }
    let marker_matches = std::fs::read(config_dir.join(MARKER_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LayoutMarker>(&bytes).ok())
        .is_some_and(|marker| &marker == expected);
    let file_matches = persisted
        .iter()
        .all(|file| persisted_data_dir(file).is_none_or(|dir| dir == expected.data_dir));
    if marker_matches && file_matches {
        return Ok(false);
    }
    for file in &persisted {
        std::fs::remove_file(file)
            .map_err(|e| MobileError::storage(format!("cannot remove {}: {e}", file.display())))?;
    }
    tracing::info!(
        config_dir = %config_dir.display(),
        "discarded a persisted node config written for another data directory or mode"
    );
    Ok(true)
}

/// Core reads `config.toml`, `config.json`, or else the first `config*` file
/// it finds, so all of them count.
fn persisted_config_files(config_dir: &Path) -> Result<Vec<PathBuf>, MobileError> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(config_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(e) => return Err(MobileError::storage(e)),
    };
    for entry in entries {
        let entry = entry.map_err(MobileError::storage)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_config =
            name.starts_with("config") && (name.ends_with(".toml") || name.ends_with(".json"));
        if is_config && entry.path().is_file() {
            files.push(entry.path());
        }
    }
    Ok(files)
}

fn persisted_data_dir(file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let value: serde_json::Value = if file.extension().is_some_and(|ext| ext == "json") {
        serde_json::from_str(&text).ok()?
    } else {
        let table: toml::Table = toml::from_str(&text).ok()?;
        serde_json::to_value(table).ok()?
    };
    value
        .get("data_dir")
        .or_else(|| value.get("data-dir"))
        .and_then(|dir| dir.as_str())
        .map(PathBuf::from)
}

/// Bind the client API's loopback listener: the preferred port when it is
/// free, else any free port.
pub(crate) fn bind_loopback(preferred: Option<u16>) -> Result<std::net::TcpListener, MobileError> {
    if let Some(port) = preferred.filter(|port| *port != 0) {
        match std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => return Ok(listener),
            Err(e) => tracing::info!(
                port,
                "preferred client API port is taken ({e}); using a free one"
            ),
        }
    }
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| MobileError::StartFailed {
        reason: format!("cannot bind a loopback port for the client API: {e}"),
    })
}

/// A UDP port that is free right now.
pub(crate) fn free_udp_port(address: Ipv4Addr) -> Result<u16, MobileError> {
    let socket = UdpSocket::bind((address, 0)).map_err(|e| MobileError::StartFailed {
        reason: format!("cannot find a free UDP port: {e}"),
    })?;
    socket
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| MobileError::StartFailed {
            reason: e.to_string(),
        })
}

/// Core's arguments for this start. Everything is explicit, so a persisted
/// `config.toml` can only fill in fields this function leaves unset.
pub(crate) fn config_args(
    settings: &NodeSettings,
    layout: &Layout,
    ws_port: u16,
) -> Result<freenet::config::ConfigArgs, MobileError> {
    use freenet::config::{ConfigArgs, ConfigPathsArgs, NetworkArgs, WebsocketApiArgs};
    use freenet::local_node::OperationMode;

    let mut args = ConfigArgs {
        // Local mode is an isolated gateway node, so it runs Core's network
        // mode. Core's own local mode installs a Ctrl-C handler and has no
        // shutdown handle, and the embedded node needs neither.
        mode: Some(OperationMode::Network),
        ws_api: WebsocketApiArgs {
            address: Some(Ipv4Addr::LOCALHOST.into()),
            ws_api_port: Some(ws_port),
            ..Default::default()
        },
        config_paths: ConfigPathsArgs {
            config_dir: Some(layout.config.clone()),
            data_dir: Some(layout.data.clone()),
            log_dir: Some(layout.logs.clone()),
        },
        log_level: Some(tracing::log::LevelFilter::Info),
        module_cache_budget_bytes: Some(
            settings
                .module_cache_budget_bytes
                .unwrap_or(DEFAULT_MODULE_CACHE_BUDGET) as usize,
        ),
        max_hosting_storage: Some(
            settings
                .max_hosting_storage_bytes
                .unwrap_or(DEFAULT_HOSTING_STORAGE),
        ),
        max_hosting_disk: Some(
            settings
                .max_hosting_disk_bytes
                .unwrap_or(DEFAULT_HOSTING_DISK),
        ),
        wasm_backend: Some(settings.wasm_backend()),
        shutdown_drain_secs: Some(SHUTDOWN_DRAIN_SECS),
        enable_event_log: Some(false),
        disable_auto_update: true,
        ..Default::default()
    };
    // No usage reports leave the phone.
    args.telemetry.enabled = false;

    args.network_api = match settings.mode {
        NodeMode::Local => {
            let port = match settings.network_port {
                Some(port) => port,
                None => free_udp_port(Ipv4Addr::LOCALHOST)?,
            };
            NetworkArgs {
                address: Some(Ipv4Addr::LOCALHOST.into()),
                network_port: Some(port),
                public_address: Some(Ipv4Addr::LOCALHOST.into()),
                public_port: Some(port),
                is_gateway: true,
                skip_load_from_network: true,
                gateways: Some(vec![]),
                location: Some(0.5),
                ignore_protocol_checking: true,
                ..Default::default()
            }
        }
        NodeMode::Network => {
            let gateways = settings
                .gateways
                .iter()
                .map(GatewayOverride::to_core_arg)
                .collect::<Result<Vec<_>, _>>()?;
            let use_index = gateways.is_empty();
            NetworkArgs {
                network_port: settings.network_port,
                is_gateway: false,
                // With overrides Core uses exactly those gateways; without, it
                // fetches the public gateway index.
                skip_load_from_network: !use_index,
                gateway: (!use_index).then_some(gateways),
                ..Default::default()
            }
        }
    };
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn modes_get_separate_stores() {
        let root = tempfile::tempdir().unwrap();
        let local = Layout::prepare(&settings(root.path(), NodeMode::Local)).unwrap();
        let network = Layout::prepare(&settings(root.path(), NodeMode::Network)).unwrap();
        assert_ne!(local.data, network.data);
        assert_ne!(local.config, network.config);
        assert!(!local.data.starts_with(&network.data));
        assert!(!network.data.starts_with(&local.data));
    }

    #[test]
    fn a_config_for_another_data_dir_is_discarded() {
        let root = tempfile::tempdir().unwrap();
        let s = settings(root.path(), NodeMode::Network);
        let layout = Layout::prepare(&s).unwrap();
        assert!(!layout.discarded_config);

        // A config written while the app container lived elsewhere.
        std::fs::write(
            layout.config.join("config.toml"),
            "mode = \"network\"\ndata_dir = \"/private/var/old-container/data/network\"\n",
        )
        .unwrap();
        let again = Layout::prepare(&s).unwrap();
        assert!(again.discarded_config);
        assert!(!again.config.join("config.toml").exists());

        // A matching config stays.
        std::fs::write(
            again.config.join("config.toml"),
            format!("data_dir = {:?}\n", again.data.display().to_string()),
        )
        .unwrap();
        let third = Layout::prepare(&s).unwrap();
        assert!(!third.discarded_config);
        assert!(third.config.join("config.toml").exists());
    }

    #[test]
    fn a_config_from_the_other_mode_is_discarded() {
        let root = tempfile::tempdir().unwrap();
        let s = settings(root.path(), NodeMode::Local);
        let layout = Layout::prepare(&s).unwrap();
        std::fs::write(
            layout.config.join("config.toml"),
            format!("data_dir = {:?}\n", layout.data.display().to_string()),
        )
        .unwrap();
        // Rewrite the marker as if the network mode had used this directory.
        let marker = LayoutMarker {
            format: 1,
            data_dir: layout.data.clone(),
            mode: NodeMode::Network,
            gateway_source: "index".into(),
        };
        std::fs::write(
            layout.config.join(MARKER_FILE),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        assert!(Layout::prepare(&s).unwrap().discarded_config);
    }

    #[test]
    fn a_config_written_with_other_gateways_is_discarded() {
        let root = tempfile::tempdir().unwrap();
        let mut s = settings(root.path(), NodeMode::Network);
        s.gateways = vec![GatewayOverride {
            address: "127.0.0.1:31337".into(),
            public_key_hex: "ab".repeat(32),
        }];
        let layout = Layout::prepare(&s).unwrap();
        std::fs::write(
            layout.config.join("config.toml"),
            format!(
                "data_dir = {:?}\nskip_load_from_network = true\n",
                layout.data.display().to_string()
            ),
        )
        .unwrap();
        // Same gateways: kept.
        assert!(!Layout::prepare(&s).unwrap().discarded_config);
        // Back to the public index: the sticky flag must go.
        s.gateways.clear();
        let index = Layout::prepare(&s).unwrap();
        assert!(index.discarded_config);
        assert!(!index.config.join("config.toml").exists());
    }

    #[test]
    fn gateway_overrides_use_core_gateway_form() {
        let key = "ab".repeat(32);
        let gateway = GatewayOverride {
            address: "127.0.0.1:31337".into(),
            public_key_hex: key.to_uppercase(),
        };
        assert_eq!(
            gateway.to_core_arg().unwrap(),
            format!("127.0.0.1:31337,{key}")
        );
        let hostname = GatewayOverride {
            address: "gw.example:31337".into(),
            public_key_hex: key,
        };
        assert!(hostname.to_core_arg().is_err());
    }

    #[test]
    fn preferred_port_is_used_when_free_and_skipped_when_taken() {
        let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let taken_port = taken.local_addr().unwrap().port();
        let listener = bind_loopback(Some(taken_port)).unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), taken_port);
        let free_port = listener.local_addr().unwrap().port();
        drop(listener);
        let again = bind_loopback(Some(free_port)).unwrap();
        assert_eq!(again.local_addr().unwrap().port(), free_port);
    }

    #[test]
    fn relative_paths_are_rejected() {
        let mut s = settings(Path::new("/tmp"), NodeMode::Local);
        s.data_dir = "data".into();
        assert!(matches!(
            s.validate(),
            Err(MobileError::InvalidSettings { .. })
        ));
    }
}
