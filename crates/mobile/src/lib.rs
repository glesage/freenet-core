//! Embeddable Freenet node for mobile hosts.
//!
//! `freenet-mobile` runs the same `freenet` node code a desktop runs, inside an
//! app process, and exposes a small surface to Swift and Kotlin through UniFFI.
//! It is the phase 1 test vehicle of the Freenet mobile plan: an ordinary peer
//! that lives only while the app is in the foreground.
//!
//! # Embedding contract
//!
//! - One process-wide tokio runtime, owned by this crate, drives every node
//!   built here. The node is constructed inside it so `freenet`'s global
//!   executor adopts it instead of building its own.
//! - Paths are always explicit. The host names the data, config and log
//!   directories in [`MobileProfile`]; nothing falls back to home or temp
//!   directories (a debug build of `freenet` would otherwise pick, and delete,
//!   `$TMPDIR/freenet`).
//! - The node's client API is its loopback websocket, bound to
//!   `127.0.0.1:<ws_port>`. This crate talks to it over that socket from inside
//!   the same process, so every request travels the exact path the node's own
//!   tests exercise. Tools on the host machine (or the Mac behind an iOS
//!   simulator, which shares its loopback) can connect to the same port.
//! - No process-global handlers are installed by this crate: no signal
//!   handlers, no abort-on-fatal hooks. Stopping is an explicit [`FreenetNode::stop`].
//!   (Local mode still runs `freenet`'s own cleanup-on-exit registration, which
//!   only reacts to SIGINT/SIGTERM.)
//! - Local and network mode never share stores: `freenet` keys its directories
//!   by operation mode, and this crate does not migrate between them.

uniffi::setup_scaffolding!();

mod api;
mod client;
mod error;
mod node;
mod profile;

pub use api::{ContractUpdateListener, GetResult, NodeStatus, init_logging};
pub use error::MobileError;
pub use node::FreenetNode;
pub use profile::{MobileProfile, NodeMode};
