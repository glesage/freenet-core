//! The WebView bridge: JSON commands from a page, JSON replies and events back.
//!
//! Every message is handled here, so iOS and Android answer each one the same
//! way; the platforms only carry text. The page reaches the node itself, over
//! its own WebSocket to the loopback client API. The bridge only tells it
//! where that is and how the host is doing.
//!
//! Protocol version 1:
//!
//! | Direction | Shape |
//! | --- | --- |
//! | page → host | `{"v":1,"id":"7","cmd":"node.status","args":{}}` |
//! | host → page, success | `{"v":1,"id":"7","ok":{…}}` |
//! | host → page, failure | `{"v":1,"id":"7","err":{"code":"node_not_running","message":"…"}}` |
//! | host → page, event | `{"v":1,"event":"node.peers","data":{"count":3}}` |
//!
//! Commands: `hello`, `node.status`, `node.peers`, `bundle.files`, `ping`.
//! Events: `node.state`, `node.peers`, `node.exited`, `host.lifecycle`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bundle::WebBundle;
use crate::node::{MobileNode, NodeEvent, NodeStatus};
use crate::runtime;

pub const BRIDGE_PROTOCOL_VERSION: u64 = 1;

static BRIDGE_SESSIONS: AtomicU64 = AtomicU64::new(0);

/// Host lifecycle moments the page is told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    /// The host is moving to the background and will stop the node.
    Backgrounding,
    /// The host is back in the foreground with a fresh node session.
    Resumed,
}

#[derive(Debug, Deserialize)]
struct Request {
    v: Option<Value>,
    id: Option<Value>,
    cmd: Option<String>,
    #[serde(default)]
    args: Value,
}

struct BridgeError {
    code: &'static str,
    message: String,
}

impl BridgeError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn reply(id: Value, result: Result<Value, BridgeError>) -> String {
    let message = match result {
        Ok(ok) => json!({ "v": BRIDGE_PROTOCOL_VERSION, "id": id, "ok": ok }),
        Err(err) => json!({
            "v": BRIDGE_PROTOCOL_VERSION,
            "id": id,
            "err": { "code": err.code, "message": err.message },
        }),
    };
    message.to_string()
}

fn event(name: &str, data: Value) -> String {
    json!({ "v": BRIDGE_PROTOCOL_VERSION, "event": name, "data": data }).to_string()
}

fn status_json(status: &NodeStatus) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

fn platform() -> &'static str {
    if cfg!(target_os = "ios") {
        "ios"
    } else if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "other"
    }
}

#[derive(uniffi::Object)]
pub struct WebBridge {
    node: Option<Arc<MobileNode>>,
    bundle: Option<Arc<WebBundle>>,
    session: u64,
}

impl WebBridge {
    async fn dispatch(&self, cmd: &str, args: Value) -> Result<Value, BridgeError> {
        match cmd {
            "hello" => Ok(self.hello()),
            "node.status" => {
                let node = self.node()?;
                Ok(status_json(&node.status()))
            }
            "node.peers" => {
                let node = self.node()?;
                let count = node
                    .clone()
                    .connected_peers()
                    .await
                    .map_err(|e| BridgeError::new("node_error", e.to_string()))?;
                Ok(json!({ "count": count }))
            }
            "bundle.files" => {
                let bundle = self
                    .bundle
                    .as_ref()
                    .ok_or_else(|| BridgeError::new("no_bundle", "this page has no bundle"))?;
                Ok(json!({ "name": bundle.name(), "files": bundle.files() }))
            }
            "ping" => Ok(json!({ "echo": args, "session": self.session })),
            other => Err(BridgeError::new(
                "unknown_command",
                format!("unknown command {other:?}"),
            )),
        }
    }

    fn node(&self) -> Result<&Arc<MobileNode>, BridgeError> {
        self.node
            .as_ref()
            .ok_or_else(|| BridgeError::new("node_not_running", "this page has no node"))
    }

