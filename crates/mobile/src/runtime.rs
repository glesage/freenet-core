//! The one async runtime for the whole process.
//!
//! The node, its client API server and every SDK task run here. Core's
//! `GlobalExecutor` spawns onto the current runtime, so building the node from
//! inside this runtime keeps all of it in one place. Nothing here installs a
//! signal, panic or abort handler: the app owns those, and stopping the node is
//! an explicit call.
//!
//! Exported async functions are polled by the Swift or Kotlin executor, not by
//! this runtime, so they hand their work to [`run`] and await the result.

use std::future::Future;
use std::sync::OnceLock;

use crate::error::MobileError;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Worker threads: enough for the node, the client API and Wasm offload, and no
/// more than a phone's performance cores.
fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 4))
        .unwrap_or(2)
}

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads())
            .max_blocking_threads(16)
            .thread_name("freenet-mobile")
            .enable_all()
            .build()
            .expect("the freenet-mobile runtime could not start")
    })
}

/// Run `future` on the process runtime and wait for it from any executor.
pub(crate) async fn run<F, T>(future: F) -> Result<T, MobileError>
where
    F: Future<Output = Result<T, MobileError>> + Send + 'static,
    T: Send + 'static,
{
    runtime()
        .spawn(future)
        .await
        .map_err(|e| MobileError::internal(format!("task failed: {e}")))?
}

/// Run `future` on the process runtime and block the calling thread until it
/// finishes. Never call it from the runtime.
#[cfg(test)]
pub(crate) fn block_on<F, T>(future: F) -> T
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    runtime().spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.recv()
        .expect("the freenet-mobile runtime dropped a task")
}
