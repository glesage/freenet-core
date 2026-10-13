//! Log output for the embedded node: a daily rolling file in the host's log
//! directory.
//!
//! Installed once per process. When the app has already installed a global
//! `tracing` subscriber, the app's subscriber stays and receives the node's
//! events instead.

use std::path::Path;
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

static GUARD: OnceLock<Option<WorkerGuard>> = OnceLock::new();

pub(crate) const LOG_FILE_PREFIX: &str = "freenet-mobile";

pub(crate) fn install(log_dir: &Path, filter: Option<&str>) {
    GUARD.get_or_init(|| {
        // The appender prunes old files at build time, so the directory must
        // exist first.
        std::fs::create_dir_all(log_dir).ok()?;
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(LOG_FILE_PREFIX)
            .filename_suffix("log")
            .max_log_files(3)
            .build(log_dir)
            .ok()?;
        let (writer, guard) = tracing_appender::non_blocking(appender);
        let filter =
            EnvFilter::try_new(filter.unwrap_or("info")).unwrap_or_else(|_| EnvFilter::new("info"));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .with_target(true)
            .with_env_filter(filter)
            .finish();
        tracing::subscriber::set_global_default(subscriber).ok()?;
        Some(guard)
    });
}
