//! Contract operations for custom Swift and Kotlin screens.
//!
//! A `NodeClient` owns one client API connection. One actor task sends each
//! request and waits for its reply before sending the next, so a reply always
//! belongs to the request in flight.
//!
//! - A request the caller cancels while it waits in the queue is never sent.
//! - Every reply must name the full contract identity the request named:
//!   instance ID and code hash. A reply for another contract is an identity
//!   mismatch, and the connection is reset.
//! - A put or update with no definite reply (time limit, closed connection,
//!   identity mismatch) fails with `UncertainWrite`: the node may still apply
//!   it, so the caller keeps the exact operation for reconciliation.
//! - Subscription updates go to each listener through its own queue, in the
//!   order the node sent them. A slow listener never holds up requests; a
//!   listener that falls `LISTENER_QUEUE` updates behind is closed.
//! - A request that times out resets the connection, so a late reply can
//!   never reach a later request. The actor then opens a new connection and
//!   subscribes every watched contract again, once. A listener whose
//!   subscription cannot be restored is closed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
};
use freenet_stdlib::prelude::{
    CodeHash, ContractCode, ContractContainer, ContractInstanceId, ContractKey,
    ContractWasmAPIVersion, Parameters, RelatedContracts, StateDelta, UpdateData, WrappedContract,
    WrappedState,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::error::{ClientErrorKind, MobileError, classify_client_error};
use crate::node::connect_native;
use crate::runtime;

/// A contract instance: its instance ID and the hash of its code, both base58.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, uniffi::Record)]
pub struct ContractRef {
    pub instance_id: String,
    pub code_hash: String,
}

impl ContractRef {
    pub(crate) fn from_key(key: &ContractKey) -> Self {
        Self {
            instance_id: key.id().to_string(),
            code_hash: key.code_hash().encode(),
        }
    }

    pub(crate) fn to_key(&self) -> Result<ContractKey, MobileError> {
        let id = parse_instance_id(&self.instance_id)?;
        let hash = bs58::decode(&self.code_hash)
            .into_vec()
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .ok_or_else(|| MobileError::InvalidSettings {
                reason: format!("code hash {:?} is not 32 bytes of base58", self.code_hash),
            })?;
        Ok(ContractKey::from_id_and_code(id, CodeHash::new(hash)))
    }
}

pub(crate) fn parse_instance_id(id: &str) -> Result<ContractInstanceId, MobileError> {
    ContractInstanceId::try_from(id.to_owned()).map_err(|e| MobileError::InvalidSettings {
        reason: format!("contract instance id {id:?} is not valid: {e}"),
    })
}

/// `ContractKey`'s own equality compares instance IDs only.
fn same_identity(a: &ContractKey, b: &ContractKey) -> bool {
    a.id() == b.id() && a.code_hash() == b.code_hash()
}

/// How long the node took to answer, measured in Rust from sending the
/// request to receiving its reply. The caller's own timing minus this is the
/// cost of the bindings.
#[derive(Debug, Clone, PartialEq, Serialize, uniffi::Record)]
pub struct OpTiming {
    pub node_ms: f64,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct GetResult {
    pub contract: ContractRef,
    pub state: Vec<u8>,
    /// The contract's Wasm, when requested.
    pub code: Option<Vec<u8>>,
    /// The contract's parameters, which travel with its code.
    pub params: Option<Vec<u8>>,
    pub timing: OpTiming,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PutResult {
    pub contract: ContractRef,
    pub timing: OpTiming,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct UpdateResult {
    pub contract: ContractRef,
    pub summary: Vec<u8>,
    pub timing: OpTiming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum UpdateKind {
    State,
    Delta,
    StateAndDelta,
    Related,
}

/// Receives a subscription's updates, in the order the node sent them.
///
/// Called on a blocking runtime thread, one call at a time per listener.
/// Implementations hand the update to the platform's own executor.
#[uniffi::export(with_foreign)]
pub trait ContractListener: Send + Sync {
    fn on_update(&self, contract: ContractRef, kind: UpdateKind, bytes: Vec<u8>);
    fn on_closed(&self, reason: String);
}

/// Updates a listener may fall behind before it is closed.
const LISTENER_QUEUE: usize = 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Time limit for each subscription restored after a reset.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(10);

/// The actor's view of one client API connection.
pub(crate) trait Connection: Send {
    fn send(&mut self, request: ClientRequest<'static>) -> BoxFuture<'_, Result<(), String>>;
    fn next(&mut self) -> BoxFuture<'_, Option<Result<HostResponse, ClientError>>>;
}

impl Connection for WebApi {
    fn send(&mut self, request: ClientRequest<'static>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move { WebApi::send(self, request).await.map_err(|e| e.to_string()) })
    }

    fn next(&mut self) -> BoxFuture<'_, Option<Result<HostResponse, ClientError>>> {
        Box::pin(StreamExt::next(self))
    }
}

/// Opens a new connection to the same node.
pub(crate) type Connector =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Box<dyn Connection>, MobileError>> + Send + Sync>;

type Reply = oneshot::Sender<Result<(HostResponse, f64), MobileError>>;

enum Command {
    Request {
        request: Box<ClientRequest<'static>>,
        expect: Expect,
        timeout: Duration,
        reply: Reply,
    },
    Watch {
        id: ContractInstanceId,
        listener: Arc<dyn ContractListener>,
    },
    Unwatch {
        id: ContractInstanceId,
    },
}

/// Which reply completes a request, and the identity it must carry.
#[derive(Debug, Clone)]
enum Expect {
    Put(ContractKey),
    Get(ContractInstanceId),
    Update(ContractKey),
    Subscribe(ContractInstanceId),
}

enum Verdict {
    /// The reply to the request in flight.
    Reply,
    /// The right kind of reply for another contract.
    Mismatch(String),
    /// Not a reply to this request, such as a subscription update.
    Other,
}

impl Expect {
    fn is_write(&self) -> bool {
        matches!(self, Self::Put(_) | Self::Update(_))
    }

    fn judge(&self, response: &HostResponse) -> Verdict {
        let HostResponse::ContractResponse(response) = response else {
            return Verdict::Other;
        };
        let mismatch = |got: &dyn std::fmt::Display| {
            Verdict::Mismatch(format!("asked about {self:?}, the reply named {got}"))
        };
        match (self, response) {
            (Self::Put(want), ContractResponse::PutResponse { key }) => {
                if same_identity(want, key) {
                    Verdict::Reply
                } else {
                    mismatch(&ContractRef::from_key(key).describe())
                }
            }
            (Self::Get(id), ContractResponse::GetResponse { key, contract, .. }) => {
                let code_matches = contract
                    .as_ref()
                    .is_none_or(|c| same_identity(&c.key(), key));
                if key.id() == id && code_matches {
                    Verdict::Reply
                } else {
                    mismatch(&ContractRef::from_key(key).describe())
                }
            }
            (Self::Get(id), ContractResponse::NotFound { instance_id }) => {
                if instance_id == id {
                    Verdict::Reply
                } else {
                    mismatch(instance_id)
                }
            }
            (Self::Update(want), ContractResponse::UpdateResponse { key, .. }) => {
                if same_identity(want, key) {
                    Verdict::Reply
                } else {
                    mismatch(&ContractRef::from_key(key).describe())
                }
            }
            (Self::Subscribe(id), ContractResponse::SubscribeResponse { key, .. }) => {
                if key.id() == id {
                    Verdict::Reply
                } else {
                    mismatch(&ContractRef::from_key(key).describe())
                }
            }
            _ => Verdict::Other,
        }
    }
}

impl ContractRef {
    fn describe(&self) -> String {
        format!("{}/{}", self.instance_id, self.code_hash)
    }
}

#[derive(uniffi::Object)]
pub struct NodeClient {
    commands: mpsc::Sender<Command>,
    ws_port: u16,
}

/// Connect to the node's client API on `ws_port`.
#[uniffi::export]
pub async fn connect_client(ws_port: u16) -> Result<Arc<NodeClient>, MobileError> {
    let connector: Connector = Arc::new(move || {
        Box::pin(async move {
            let api = connect_native(ws_port).await?;
            Ok(Box::new(api) as Box<dyn Connection>)
        })
    });
    runtime::run(async move { NodeClient::spawn(ws_port, connector).await }).await
}

impl NodeClient {
    /// Open the first connection and start the actor on the current runtime.
    pub(crate) async fn spawn(
        ws_port: u16,
        connector: Connector,
    ) -> Result<Arc<Self>, MobileError> {
        let connection = connector().await?;
        let (commands, inbox) = mpsc::channel(64);
        tokio::spawn(actor(connector, connection, inbox));
        Ok(Arc::new(NodeClient { commands, ws_port }))
    }

    /// Queue a request and wait for its reply. The wait runs on the caller's
    /// executor: when the caller cancels, the reply channel closes and the
    /// actor skips the request if it has not been sent yet.
    async fn request(
        &self,
        request: ClientRequest<'static>,
        expect: Expect,
        timeout: Duration,
    ) -> Result<(HostResponse, f64), MobileError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Request {
                request: Box::new(request),
                expect,
                timeout,
                reply,
            })
            .await
            .map_err(|_| MobileError::client(ClientErrorKind::Disconnected, "client closed"))?;
        answer
            .await
            .map_err(|_| MobileError::client(ClientErrorKind::Disconnected, "client closed"))?
    }

    async fn command(&self, command: Command) -> Result<(), MobileError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| MobileError::client(ClientErrorKind::Disconnected, "client closed"))
    }
}

fn timeout_from(ms: Option<u64>) -> Duration {
    ms.map(Duration::from_millis).unwrap_or(DEFAULT_TIMEOUT)
}

fn unexpected(response: HostResponse) -> MobileError {
    MobileError::client(
        ClientErrorKind::Other,
        format!("unexpected reply: {response}"),
    )
}

#[uniffi::export]
impl NodeClient {
    pub fn ws_port(&self) -> u16 {
        self.ws_port
    }

    /// Store a contract and its first state on the node.
    pub async fn put(
        &self,
        code: Vec<u8>,
        parameters: Vec<u8>,
        state: Vec<u8>,
        subscribe: bool,
        timeout_ms: Option<u64>,
    ) -> Result<PutResult, MobileError> {
        let contract = ContractContainer::Wasm(ContractWasmAPIVersion::V1(WrappedContract::new(
            Arc::new(ContractCode::from(code)),
            Parameters::from(parameters),
        )));
        let key = contract.key();
        let request = ClientRequest::ContractOp(ContractRequest::Put {
            contract,
            state: WrappedState::new(state),
            related_contracts: RelatedContracts::default(),
            subscribe,
            blocking_subscribe: false,
        });
        let (response, node_ms) = self
            .request(request, Expect::Put(key), timeout_from(timeout_ms))
            .await?;
        match response {
            HostResponse::ContractResponse(ContractResponse::PutResponse { key }) => {
                Ok(PutResult {
                    contract: ContractRef::from_key(&key),
                    timing: OpTiming { node_ms },
                })
            }
            other => Err(unexpected(other)),
        }
    }

    /// Read a contract's current state.
    pub async fn get(
        &self,
        instance_id: String,
        return_code: bool,
        subscribe: bool,
        timeout_ms: Option<u64>,
    ) -> Result<GetResult, MobileError> {
        let id = parse_instance_id(&instance_id)?;
        let request = ClientRequest::ContractOp(ContractRequest::Get {
            key: id,
            return_contract_code: return_code,
            subscribe,
            blocking_subscribe: false,
        });
        let (response, node_ms) = self
            .request(request, Expect::Get(id), timeout_from(timeout_ms))
            .await?;
        match response {
            HostResponse::ContractResponse(ContractResponse::GetResponse {
                key,
                contract,
                state,
            }) => Ok(GetResult {
                contract: ContractRef::from_key(&key),
                state: state.as_ref().to_vec(),
                code: contract.as_ref().map(|c| c.data().to_vec()),
                params: contract.as_ref().map(|c| c.params().as_ref().to_vec()),
                timing: OpTiming { node_ms },
            }),
            HostResponse::ContractResponse(ContractResponse::NotFound { instance_id }) => {
                Err(MobileError::client(
                    ClientErrorKind::NotFound,
                    format!("{instance_id} was not found"),
                ))
            }
            other => Err(unexpected(other)),
        }
    }

    /// Send a delta to a contract. Returns the node's summary of the new state.
    pub async fn update(
        &self,
        contract: ContractRef,
        delta: Vec<u8>,
        timeout_ms: Option<u64>,
    ) -> Result<UpdateResult, MobileError> {
        let key = contract.to_key()?;
        let request = ClientRequest::ContractOp(ContractRequest::Update {
            key,
            data: UpdateData::Delta(StateDelta::from(delta)),
        });
        let (response, node_ms) = self
            .request(request, Expect::Update(key), timeout_from(timeout_ms))
            .await?;
        match response {
            HostResponse::ContractResponse(ContractResponse::UpdateResponse { key, summary }) => {
                Ok(UpdateResult {
                    contract: ContractRef::from_key(&key),
                    summary: summary.as_ref().to_vec(),
                    timing: OpTiming { node_ms },
                })
            }
            other => Err(unexpected(other)),
        }
    }

    /// Subscribe to a contract's updates. `listener` receives them until
    /// `unsubscribe`, until the connection closes, or until a restore after a
    /// reset fails.
    pub async fn subscribe(
        &self,
        instance_id: String,
        listener: Arc<dyn ContractListener>,
        timeout_ms: Option<u64>,
    ) -> Result<OpTiming, MobileError> {
        let id = parse_instance_id(&instance_id)?;
        self.command(Command::Watch { id, listener }).await?;
        let request = ClientRequest::ContractOp(ContractRequest::Subscribe {
            key: id,
            summary: None,
        });
        let result = self
            .request(request, Expect::Subscribe(id), timeout_from(timeout_ms))
            .await;
        let outcome = match result {
            Ok((
                HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                    subscribed: true,
                    ..
                }),
                node_ms,
            )) => return Ok(OpTiming { node_ms }),
            Ok((
                HostResponse::ContractResponse(ContractResponse::SubscribeResponse { key, .. }),
                _,
            )) => MobileError::client(
                ClientErrorKind::OperationFailed,
                format!("the node did not subscribe to {key}"),
            ),
            Ok((other, _)) => unexpected(other),
            Err(err) => err,
        };
        let _ = self.command(Command::Unwatch { id }).await;
        Err(outcome)
    }

    /// Stop delivering a contract's updates to its listener. The node keeps
    /// the subscription until this connection closes.
    pub async fn unsubscribe(&self, instance_id: String) -> Result<(), MobileError> {
        let id = parse_instance_id(&instance_id)?;
        self.command(Command::Unwatch { id }).await
    }
}

