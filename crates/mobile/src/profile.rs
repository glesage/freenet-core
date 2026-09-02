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
}

impl MobileProfile {
    fn data_path(&self) -> PathBuf {
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

    /// `freenet` persists the effective config and merges parts of it back on
    /// the next build: absolute paths, and `skip_load_from_network` whenever
    /// the file says `true`. Two mobile realities break that: the app container
    /// moves on reinstall (files included), so persisted paths point nowhere,
    /// and one app switches between local and network mode, so a local run's
    /// `skip_load_from_network = true` would stop the next network run from
    /// fetching the public gateway index. The profile is the source of truth
    /// here: when the persisted file is stale for this profile, drop it and the
    /// cached `gateways.toml` (absolute key paths too) and let `freenet`
    /// rebuild both.
    fn discard_relocated_config(&self) -> Result<(), MobileError> {
        let config_dir = Path::new(&self.config_dir);
        let config_file = config_dir.join("config.toml");
        let Ok(text) = std::fs::read_to_string(&config_file) else {
            return Ok(());
        };
        if !self.persisted_config_is_stale(&text) {
            return Ok(());
        }
        tracing::info!(
            config = %config_file.display(),
            "persisted config is stale for this profile; discarding it"
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

    /// Whether a persisted `config.toml` would misconfigure a node built from
    /// this profile: another data dir, another mode, or a persisted
    /// `skip_load_from_network = true` when this profile needs the public
    /// gateway index. Unparseable files count as stale.
    fn persisted_config_is_stale(&self, text: &str) -> bool {
        let Ok(value) = toml::from_str::<toml::Value>(text) else {
            return true;
        };
        let other_data_dir = value
            .get("data_dir")
            .and_then(|v| v.as_str())
            .is_none_or(|dir| Path::new(dir) != self.data_path());
        let wanted_mode = match self.mode {
            NodeMode::Local => "local",
            NodeMode::Network => "network",
        };
        let other_mode = value
            .get("mode")
            .and_then(|v| v.as_str())
            .is_some_and(|mode| !mode.eq_ignore_ascii_case(wanted_mode));
        let wants_public_index = self.mode == NodeMode::Network && self.gateways.is_empty();
        let blocks_public_index = wants_public_index
            && value
                .get("skip_load_from_network")
                .and_then(|v| v.as_bool())
                .is_some_and(|skip| skip);
        other_data_dir || other_mode || blocks_public_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(mode: NodeMode, gateways: Vec<String>) -> MobileProfile {
        MobileProfile {
            mode,
            data_dir: "/app/freenet/data".into(),
            config_dir: "/app/freenet/config".into(),
            log_dir: "/app/freenet/logs".into(),
            ws_port: 7509,
            network_port: None,
            gateways,
        }
    }

    #[test]
    fn matching_persisted_config_is_kept() {
        let p = profile(NodeMode::Local, vec![]);
        let text =
            "mode = \"local\"\ndata_dir = \"/app/freenet/data\"\nskip_load_from_network = true\n";
        assert!(!p.persisted_config_is_stale(text));
    }

    #[test]
    fn another_data_dir_is_stale() {
        let p = profile(NodeMode::Local, vec![]);
        let text = "mode = \"local\"\ndata_dir = \"/old-container/freenet/data\"\n";
        assert!(p.persisted_config_is_stale(text));
    }

    #[test]
    fn another_mode_is_stale() {
        let p = profile(NodeMode::Network, vec!["{}".into()]);
        let text = "mode = \"local\"\ndata_dir = \"/app/freenet/data\"\n";
        assert!(p.persisted_config_is_stale(text));
    }

    #[test]
    fn persisted_skip_blocks_public_index_and_is_stale() {
        let p = profile(NodeMode::Network, vec![]);
        let text =
            "mode = \"network\"\ndata_dir = \"/app/freenet/data\"\nskip_load_from_network = true\n";
        assert!(p.persisted_config_is_stale(text));
        // With explicit gateways the profile wants the skip itself: not stale.
        let p = profile(NodeMode::Network, vec!["{}".into()]);
        assert!(!p.persisted_config_is_stale(text));
    }

    #[test]
    fn unparseable_persisted_config_is_stale() {
        assert!(profile(NodeMode::Local, vec![]).persisted_config_is_stale("not toml ["));
    }
}
