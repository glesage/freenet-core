//! In-process client for the node's loopback websocket API.
//!
//! One actor task owns the [`WebApi`] connection. Requests are serialized (one
//! in flight at a time) because the wire protocol carries no request ids;
//! responses are matched by variant and contract key. Subscription updates
//! (`UpdateNotification`) can arrive at any moment, including while a request
//! is waiting for its answer, and are always routed to the listener instead.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
};
use freenet_stdlib::prelude::*;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;

use crate::api::{ContractUpdateListener, GetResult};
use crate::error::MobileError;

/// How long a single request may wait for its response. Network-mode GETs can
/// take a while on first contact; local-mode requests answer in milliseconds.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Shared slot for the host's listener so it can be (re)set after start.
pub(crate) type ListenerSlot = Arc<std::sync::RwLock<Option<Arc<dyn ContractUpdateListener>>>>;

enum Command {
    Get {
        key: ContractInstanceId,
        subscribe: bool,
        reply: oneshot::Sender<Result<GetResult, MobileError>>,
    },
    Put {
        contract: ContractContainer,
        state: Vec<u8>,
        subscribe: bool,
        reply: oneshot::Sender<Result<String, MobileError>>,
    },
    UpdateDelta {
        key: ContractInstanceId,
        delta: Vec<u8>,
        reply: oneshot::Sender<Result<(), MobileError>>,
    },
    Subscribe {
        key: ContractInstanceId,
        reply: oneshot::Sender<Result<(), MobileError>>,
    },
    Shutdown,
}

/// Handle to the running client actor.
pub(crate) struct ClientHandle {
    tx: mpsc::Sender<Command>,
    task: JoinHandle<()>,
}

impl ClientHandle {
    /// Connect to `ws://127.0.0.1:{port}` (retrying until `within` elapses, the
    /// listener may still be coming up) and spawn the actor on `handle`.
    pub(crate) async fn connect(
        handle: &Handle,
        port: u16,
        listener: ListenerSlot,
        within: Duration,
    ) -> Result<Self, MobileError> {
        let api = handle.spawn(connect_with_retry(port, within)).await??;
        let (tx, rx) = mpsc::channel(32);
        let task = handle.spawn(actor(api, rx, listener));
        Ok(Self { tx, task })
    }

    pub(crate) async fn get(
        &self,
        key: ContractInstanceId,
        subscribe: bool,
    ) -> Result<GetResult, MobileError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Get {
            key,
            subscribe,
            reply,
        })
        .await?;
        rx.await.map_err(|_| actor_gone())?
    }

    pub(crate) async fn put(
        &self,
        contract: ContractContainer,
        state: Vec<u8>,
        subscribe: bool,
    ) -> Result<String, MobileError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Put {
            contract,
            state,
            subscribe,
            reply,
        })
        .await?;
        rx.await.map_err(|_| actor_gone())?
    }

    pub(crate) async fn update_delta(
        &self,
        key: ContractInstanceId,
        delta: Vec<u8>,
    ) -> Result<(), MobileError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::UpdateDelta { key, delta, reply })
            .await?;
        rx.await.map_err(|_| actor_gone())?
    }

    pub(crate) async fn subscribe(&self, key: ContractInstanceId) -> Result<(), MobileError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Subscribe { key, reply }).await?;
        rx.await.map_err(|_| actor_gone())?
    }

    /// Close the connection and wait for the actor to finish.
    pub(crate) async fn shutdown(self) {
        if self.tx.send(Command::Shutdown).await.is_err() {
            tracing::debug!("client actor already stopped");
        }
        drop(self.tx);
        if let Err(e) = self.task.await {
            tracing::warn!(%e, "client actor task ended abnormally");
        }
    }

    async fn send(&self, cmd: Command) -> Result<(), MobileError> {
        self.tx.send(cmd).await.map_err(|_| actor_gone())
    }
}

fn actor_gone() -> MobileError {
    MobileError::InvalidState("node client is not connected".into())
}