/// Copy bytes across the bindings and back, without touching the node. The
/// caller's round-trip time for this is the bindings' copy cost.
#[uniffi::export]
pub fn echo_bytes(bytes: Vec<u8>) -> Vec<u8> {
    bytes
}

enum Delivery {
    Update(ContractRef, UpdateKind, Vec<u8>),
    Closed(String),
}

/// One listener's ordered delivery queue, drained off the actor.
struct Watch {
    queue: mpsc::Sender<Delivery>,
}

impl Watch {
    fn new(listener: Arc<dyn ContractListener>) -> Self {
        let (queue, mut deliveries) = mpsc::channel::<Delivery>(LISTENER_QUEUE);
        tokio::spawn(async move {
            while let Some(delivery) = deliveries.recv().await {
                let listener = listener.clone();
                let last = matches!(delivery, Delivery::Closed(_));
                let _ = tokio::task::spawn_blocking(move || match delivery {
                    Delivery::Update(contract, kind, bytes) => {
                        listener.on_update(contract, kind, bytes)
                    }
                    Delivery::Closed(reason) => listener.on_closed(reason),
                })
                .await;
                if last {
                    return;
                }
            }
        });
        Self { queue }
    }

    /// Queue an update. `false` when the listener has fallen too far behind.
    fn offer(&self, delivery: Delivery) -> bool {
        self.queue.try_send(delivery).is_ok()
    }

