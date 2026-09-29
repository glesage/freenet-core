//! Contract operations for custom Swift and Kotlin screens.
//!
//! A `NodeClient` owns one client API connection. One actor task sends each
//! request and waits for its reply before sending the next, so a reply always
//! belongs to the request in flight. Subscription updates that arrive in
//! between go to their listeners in the order the node sent them.
//!
//! A request that times out has an uncertain outcome: the node may still apply
//! it. The actor then drops the connection, so a late reply can never reach a
//! later request, and tells every subscription listener its subscription
//! closed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use freenet_stdlib::client_api::{
    ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi,
};
use freenet_stdlib::prelude::{
    CodeHash, ContractCode, ContractContainer, ContractInstanceId, ContractKey,
    ContractWasmAPIVersion, Parameters, RelatedContracts, StateDelta, UpdateData, WrappedContract,
    WrappedState,
};
use futures::StreamExt;
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
#[uniffi::export(with_foreign)]
pub trait ContractListener: Send + Sync {
    fn on_update(&self, contract: ContractRef, kind: UpdateKind, bytes: Vec<u8>);
    fn on_closed(&self, reason: String);
}

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

/// Which reply completes a request.
#[derive(Debug, Clone)]
enum Expect {
    Put,
    Get(ContractInstanceId),
    Update(ContractInstanceId),
    Subscribe(ContractInstanceId),
}

impl Expect {
    fn matches(&self, response: &HostResponse) -> bool {
        let HostResponse::ContractResponse(response) = response else {
            return false;
        };
        match (self, response) {
            (Self::Put, ContractResponse::PutResponse { .. }) => true,
            (Self::Get(id), ContractResponse::GetResponse { key, .. }) => key.id() == id,
            (Self::Get(id), ContractResponse::NotFound { instance_id }) => instance_id == id,
            (Self::Update(id), ContractResponse::UpdateResponse { key, .. }) => key.id() == id,
            (Self::Subscribe(id), ContractResponse::SubscribeResponse { key, .. }) => {
                key.id() == id
            }
            _ => false,
        }
    }
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(uniffi::Object)]
pub struct NodeClient {
    commands: mpsc::Sender<Command>,
    ws_port: u16,
}

/// Connect to the node's client API on `ws_port`.
#[uniffi::export]
pub async fn connect_client(ws_port: u16) -> Result<Arc<NodeClient>, MobileError> {
    runtime::run(async move {
        let api = connect_native(ws_port).await?;
        let (commands, inbox) = mpsc::channel(64);
        tokio::spawn(actor(ws_port, api, inbox));
        Ok(Arc::new(NodeClient { commands, ws_port }))
    })
    .await
}