    fn hello(&self) -> Value {
        let node = self.node.as_ref().map(|node| {
            let status = node.status();
            let info = node.current_info();
            json!({
                "state": status.state,
                "session": status.session,
                "connectedPeers": status.connected_peers,
                "wsUrl": info.as_ref().map(|i| i.ws_url.clone()),
                "httpBase": info.as_ref().map(|i| i.http_base.clone()),
                "mode": info.as_ref().map(|i| i.mode),
                "wasmBackend": info.as_ref().map(|i| i.wasm_backend),
            })
        });
        let bundle = self.bundle.as_ref().map(|bundle| {
            json!({
                "name": bundle.name(),
                "protocolVersion": bundle.protocol_version(),
                "files": bundle.files().len(),
            })
        });
        json!({
            "protocol": BRIDGE_PROTOCOL_VERSION,
            "session": self.session,
            "platform": platform(),
            "node": node,
            "bundle": bundle,
        })
    }
}

#[uniffi::export]
impl WebBridge {
    /// A bridge for one page load. Each load gets a new bridge session.
    #[uniffi::constructor]
    pub fn new(node: Option<Arc<MobileNode>>, bundle: Option<Arc<WebBundle>>) -> Arc<Self> {
        Arc::new(Self {
            node,
            bundle,
            session: BRIDGE_SESSIONS.fetch_add(1, Ordering::SeqCst) + 1,
        })
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    /// Answer one message from the page. Always returns a reply to send back.
    pub async fn handle(self: Arc<Self>, message: String) -> String {
        let this = self.clone();
        runtime::run(async move { Ok(this.handle_on_runtime(&message).await) })
            .await
            .unwrap_or_else(|e| {
                reply(
                    Value::Null,
                    Err(BridgeError::new("internal", e.to_string())),
                )
            })
    }

    /// The page event for a node event.
    pub fn event_for(&self, event: NodeEvent) -> String {
        match event {
            NodeEvent::StateChanged { status } => event_named("node.state", status_json(&status)),
            NodeEvent::PeersChanged { count } => {
                event_named("node.peers", json!({ "count": count }))
            }
            NodeEvent::Exited { reason } => event_named("node.exited", json!({ "reason": reason })),
        }
    }

    /// The page event for a host lifecycle moment.
    pub fn lifecycle_event(&self, phase: LifecyclePhase) -> String {
        event_named("host.lifecycle", json!({ "phase": phase }))
    }
}

fn event_named(name: &str, data: Value) -> String {
    event(name, data)
}

impl WebBridge {
    pub(crate) async fn handle_on_runtime(&self, message: &str) -> String {
        let request: Request = match serde_json::from_str(message) {
            Ok(request) => request,
            Err(e) => {
                return reply(
                    Value::Null,
                    Err(BridgeError::new(
                        "invalid_json",
                        format!("not a bridge message: {e}"),
                    )),
                );
            }
        };
        let id = request.id.clone().unwrap_or(Value::Null);
        if request.v.as_ref().and_then(Value::as_u64) != Some(BRIDGE_PROTOCOL_VERSION) {
            return reply(
                id,
                Err(BridgeError::new(
                    "unsupported_version",
                    format!("this host speaks bridge protocol {BRIDGE_PROTOCOL_VERSION}"),
                )),
            );
        }
        if request.id.is_none() {
            return reply(
                id,
                Err(BridgeError::new("missing_id", "every command needs an id")),
            );
        }
        let Some(cmd) = request.cmd else {
            return reply(id, Err(BridgeError::new("missing_command", "no cmd field")));
        };
        let result = self.dispatch(&cmd, request.args).await;
        reply(id, result)
    }
}

/// The script every bridged page gets at document start. It defines
/// `window.freenetHost` with `request(cmd, args)` and `on(event, callback)`,
/// over whichever transport the platform installed: a WKWebView message
/// handler named `freenetHost` on iOS, or a web message listener object named
/// `freenetHostPort` on Android. Replies and events arrive through
/// `window.__freenetHostReceive(text)`.
#[uniffi::export]
pub fn bridge_script() -> String {
    BRIDGE_SCRIPT.to_owned()
}

const BRIDGE_SCRIPT: &str = r#"(function () {
  if (window.freenetHost) { return; }
  var pending = new Map();
  var listeners = new Map();
  var nextId = 1;
  function send(text) {
    if (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.freenetHost) {
      window.webkit.messageHandlers.freenetHost.postMessage(text);
    } else if (window.freenetHostPort) {
      window.freenetHostPort.postMessage(text);
    } else {
      throw new Error('no freenet host transport');
    }
  }
  function receive(text) {
    var msg;
    try { msg = JSON.parse(text); } catch (e) { return; }
    if (!msg || msg.v !== 1) { return; }
    if (msg.event) {
      var named = listeners.get(msg.event) || [];
      var all = listeners.get('*') || [];
      named.concat(all).forEach(function (cb) {
        try { cb(msg.data, msg.event); } catch (e) { console.error(e); }
      });
      return;
    }
    var entry = pending.get(msg.id);
    if (!entry) { return; }
    pending.delete(msg.id);
    if (msg.err) {
      var err = new Error(msg.err.message);
      err.code = msg.err.code;
      entry.reject(err);
    } else {
      entry.resolve(msg.ok);
    }
  }
  function request(cmd, args) {
    var id = String(nextId++);
    var text = JSON.stringify({ v: 1, id: id, cmd: cmd, args: args || {} });
    return new Promise(function (resolve, reject) {
      pending.set(id, { resolve: resolve, reject: reject });
      try { send(text); } catch (e) { pending.delete(id); reject(e); }
    });
  }
  function on(name, cb) {
    var list = listeners.get(name) || [];
    list.push(cb);
    listeners.set(name, list);
    return function () {
      listeners.set(name, (listeners.get(name) || []).filter(function (x) { return x !== cb; }));
    };
  }
  if (window.freenetHostPort) {
    window.freenetHostPort.onmessage = function (e) { receive(e.data); };
  }
  window.__freenetHostReceive = receive;
  window.freenetHost = Object.freeze({ protocol: 1, request: request, on: on });
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> Arc<WebBridge> {
        WebBridge::new(None, None)
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn replies_carry_the_request_id() {
        let reply = runtime::block_on(async {
            bridge()
                .handle_on_runtime(r#"{"v":1,"id":"a1","cmd":"ping","args":{"x":1}}"#)
                .await
        });
        let reply = parse(&reply);
        assert_eq!(reply["id"], "a1");
        assert_eq!(reply["ok"]["echo"]["x"], 1);
    }

    #[test]
    fn errors_are_typed() {
        let cases = [
            ("not json", "invalid_json"),
            (r#"{"v":2,"id":"1","cmd":"hello"}"#, "unsupported_version"),
            (r#"{"id":"1","cmd":"hello"}"#, "unsupported_version"),
            (r#"{"v":1,"cmd":"hello"}"#, "missing_id"),
            (r#"{"v":1,"id":"1"}"#, "missing_command"),
            (r#"{"v":1,"id":"1","cmd":"nope"}"#, "unknown_command"),
            (
                r#"{"v":1,"id":"1","cmd":"node.status"}"#,
                "node_not_running",
            ),
        ];
        for (message, code) in cases {
            let message = message.to_owned();
            let reply =
                runtime::block_on(async move { bridge().handle_on_runtime(&message).await });
            assert_eq!(parse(&reply)["err"]["code"], code, "{reply}");
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let reply = runtime::block_on(async {
            bridge()
                .handle_on_runtime(r#"{"v":1,"id":7,"cmd":"hello","extra":true}"#)
                .await
        });
        let reply = parse(&reply);
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["ok"]["protocol"], 1);
    }

    #[test]
    fn events_have_names_and_data() {
        let text = bridge().event_for(NodeEvent::PeersChanged { count: 3 });
        let value = parse(&text);
        assert_eq!(value["event"], "node.peers");
        assert_eq!(value["data"]["count"], 3);
        let lifecycle = parse(&bridge().lifecycle_event(LifecyclePhase::Backgrounding));
        assert_eq!(lifecycle["data"]["phase"], "backgrounding");
    }
}