    fn close(self, reason: String) {
        let queue = self.queue;
        tokio::spawn(async move {
            let _ = queue.send(Delivery::Closed(reason)).await;
        });
    }
}

struct Actor {
    connector: Connector,
    api: Option<Box<dyn Connection>>,
    watches: HashMap<ContractInstanceId, Watch>,
    /// Set when the last request lost its connection; the subscriptions are
    /// restored after the caller has its answer.
    pending_reset: Option<&'static str>,
}

impl Actor {
    fn deliver(&mut self, response: HostResponse) {
        let HostResponse::ContractResponse(ContractResponse::UpdateNotification { key, update }) =
            response
        else {
            tracing::debug!("dropping an unrequested reply: {response}");
            return;
        };
        let Some(watch) = self.watches.get(key.id()) else {
            return;
        };
        let (kind, bytes) = match update {
            UpdateData::State(state) => (UpdateKind::State, state.as_ref().to_vec()),
            UpdateData::Delta(delta) => (UpdateKind::Delta, delta.as_ref().to_vec()),
            UpdateData::StateAndDelta { delta, .. } => {
                (UpdateKind::StateAndDelta, delta.as_ref().to_vec())
            }
            _ => (UpdateKind::Related, Vec::new()),
        };
        if !watch.offer(Delivery::Update(ContractRef::from_key(&key), kind, bytes)) {
            if let Some(watch) = self.watches.remove(key.id()) {
                watch.close(format!("the listener fell {LISTENER_QUEUE} updates behind"));
            }
        }
    }

