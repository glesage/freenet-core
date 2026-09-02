//! The host-facing profile and its mapping onto `freenet`'s configuration.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use freenet::config::{Config, ConfigArgs, ConfigPathsArgs, NetworkArgs, WebsocketApiArgs};
use freenet::local_node::OperationMode;

use crate::error::MobileError;

/// Which of `freenet`'s two operation modes the node runs in.
///
/// They never share stores: `freenet` keeps `db/local`, `contracts/local` and
/// friends apart from their network-mode siblings under the same data dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NodeMode {
    /// Contracts live on this device only. No transport, no ring, no gateways.
    Local,
    /// Join the network as an ordinary peer.
    Network,
}

/// Everything the host decides about a node before starting it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct MobileProfile {
    pub mode: NodeMode,
    /// Stores, contracts, secrets, event log, wasmtime cache. Must be writable
    /// by the app (on iOS: inside the app container).
    pub data_dir: String,
    /// `config.toml` and `gateways.toml`.
    pub config_dir: String,
    /// Rotated log files, when file logging is enabled.
    pub log_dir: String,
    /// Loopback port of the node's websocket client API. `freenet` tooling
    /// defaults to 7509.
    pub ws_port: u16,
    /// UDP port for peer traffic in network mode. `None` lets `freenet` pick its
    /// default (31337).
    pub network_port: Option<u16>,
    /// Gateway overrides, one JSON object per entry in the exact shape of the
    /// `freenet --gateway` flag (`{"address":"ip:port","public_key":"/path/to/gw.pub","location":0.5}`).
    /// Empty in network mode means: fetch the public gateway index. Ignored in
    /// local mode.
    pub gateways: Vec<String>,
    /// Log to stderr (the Xcode console) instead of files under `log_dir`.
    /// Only consulted by [`crate::init_logging`].
    pub log_to_stderr: bool,
}

impl MobileProfile {
    pub(crate) fn data_path(&self) -> PathBuf {
        PathBuf::from(&self.data_dir)
    }

    /// The `freenet` CLI arguments this profile stands for. Every directory is
    /// explicit and created up front.
    pub(crate) fn config_args(&self) -> Result<ConfigArgs, MobileError> {
        for dir in [&self.data_dir, &self.config_dir, &self.log_dir] {
            std::fs::create_dir_all(dir)
                .map_err(|e| MobileError::Config(format!("cannot create {dir}: {e}")))?;
        }
        let mode = match self.mode {
            NodeMode::Local => OperationMode::Local,
            NodeMode::Network => OperationMode::Network,
        };
        let explicit_gateways = !self.gateways.is_empty();
        let network_api = NetworkArgs {
            network_port: self.network_port,
            // `freenet` defaults this to true (no public index). Network mode
            // without overrides must fetch the index or it cannot join; with
            // overrides the CLI semantics are "use exactly these".
            skip_load_from_network: mode != OperationMode::Network || explicit_gateways,
            gateways: explicit_gateways.then(|| self.gateways.clone()),
            ..Default::default()
        };
        Ok(ConfigArgs {
            mode: Some(mode),
            ws_api: WebsocketApiArgs {
                address: Some(Ipv4Addr::LOCALHOST.into()),
                ws_api_port: Some(self.ws_port),
                ..Default::default()
            },
            network_api,
            config_paths: ConfigPathsArgs {
                config_dir: Some(PathBuf::from(&self.config_dir)),
                data_dir: Some(self.data_path()),
                log_dir: Some(PathBuf::from(&self.log_dir)),
            },
            ..Default::default()
        })
    }

    /// Build the node [`Config`]. This writes `config.toml` (and, in network
    /// mode with no overrides, fetches and saves `gateways.toml`).
    pub(crate) async fn build_config(&self) -> Result<Config, MobileError> {
        self.discard_relocated_config()?;
        let mut cfg = self
            .config_args()?
            .build()
            .await
            .map_err(|e| MobileError::Config(e.to_string()))?;
        // `freenet` LRU-sweeps this directory with deletes; keep it inside the
        // app's own data dir rather than its OS-cache default.
        cfg.ws_api.webapp_cache_dir = self.data_path().join("webapp_cache");
        Ok(cfg)
    }

    /// `freenet` persists the effective config, absolute paths included, and
    /// merges it back on the next build. On iOS the app container moves on
    /// reinstall (and the files inside it move along), so a persisted
    /// `config.toml` from another container points at paths that no longer
    /// exist. The profile is the source of truth here: when the persisted
    /// `data_dir` is not ours, drop `config.toml` and the cached
    /// `gateways.toml` (which names key files by absolute path too) and let
    /// `freenet` rebuild both from the profile.
    fn discard_relocated_config(&self) -> Result<(), MobileError> {
        let config_dir = Path::new(&self.config_dir);
        let config_file = config_dir.join("config.toml");
        let Ok(text) = std::fs::read_to_string(&config_file) else {
            return Ok(());
        };
        let persisted_data_dir = toml::from_str::<toml::Value>(&text)
            .ok()
            .and_then(|v| v.get("data_dir")?.as_str().map(PathBuf::from));
        match persisted_data_dir {
            Some(dir) if dir == self.data_path() => Ok(()),
            _ => {
                tracing::info!(
                    config = %config_file.display(),
                    "persisted config names another data dir; discarding it"
                );
                for stale in [config_file, config_dir.join("gateways.toml")] {
                    match std::fs::remove_file(&stale) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(MobileError::Config(format!(
                                "cannot remove stale {}: {e}",
                                stale.display()
                            )));
                        }
                    }
                }
                Ok(())
            }
        }
    }
}
