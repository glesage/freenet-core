//! Request ownership and delivery, against a scripted connection.

use std::sync::Arc;
use std::time::Duration;

use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractRequest, ContractResponse, HostResponse,
};
use freenet_stdlib::prelude::{
    CodeHash, ContractCode, ContractContainer, ContractKey, ContractWasmAPIVersion, Parameters,
    StateDelta, StateSummary, UpdateData, WrappedContract, WrappedState,
};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::{Connection, Connector, ContractListener, ContractRef, NodeClient, UpdateKind};
use crate::error::{ClientErrorKind, MobileError};

type Incoming = Option<Result<HostResponse, ClientError>>;

/// The test's end of one connection.
struct Server {
    requests: UnboundedReceiver<ClientRequest<'static>>,
    replies: UnboundedSender<Incoming>,
}

impl Server {
    async fn request(&mut self) -> ClientRequest<'static> {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .expect("a request in time")
            .expect("the connection is open")
    }

    fn reply(&self, response: ContractResponse) {
        let _ = self
            .replies
            .send(Some(Ok(HostResponse::ContractResponse(response))));
    }

    async fn nothing_sent(&mut self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(self.requests.try_recv().is_err(), "nothing more was sent");
    }
}

struct Fake {
    requests: UnboundedSender<ClientRequest<'static>>,
    replies: UnboundedReceiver<Incoming>,
}

impl Connection for Fake {
    fn send(&mut self, request: ClientRequest<'static>) -> BoxFuture<'_, Result<(), String>> {
        let sent = self.requests.send(request).map_err(|e| e.to_string());
        Box::pin(async move { sent })
    }

    fn next(&mut self) -> BoxFuture<'_, Option<Result<HostResponse, ClientError>>> {
        Box::pin(async move { self.replies.recv().await.flatten() })
    }
}

async fn client() -> (Arc<NodeClient>, UnboundedReceiver<Server>) {
    let (servers, accepted) = unbounded_channel();
    let connector: Connector = Arc::new(move || {
        let (requests, request_rx) = unbounded_channel();
        let (reply_tx, replies) = unbounded_channel();
        let _ = servers.send(Server {
            requests: request_rx,
            replies: reply_tx,
        });
        Box::pin(async move { Ok(Box::new(Fake { requests, replies }) as Box<dyn Connection>) })
    });
    let client = NodeClient::spawn(1, connector).await.unwrap();
    (client, accepted)
}

async fn accept(servers: &mut UnboundedReceiver<Server>) -> Server {
    tokio::time::timeout(Duration::from_secs(5), servers.recv())
        .await
        .expect("a connection in time")
        .expect("the connector is alive")
}

fn contract(code: u8) -> (ContractContainer, ContractKey) {
    let container = ContractContainer::Wasm(ContractWasmAPIVersion::V1(WrappedContract::new(
        Arc::new(ContractCode::from(vec![code; 8])),
        Parameters::from(vec![]),
    )));
    let key = container.key();
    (container, key)
}

fn other_code(key: &ContractKey) -> ContractKey {
    ContractKey::from_id_and_code(*key.id(), CodeHash::new([0xAB; 32]))
}

