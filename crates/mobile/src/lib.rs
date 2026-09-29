//! Embedded Freenet node for iOS and Android apps.
//!
//! One process-wide async runtime owns the node. The host app supplies the
//! data, config and log directories, starts and stops the node explicitly, and
//! reaches it through the loopback WebSocket client API on a port the node
//! picks at start. UniFFI generates the Swift and Kotlin bindings.
//!
//! | Module | What it holds |
//! | --- | --- |
//! | [`node`] | Start, stop, status, peer count and events |
//! | [`settings`] | Host-supplied settings and the per-mode store layout |
//! | [`client`] | Contract operations for custom Swift and Kotlin screens |
//! | [`bundle`] | Host-served web bundles checked against a SHA-256 manifest |
//! | [`bridge`] | The WebView JSON bridge, handled the same on both platforms |
//! | [`conformance`] | The Wasm backend suite, run on the device |
//! | [`fixtures`] | Shared protocol fixtures compared with desktop |
//! | [`metrics`] | Memory, CPU and storage measurements |

uniffi::setup_scaffolding!();

pub mod bridge;
pub mod bundle;
pub mod client;
pub mod conformance;
pub mod error;
pub mod fixtures;
pub mod json;
mod logging;
pub mod metrics;
pub mod node;
mod runtime;
pub mod settings;

pub use error::{ClientErrorKind, MobileError};
pub use node::{MobileNode, NodeEvent, NodeEventListener, NodeInfo, NodeState, NodeStatus};
pub use settings::{GatewayOverride, NodeMode, NodeSettings, WasmBackendChoice};

/// The freenet-stdlib version this build links, from `Cargo.lock`.
pub const STDLIB_VERSION: &str = env!("FREENET_MOBILE_STDLIB_VERSION");

/// Exactly what this library was built from, for every report.
#[derive(Debug, Clone, PartialEq, serde::Serialize, uniffi::Record)]
pub struct BuildInfo {
    pub mobile_version: String,
    pub core_version: String,
    pub core_revision: String,
    pub stdlib_version: String,
    pub wasmtime_version: String,
    pub uniffi_version: String,
    pub target: String,
    pub profile: String,
    pub default_backend: WasmBackendChoice,
    pub available_backends: Vec<WasmBackendChoice>,
}

#[uniffi::export]
pub fn build_info() -> BuildInfo {
    use freenet::config::WasmBackend;
    BuildInfo {
        mobile_version: env!("CARGO_PKG_VERSION").into(),
        core_version: freenet_core_version(),
        core_revision: env!("FREENET_MOBILE_CORE_REVISION").into(),
        stdlib_version: STDLIB_VERSION.into(),
        wasmtime_version: env!("FREENET_MOBILE_WASMTIME_VERSION").into(),
        uniffi_version: env!("FREENET_MOBILE_UNIFFI_VERSION").into(),
        target: env!("FREENET_MOBILE_TARGET").into(),
        profile: env!("FREENET_MOBILE_PROFILE").into(),
        default_backend: WasmBackend::default_for_target().into(),
        available_backends: WasmBackend::ALL
            .into_iter()
            .filter(|b| b.is_available())
            .map(Into::into)
            .collect(),
    }
}

fn freenet_core_version() -> String {
    // The mobile crate is versioned apart from Core; Core's version is the
    // workspace's `freenet` package version.
    const CORE_MANIFEST: &str = include_str!("../../core/Cargo.toml");
    CORE_MANIFEST
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or("unknown")
        .to_owned()
}