    fn close_subscriptions(&mut self, reason: &str) {
        for (_, watch) in self.watches.drain() {
            watch.close(reason.to_owned());
        }
    }

    async fn connection(&mut self) -> Result<&mut Box<dyn Connection>, MobileError> {
        if self.api.is_none() {
            self.api = Some((self.connector)().await?);
        }
        Ok(self.api.as_mut().expect("connected above"))
    }

    /// A request's outcome was uncertain and its connection was dropped, so a
    /// late reply cannot complete a later request. Restore the subscriptions
    /// once on a new connection.
    async fn reset(&mut self, why: &str) {
        if self.watches.is_empty() {
            return;
        }
        let ids: Vec<ContractInstanceId> = self.watches.keys().copied().collect();
        if let Err(err) = self.connection().await {
            self.close_subscriptions(&format!("{why}; reconnecting failed: {err}"));
            return;
        }
        for id in ids {
            let request = ClientRequest::ContractOp(ContractRequest::Subscribe {
                key: id,
                summary: None,
            });
            let restored = match self
                .exchange(request, &Expect::Subscribe(id), RESTORE_TIMEOUT)
                .await
            {
                Ok((
                    HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                        subscribed: true,
                        ..
                    }),
                    _,
                )) => Ok(()),
                Ok((other, _)) => Err(format!("unexpected reply {other}")),
                Err(Exchange::Failed(err)) => Err(err.to_string()),
                Err(Exchange::Lost(lost)) => {
                    // No second reset: the remaining subscriptions close.
                    self.api = None;
                    let err = lost.into_error(&Expect::Subscribe(id));
                    self.close_subscriptions(&format!("{why}; restoring failed: {err}"));
                    return;
                }
            };
            if let Err(err) = restored {
                if let Some(watch) = self.watches.remove(&id) {
                    watch.close(format!("{why}; restoring the subscription failed: {err}"));
                }
            }
        }
    }

    /// Send one request and wait for its reply on the current connection.
    async fn exchange(
        &mut self,
        request: ClientRequest<'static>,
        expect: &Expect,
        timeout: Duration,
    ) -> Result<(HostResponse, f64), Exchange> {
        let started = Instant::now();
        let api = self.connection().await.map_err(Exchange::Failed)?;
        api.send(request)
            .await
            .map_err(|e| Exchange::Failed(MobileError::client(ClientErrorKind::Disconnected, e)))?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let api = self.api.as_mut().expect("connected");
            match tokio::time::timeout_at(deadline, api.next()).await {
                Ok(Some(Ok(response))) => match expect.judge(&response) {
                    Verdict::Reply => {
                        return Ok((response, started.elapsed().as_secs_f64() * 1000.0));
                    }
                    Verdict::Mismatch(detail) => {
                        return Err(Exchange::Lost(Lost::Mismatch(detail)));
                    }
                    Verdict::Other => self.deliver(response),
                },
                Ok(Some(Err(err))) => {
                    let kind = classify_client_error(&err);
                    return Err(Exchange::Failed(MobileError::client(kind, err.to_string())));
                }
                Ok(None) => return Err(Exchange::Lost(Lost::Closed)),
                Err(_) => return Err(Exchange::Lost(Lost::TimedOut)),
            }
        }
    }

    async fn run_request(
        &mut self,
        request: ClientRequest<'static>,
        expect: Expect,
        timeout: Duration,
    ) -> Result<(HostResponse, f64), MobileError> {
        match self.exchange(request, &expect, timeout).await {
            Ok(reply) => Ok(reply),
            Err(Exchange::Failed(err)) => Err(err),
            Err(Exchange::Lost(lost)) => {
                match &lost {
                    Lost::Closed => {
                        self.api = None;
                        self.close_subscriptions("the connection closed");
                    }
                    Lost::TimedOut => {
                        self.api = None;
                        self.pending_reset =
                            Some("a request timed out and the connection was reset");
                    }
                    Lost::Mismatch(_) => {
                        self.api = None;
                        self.pending_reset =
                            Some("a reply named another contract and the connection was reset");
                    }
                }
                Err(lost.into_error(&expect))
            }
        }
    }
}