async fn connect_with_retry(port: u16, within: Duration) -> Result<WebApi, MobileError> {
    let url = format!("ws://127.0.0.1:{port}/v1/contract/command?encodingProtocol=native");
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match connect_async(&url).await {
            Ok((stream, _)) => return Ok(WebApi::start(stream)),
            Err(e) if tokio::time::Instant::now() >= deadline => {
                return Err(MobileError::Startup(format!(
                    "client API on port {port} did not accept a connection within {within:?}: {e}"
                )));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Contract keys learned from responses, so an UPDATE (which needs the full
/// key, code hash included) can be issued from an instance id alone.
type KnownKeys = HashMap<ContractInstanceId, ContractKey>;

async fn actor(mut api: WebApi, mut rx: mpsc::Receiver<Command>, listener: ListenerSlot) {
    let mut known = KnownKeys::new();
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None | Some(Command::Shutdown) => break,
                Some(cmd) => handle_command(&mut api, &mut known, &listener, cmd).await,
            },
            msg = api.recv() => match msg {
                Ok(response) => {
                    if !dispatch_notification(&response, &mut known, &listener) {
                        tracing::debug!(?response, "ignoring unsolicited response");
                    }
                }
                Err(e) => {
                    tracing::warn!(%e, "node client connection failed; stopping client actor");
                    break;
                }
            },
        }
    }
    api.disconnect("freenet-mobile client stopping").await;
}

async fn handle_command(
    api: &mut WebApi,
    known: &mut KnownKeys,
    listener: &ListenerSlot,
    cmd: Command,
) {
    match cmd {
        Command::Get {
            key,
            subscribe,
            reply,
        } => {
            reply_or_log(reply, do_get(api, known, listener, key, subscribe).await);
        }
        Command::Put {
            contract,
            state,
            subscribe,
            reply,
        } => {
            reply_or_log(
                reply,
                do_put(api, known, listener, contract, state, subscribe).await,
            );
        }
        Command::UpdateDelta { key, delta, reply } => {
            reply_or_log(reply, do_update(api, known, listener, key, delta).await);
        }
        Command::Subscribe { key, reply } => {
            reply_or_log(reply, do_subscribe(api, known, listener, key).await);
        }
        Command::Shutdown => {}
    }
}

/// Deliver a command's outcome; a receiver that already went away is not an error.
fn reply_or_log<T>(
    reply: oneshot::Sender<Result<T, MobileError>>,
    outcome: Result<T, MobileError>,
) {
    if reply.send(outcome).is_err() {
        tracing::debug!("command reply dropped: caller went away");
    }
}

fn send_err(e: impl std::fmt::Display) -> MobileError {
    MobileError::Request(format!("failed to send request: {e}"))
}

async fn do_get(
    api: &mut WebApi,
    known: &mut KnownKeys,
    listener: &ListenerSlot,
    key: ContractInstanceId,
    subscribe: bool,
) -> Result<GetResult, MobileError> {
    api.send(ClientRequest::ContractOp(ContractRequest::Get {
        key,
        return_contract_code: false,
        subscribe,
        blocking_subscribe: false,
    }))
    .await
    .map_err(send_err)?;
    wait_for(api, known, listener, |response, known| match response {
        HostResponse::ContractResponse(ContractResponse::GetResponse {
            key: full, state, ..
        }) if *full.id() == key => {
            known.insert(*full.id(), *full);
            Some(Ok(GetResult {
                key: full.encoded_contract_id(),
                state: state.as_ref().to_vec(),
            }))
        }
        HostResponse::ContractResponse(ContractResponse::NotFound { instance_id })
            if *instance_id == key =>
        {
            Some(Err(MobileError::NotFound(key.to_string())))
        }
        _ => None,
    })
    .await
}

async fn do_put(
    api: &mut WebApi,
    known: &mut KnownKeys,
    listener: &ListenerSlot,
    contract: ContractContainer,
    state: Vec<u8>,
    subscribe: bool,
) -> Result<String, MobileError> {
    let expected = contract.key();
    api.send(ClientRequest::ContractOp(ContractRequest::Put {
        contract,
        state: WrappedState::new(state),
        related_contracts: RelatedContracts::default(),
        subscribe,
        blocking_subscribe: false,
    }))
    .await
    .map_err(send_err)?;
    let id = wait_for(api, known, listener, |response, known| match response {
        HostResponse::ContractResponse(ContractResponse::PutResponse { key })
            if key.id() == expected.id() =>
        {
            known.insert(*key.id(), *key);
            Some(Ok(key.encoded_contract_id()))
        }
        _ => None,
    })
    .await?;
    if subscribe {
        // `Put { subscribe: true }` registers the subscription on the network
        // path but not in local mode (`Executor::contract_requests` acknowledges
        // the PUT without a listener), so make the guarantee explicit in both.
        do_subscribe(api, known, listener, *expected.id()).await?;
    }
    Ok(id)
}

async fn do_update(
    api: &mut WebApi,
    known: &mut KnownKeys,
    listener: &ListenerSlot,
    key: ContractInstanceId,
    delta: Vec<u8>,
) -> Result<(), MobileError> {
    let full = match known.get(&key) {
        Some(full) => *full,
        None => {
            // UPDATE needs the code hash; learn it with a GET first.
            do_get(api, known, listener, key, false).await?;
            known
                .get(&key)
                .copied()
                .ok_or_else(|| MobileError::NotFound(key.to_string()))?
        }
    };
    api.send(ClientRequest::ContractOp(ContractRequest::Update {
        key: full,
        data: UpdateData::Delta(StateDelta::from(delta)),
    }))
    .await
    .map_err(send_err)?;
    wait_for(api, known, listener, |response, _| match response {
        HostResponse::ContractResponse(ContractResponse::UpdateResponse { key: full, .. })
            if *full.id() == key =>
        {
            Some(Ok(()))
        }
        _ => None,
    })
    .await
}

async fn do_subscribe(
    api: &mut WebApi,
    known: &mut KnownKeys,
    listener: &ListenerSlot,
    key: ContractInstanceId,
) -> Result<(), MobileError> {
    api.send(ClientRequest::ContractOp(ContractRequest::Subscribe {
        key,
        summary: None,
    }))
    .await
    .map_err(send_err)?;
    wait_for(api, known, listener, |response, known| match response {
        HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
            key: full,
            subscribed,
        }) if *full.id() == key => {
            known.insert(*full.id(), *full);
            Some(if *subscribed {
                Ok(())
            } else {
                Err(MobileError::Request(format!(
                    "subscription to {key} refused"
                )))
            })
        }
        _ => None,
    })
    .await
}