fn kind(err: MobileError) -> ClientErrorKind {
    err.client_kind().expect("a client error")
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<String>>,
    block: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl ContractListener for Recorder {
    fn on_update(&self, _contract: ContractRef, kind: UpdateKind, bytes: Vec<u8>) {
        if let Some(gate) = self.block.lock().take() {
            let _ = gate.recv();
        }
        self.events.lock().push(format!("{kind:?}:{bytes:?}"));
    }

    fn on_closed(&self, reason: String) {
        self.events.lock().push(format!("closed:{reason}"));
    }
}

impl Recorder {
    async fn wait_for(&self, count: usize) -> Vec<String> {
        for _ in 0..250 {
            if self.events.lock().len() >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.events.lock().clone()
    }
}

fn notification(key: ContractKey, byte: u8) -> ContractResponse {
    ContractResponse::UpdateNotification {
        key,
        update: UpdateData::Delta(StateDelta::from(vec![byte])),
    }
}

async fn subscribed(
    client: &Arc<NodeClient>,
    server: &mut Server,
    key: ContractKey,
    listener: Arc<Recorder>,
) {
    let call = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .subscribe(key.id().to_string(), listener, Some(5_000))
                .await
        })
    };
    assert!(matches!(
        server.request().await,
        ClientRequest::ContractOp(ContractRequest::Subscribe { .. })
    ));
    server.reply(ContractResponse::SubscribeResponse {
        key,
        subscribed: true,
    });
    call.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_canceled_in_the_queue_is_never_sent() {
    let (client, mut servers) = client().await;
    let mut server = accept(&mut servers).await;
    let (_, first) = contract(1);
    let (_, second) = contract(2);

    let in_flight = {
        let client = client.clone();
        let target = ContractRef::from_key(&first);
        tokio::spawn(async move { client.update(target, vec![1], Some(5_000)).await })
    };
    assert!(matches!(
        server.request().await,
        ClientRequest::ContractOp(ContractRequest::Update { .. })
    ));
    let queued = {
        let client = client.clone();
        let target = ContractRef::from_key(&second);
        tokio::spawn(async move { client.update(target, vec![2], Some(5_000)).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    queued.abort();
    let _ = queued.await;

    server.reply(ContractResponse::UpdateResponse {
        key: first,
        summary: StateSummary::from(vec![]),
    });
    in_flight.await.unwrap().unwrap();
    server.nothing_sent().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_update_reply_with_another_code_hash_is_an_uncertain_write() {
    let (client, mut servers) = client().await;
    let mut server = accept(&mut servers).await;
    let (_, key) = contract(1);
    let call = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .update(ContractRef::from_key(&key), vec![1], Some(5_000))
                .await
        })
    };
    server.request().await;
    server.reply(ContractResponse::UpdateResponse {
        key: other_code(&key),
        summary: StateSummary::from(vec![]),
    });
    let err = call.await.unwrap().unwrap_err();
    assert_eq!(kind(err), ClientErrorKind::UncertainWrite);

    // The connection was reset: the next request goes out on a new one.
    let next = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .get(key.id().to_string(), false, false, Some(5_000))
                .await
        })
    };
    let mut fresh = accept(&mut servers).await;
    fresh.request().await;
    fresh.reply(ContractResponse::NotFound {
        instance_id: *key.id(),
    });
    assert_eq!(
        kind(next.await.unwrap().unwrap_err()),
        ClientErrorKind::NotFound
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_put_reply_for_another_contract_is_an_uncertain_write() {
    let (client, mut servers) = client().await;
    let mut server = accept(&mut servers).await;
    let (_, other) = contract(9);
    let call = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .put(vec![1; 8], vec![], vec![0], false, Some(5_000))
                .await
        })
    };
    server.request().await;
    server.reply(ContractResponse::PutResponse { key: other });
    assert_eq!(
        kind(call.await.unwrap().unwrap_err()),
        ClientErrorKind::UncertainWrite
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_reply_for_another_contract_is_an_identity_mismatch() {
    let (client, mut servers) = client().await;
    let mut server = accept(&mut servers).await;
    let (_, asked) = contract(1);
    let (_, other) = contract(2);
    let call = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .get(asked.id().to_string(), false, false, Some(5_000))
                .await
        })
    };
    server.request().await;
    server.reply(ContractResponse::GetResponse {
        key: other,
        contract: None,
        state: WrappedState::new(vec![]),
    });
    assert_eq!(
        kind(call.await.unwrap().unwrap_err()),
        ClientErrorKind::IdentityMismatch
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn returned_code_must_match_the_returned_key() {
    let (client, mut servers) = client().await;
    let mut server = accept(&mut servers).await;
    let (_, asked) = contract(1);
    let (code, _) = contract(2);
    let call = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .get(asked.id().to_string(), true, false, Some(5_000))
                .await
        })
    };
    server.request().await;
    server.reply(ContractResponse::GetResponse {
        key: asked,
        contract: Some(code),
        state: WrappedState::new(vec![]),
    });
    assert_eq!(
        kind(call.await.unwrap().unwrap_err()),
        ClientErrorKind::IdentityMismatch
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_with_no_reply_in_time_is_uncertain_and_a_read_times_out() {
    let (client, mut servers) = client().await;
    let mut server = accept(&mut servers).await;
    let (_, key) = contract(1);
    let err = {
        let client = client.clone();
        let call = tokio::spawn(async move {
            client
                .update(ContractRef::from_key(&key), vec![1], Some(100))
                .await
        });
        server.request().await;
        call.await.unwrap().unwrap_err()
    };
    assert_eq!(kind(err), ClientErrorKind::UncertainWrite);

    let call = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .get(key.id().to_string(), false, false, Some(100))
                .await
        })
    };
    let mut fresh = accept(&mut servers).await;
    fresh.request().await;
    assert_eq!(
        kind(call.await.unwrap().unwrap_err()),
        ClientErrorKind::Timeout
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_listener_does_not_hold_up_requests_and_keeps_its_order() {
    let (client, mut servers) = client().await;
    let mut server = accept(&mut servers).await;
    let (_, watched) = contract(1);
    let (_, read) = contract(2);
    let listener = Arc::new(Recorder::default());
    let (open_gate, gate) = std::sync::mpsc::channel();
    *listener.block.lock() = Some(gate);
    subscribed(&client, &mut server, watched, listener.clone()).await;

    for byte in 1..=3 {
        server.reply(notification(watched, byte));
    }
    let call = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .get(read.id().to_string(), false, false, Some(2_000))
                .await
        })
    };
    server.request().await;
    server.reply(ContractResponse::GetResponse {
        key: read,
        contract: None,
        state: WrappedState::new(vec![7]),
    });
    // The listener is still blocked on the first update.
    let got = call.await.unwrap().unwrap();
    assert_eq!(got.state, vec![7]);
    assert!(listener.events.lock().is_empty());

    open_gate.send(()).unwrap();
    assert_eq!(
        listener.wait_for(3).await,
        vec!["Delta:[1]", "Delta:[2]", "Delta:[3]"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timeout_restores_each_subscription_once_on_a_new_connection() {
    let (client, mut servers) = client().await;
    let mut server = accept(&mut servers).await;
    let (_, kept) = contract(1);
    let (_, lost) = contract(2);
    let (_, read) = contract(3);
    let kept_listener = Arc::new(Recorder::default());
    let lost_listener = Arc::new(Recorder::default());
    subscribed(&client, &mut server, kept, kept_listener.clone()).await;
    subscribed(&client, &mut server, lost, lost_listener.clone()).await;

    let call = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .get(read.id().to_string(), false, false, Some(100))
                .await
        })
    };
    server.request().await;
    assert_eq!(
        kind(call.await.unwrap().unwrap_err()),
        ClientErrorKind::Timeout
    );

    let mut fresh = accept(&mut servers).await;
    for _ in 0..2 {
        let ClientRequest::ContractOp(ContractRequest::Subscribe { key, .. }) =
            fresh.request().await
        else {
            panic!("expected a subscribe");
        };
        if key == *kept.id() {
            fresh.reply(ContractResponse::SubscribeResponse {
                key: kept,
                subscribed: true,
            });
        } else {
            fresh.reply(ContractResponse::SubscribeResponse {
                key: lost,
                subscribed: false,
            });
        }
    }
    fresh.nothing_sent().await;
    fresh.reply(notification(kept, 5));
    assert_eq!(kept_listener.wait_for(1).await, vec!["Delta:[5]"]);
    let closed = lost_listener.wait_for(1).await;
    assert_eq!(closed.len(), 1);
    assert!(closed[0].starts_with("closed:"), "{closed:?}");
    // The old connection is gone: nothing more arrives there.
    server.nothing_sent().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_subscribe_stops_watching() {
    let (client, mut servers) = client().await;
    let mut server = accept(&mut servers).await;
    let (_, key) = contract(1);
    let listener = Arc::new(Recorder::default());
    let call = {
        let client = client.clone();
        let listener = listener.clone();
        tokio::spawn(async move {
            client
                .subscribe(key.id().to_string(), listener, Some(5_000))
                .await
        })
    };
    server.request().await;
    server.reply(ContractResponse::SubscribeResponse {
        key,
        subscribed: false,
    });
    assert_eq!(
        kind(call.await.unwrap().unwrap_err()),
        ClientErrorKind::OperationFailed
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.reply(notification(key, 1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(listener.events.lock().is_empty());
}