/// Why a request ended without its reply.
enum Exchange {
    /// A definite failure: the request was refused or never sent.
    Failed(MobileError),
    /// The request was sent and its outcome is unknown.
    Lost(Lost),
}

enum Lost {
    TimedOut,
    Closed,
    Mismatch(String),
}

impl Lost {
    fn into_error(self, expect: &Expect) -> MobileError {
        let write = expect.is_write();
        match self {
            Self::TimedOut if write => MobileError::client(
                ClientErrorKind::UncertainWrite,
                format!("no reply to {expect:?} in time"),
            ),
            Self::TimedOut => MobileError::Timeout {
                what: format!("the reply to {expect:?}"),
            },
            Self::Closed if write => MobileError::client(
                ClientErrorKind::UncertainWrite,
                "the connection closed before the reply",
            ),
            Self::Closed => {
                MobileError::client(ClientErrorKind::Disconnected, "the connection closed")
            }
            Self::Mismatch(detail) if write => {
                MobileError::client(ClientErrorKind::UncertainWrite, detail)
            }
            Self::Mismatch(detail) => {
                MobileError::client(ClientErrorKind::IdentityMismatch, detail)
            }
        }
    }
}

async fn actor(
    connector: Connector,
    connection: Box<dyn Connection>,
    mut inbox: mpsc::Receiver<Command>,
) {
    let mut actor = Actor {
        connector,
        api: Some(connection),
        watches: HashMap::new(),
        pending_reset: None,
    };
    loop {
        let command = match actor.api.as_mut() {
            Some(api) => tokio::select! {
                command = inbox.recv() => command,
                response = api.next() => {
                    match response {
                        Some(Ok(response)) => actor.deliver(response),
                        Some(Err(err)) => tracing::debug!("client error between requests: {err}"),
                        None => {
                            actor.api = None;
                            actor.close_subscriptions("the connection closed");
                        }
                    }
                    continue;
                }
            },
            None => inbox.recv().await,
        };
        let Some(command) = command else {
            actor.close_subscriptions("the client closed");
            return;
        };
        match command {
            Command::Request {
                request,
                expect,
                timeout,
                reply,
            } => {
                // Canceled while queued: never sent.
                if reply.is_closed() {
                    tracing::debug!("skipping {expect:?}: canceled before it was sent");
                    continue;
                }
                let result = actor.run_request(*request, expect, timeout).await;
                let _ = reply.send(result);
                if let Some(why) = actor.pending_reset.take() {
                    actor.reset(why).await;
                }
            }
            Command::Watch { id, listener } => {
                if let Some(old) = actor.watches.insert(id, Watch::new(listener)) {
                    old.close("replaced by a new listener".into());
                }
            }
            Command::Unwatch { id } => {
                actor.watches.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests;