/// Read responses until `matcher` accepts one, routing update notifications to
/// the listener on the way. An error response ends the request with that error.
async fn wait_for<T>(
    api: &mut WebApi,
    known: &mut KnownKeys,
    listener: &ListenerSlot,
    mut matcher: impl FnMut(&HostResponse, &mut KnownKeys) -> Option<Result<T, MobileError>>,
) -> Result<T, MobileError> {
    let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(MobileError::Timeout(format!(
                "no response within {REQUEST_TIMEOUT:?}"
            )));
        }
        match tokio::time::timeout(remaining, api.recv()).await {
            Ok(Ok(response)) => {
                if let Some(outcome) = matcher(&response, known) {
                    return outcome;
                }
                if !dispatch_notification(&response, known, listener) {
                    tracing::debug!(?response, "ignoring response while waiting");
                }
            }
            Ok(Err(e)) => return Err(MobileError::Request(e.to_string())),
            Err(_) => {
                return Err(MobileError::Timeout(format!(
                    "no response within {REQUEST_TIMEOUT:?}"
                )));
            }
        }
    }
}

/// Forward an `UpdateNotification` to the listener. Returns whether `response`
/// was one.
fn dispatch_notification(
    response: &HostResponse,
    known: &mut KnownKeys,
    listener: &ListenerSlot,
) -> bool {
    let HostResponse::ContractResponse(ContractResponse::UpdateNotification { key, update }) =
        response
    else {
        return false;
    };
    known.insert(*key.id(), *key);
    let (state, delta) = match update {
        UpdateData::State(state) => (Some(state.as_ref().to_vec()), None),
        UpdateData::Delta(delta) => (None, Some(delta.as_ref().to_vec())),
        UpdateData::StateAndDelta { state, delta } => {
            (Some(state.as_ref().to_vec()), Some(delta.as_ref().to_vec()))
        }
        // Related-contract updates carry another contract's data; the API has
        // no shape for them yet.
        _ => {
            tracing::debug!(%key, "ignoring related-contract update notification");
            return true;
        }
    };
    let listener = listener.read().ok().and_then(|slot| slot.clone());
    match listener {
        Some(listener) => listener.on_update(key.encoded_contract_id(), state, delta),
        None => tracing::debug!(%key, "update notification with no listener registered"),
    }
    true
}