impl NodeClient {
    async fn request(
        &self,
        request: ClientRequest<'static>,
        expect: Expect,
        timeout: Duration,
    ) -> Result<(HostResponse, f64), MobileError> {
        let commands = self.commands.clone();
        runtime::run(async move {
            let (reply, answer) = oneshot::channel();
            commands
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
        })
        .await
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
        let request = ClientRequest::ContractOp(ContractRequest::Put {
            contract,
            state: WrappedState::new(state),
            related_contracts: RelatedContracts::default(),
            subscribe,
            blocking_subscribe: false,
        });
        let (response, node_ms) = self
            .request(request, Expect::Put, timeout_from(timeout_ms))
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
            .request(request, Expect::Update(*key.id()), timeout_from(timeout_ms))
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
    /// `unsubscribe` or until the connection closes.
    pub async fn subscribe(
        &self,
        instance_id: String,
        listener: Arc<dyn ContractListener>,
        timeout_ms: Option<u64>,
    ) -> Result<OpTiming, MobileError> {
        let id = parse_instance_id(&instance_id)?;
        let commands = self.commands.clone();
        runtime::run(async move {
            commands
                .send(Command::Watch { id, listener })
                .await
                .map_err(|_| MobileError::client(ClientErrorKind::Disconnected, "client closed"))
        })
        .await?;
        let request = ClientRequest::ContractOp(ContractRequest::Subscribe {
            key: id,
            summary: None,
        });
        let (response, node_ms) = self
            .request(request, Expect::Subscribe(id), timeout_from(timeout_ms))
            .await?;
        match response {
            HostResponse::ContractResponse(ContractResponse::SubscribeResponse {
                subscribed: true,
                ..
            }) => Ok(OpTiming { node_ms }),
            HostResponse::ContractResponse(ContractResponse::SubscribeResponse { key, .. }) => {
                Err(MobileError::client(
                    ClientErrorKind::OperationFailed,
                    format!("the node did not subscribe to {key}"),
                ))
            }
            other => Err(unexpected(other)),
        }
    }

    /// Stop delivering a contract's updates to its listener. The node keeps
    /// the subscription until this connection closes.
    pub async fn unsubscribe(&self, instance_id: String) -> Result<(), MobileError> {
        let id = parse_instance_id(&instance_id)?;
        let commands = self.commands.clone();
        runtime::run(async move {
            commands
                .send(Command::Unwatch { id })
                .await
                .map_err(|_| MobileError::client(ClientErrorKind::Disconnected, "client closed"))
        })
        .await
    }
}

/// Copy bytes across the bindings and back, without touching the node. The
/// caller's round-trip time for this is the bindings' copy cost.
#[uniffi::export]
pub fn echo_bytes(bytes: Vec<u8>) -> Vec<u8> {
    bytes
}

struct Actor {
    port: u16,
    api: Option<WebApi>,
    listeners: HashMap<ContractInstanceId, Arc<dyn ContractListener>>,
}

impl Actor {
    fn deliver(&self, response: HostResponse) {
        let HostResponse::ContractResponse(ContractResponse::UpdateNotification { key, update }) =
            response
        else {
            tracing::debug!("dropping an unrequested reply: {response}");
            return;
        };
        let Some(listener) = self.listeners.get(key.id()) else {
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
        listener.on_update(ContractRef::from_key(&key), kind, bytes);
    }

    fn close_subscriptions(&mut self, reason: &str) {
        for (_, listener) in self.listeners.drain() {
            listener.on_closed(reason.to_owned());
        }
    }

    async fn connection(&mut self) -> Result<&mut WebApi, MobileError> {
        if self.api.is_none() {
            self.api = Some(connect_native(self.port).await?);
        }
        Ok(self.api.as_mut().expect("connected above"))
    }

    async fn run_request(
        &mut self,
        request: ClientRequest<'static>,
        expect: Expect,
        timeout: Duration,
    ) -> Result<(HostResponse, f64), MobileError> {
        let started = Instant::now();
        let api = self.connection().await?;
        api.send(request)
            .await
            .map_err(|e| MobileError::client(ClientErrorKind::Disconnected, e.to_string()))?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let next =
                tokio::time::timeout_at(deadline, self.api.as_mut().expect("connected").next())
                    .await;
            match next {
                Ok(Some(Ok(response))) if expect.matches(&response) => {
                    return Ok((response, started.elapsed().as_secs_f64() * 1000.0));
                }
                Ok(Some(Ok(response))) => self.deliver(response),
                Ok(Some(Err(err))) => {
                    let kind = classify_client_error(&err);
                    return Err(MobileError::client(kind, err.to_string()));
                }
                Ok(None) => {
                    self.api = None;
                    self.close_subscriptions("the connection closed");
                    return Err(MobileError::client(
                        ClientErrorKind::Disconnected,
                        "the connection closed",
                    ));
                }
                Err(_) => {
                    // Uncertain outcome. Drop the connection so the late reply
                    // cannot complete a later request.
                    self.api = None;
                    self.close_subscriptions("a request timed out and the connection was reset");
                    return Err(MobileError::Timeout {
                        what: format!("the reply to {expect:?}"),
                    });
                }
            }
        }
    }
}

async fn actor(port: u16, api: WebApi, mut inbox: mpsc::Receiver<Command>) {
    let mut actor = Actor {
        port,
        api: Some(api),
        listeners: HashMap::new(),
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
            return;
        };
        match command {
            Command::Request {
                request,
                expect,
                timeout,
                reply,
            } => {
                let result = actor.run_request(*request, expect, timeout).await;
                let _ = reply.send(result);
            }
            Command::Watch { id, listener } => {
                actor.listeners.insert(id, listener);
            }
            Command::Unwatch { id } => {
                actor.listeners.remove(&id);
            }
        }
    }
}
