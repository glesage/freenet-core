# The WebView host ↔ Wasm client protocol

This is a fork-branch document. The reference implementation lives in a sibling
checkout, `atlas-discover-ios`, not in this repository, so citations below name a
file and function rather than linking to one — there is no relative path that
reaches across the two repos.

## 1. Purpose and scope

A native app hosts an off-screen WebView that runs the app's own
wasm-bindgen client. That client — not the host — opens the WebSocket
connection to the embedded node's loopback client API; the bridge between the
native host and the WebView carries only commands and events, in JSON, never
Wasm bytes and never signing material. Keys are derived host-side and handed
to the client once, inside an `initialize` payload; nothing the client emits
afterward carries them back out.

The protocol has two layers:

- **Generic layer.** Envelope shape, reserved commands and message types,
  sessions and generations, error categories, the bundle manifest, asset
  serving, CSP, and lifecycle/timeout semantics. This is what an Android host
  must reproduce byte-for-byte.
- **Application layer.** The specific commands and events an app defines on
  top of the generic layer. §13 documents the one shipped example's layer as
  a worked example; it is **not normative**, and its command/event/error-type
  names are not used anywhere else in this document.

Every envelope and every message carries `protocolVersion: 1`. There is
currently one version; a host that does not recognize it must not process the
message (see §5, §6).

## 2. Bundle layout and manifest

The client bundle is a directory of flat files (no subdirectories) plus one
`manifest.json` alongside them. `manifest.json` is read by the host and
**never served** to the page — `AtlasClientResources.servedNames` (the fixed
set of names the host will ever hand to the WebView) does not include it, and
`AtlasAssetSchemeHandler.file(for:)` only resolves names present in that set,
so even a same-named request for it fails closed.

The manifest is a JSON object with two required keys:

| Key | Type | Meaning |
| --- | --- | --- |
| `protocolVersion` | integer | Must equal the protocol version this document describes (currently `1`). A host must refuse to load a bundle whose manifest declares any other value. |
| `files` | object, name → string | Every file's SHA-256 hash, lowercase hex, exactly 64 characters. |

Everything else in the manifest is optional provenance, informational only —
the reference bundle writer
(`scripts/update-bundled-client.py`, `finish_full_build`) records
`atlasRevision`, `atlasDirty`, `cargoLockSHA256`, `rustVersion`,
`wasmBindgenVersion` and `contractSourceCommit`. **Hosts must ignore unknown
manifest keys.** The reference host's manifest decoder
(`AtlasClientResources.swift`, the private `AtlasClientResourceManifest`
struct) only declares fields for `protocolVersion` and `files`; Swift's
`Decodable` silently drops everything else, which is the ignore-unknown-keys
behavior in practice. A manifest may also list files the host never serves —
the shipped one lists `client.d.ts` and `client_bg.wasm.d.ts` alongside the
five files the host actually serves — and a host is not required to verify or
serve entries outside its own served-names set.

Rules on names:

- A name **must** be a single path component: no `/`, and not `.` or `..`.
  The host's scheme handler enforces this on every request
  (`AtlasAssetSchemeHandler.swift`, `file(for:)`: it rejects any name whose
  `lastPathComponent` differs from itself, and `.`/`..` explicitly), and the
  bundle writer enforces the same rule when it reads the manifest back
  (`scripts/update-bundled-client.py`, `generated_stale`: `Path(name).name !=
  name` raises).
- The host **must** verify every file it intends to serve before loading the
  page that will fetch them — `AtlasClientResources.load(directory:)` hashes
  every name in its served-names set and raises before
  `AtlasWebClient.open` ever calls `view.load(...)`, i.e. verification is
  strictly ordered before the page gets a chance to run.
- Verification **must** stream the file rather than reading it whole
  (`AtlasClientResources.swift`, the private `verify` function reads in 256
  KiB chunks through a `FileHandle`) — the point is to keep a Wasm-sized
  asset out of the host process's heap during a check that runs on every
  launch.
- Empty files are permitted only where the application layer explicitly
  allows it. The reference host additionally requires a fixed subset of
  served names to be non-empty (`AtlasClientResources.requiredNonEmpty`:
  `client.js`, `client_bg.wasm`, `atlas_index_contract.wasm`) and fails
  loading otherwise; that specific list is Atlas's, not generic, but the rule
  — decide per name whether empty is acceptable — is.

## 3. Asset serving

The page is loaded from a private URL scheme with a fixed origin
(`<scheme>://assets/index.html`; the reference implementation uses
`atlas-asset`, defined as `AtlasAssetSchemeHandler.scheme`/`.host`/`.pageURL`),
never from `file:`. A fixed, non-opaque origin is what lets the page's own
`fetch()` calls for sibling assets be same-origin, so the client's own Wasm
and its data never have to cross the bridge as messages.

Only names present in the manifest **and** in the host's own served-names set
are ever served; any other request fails. `AtlasAssetSchemeHandler.file(for:)`
returns `nil` for anything not in `allowedNames`, and
`webView(_:start:)` turns a `nil` into `urlSchemeTask.didFailWithError(...)` —
there is no fallback content and no directory listing.

MIME types (`AtlasAssetSchemeHandler.mimeType(for:)`):

| Extension | Content-Type |
| --- | --- |
| `.wasm` | `application/wasm` |
| `.js` | `text/javascript` |
| `.html` | `text/html` |
| `.json` | `application/json` |
| anything else | `application/octet-stream` |

Bodies are delivered in chunks (`AtlasAssetSchemeHandler.stream(_:handle:)`,
default 256 KiB per chunk, yielding to the run loop between chunks) so a
multi-megabyte Wasm file never sits whole in the host process's memory while
it streams to the WebContent process. Every response carries
`Cache-Control: no-store` (set in `webView(_:start:)`'s response headers) —
the bundle is versioned with the app itself, so there is nothing to
revalidate and nothing that should survive a bundle update.

Navigation away from the entry page is denied. The host's navigation delegate
(`AtlasWebClient.swift`, `webView(_:decidePolicyFor:decisionHandler:)`) only
allows a navigation whose URL equals the loaded resources' `pageURL`, on the
WebView instance the host itself owns; everything else — including a
same-origin navigation to a different path — is cancelled.

## 4. Content-Security-Policy

The entry page (`index.html`) declares:

```
default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self' ws://127.0.0.1:*;
```

| Directive | Effect |
| --- | --- |
| `default-src 'none'` | Nothing loads unless a more specific directive allows it — no images, fonts, styles, frames. The runtime page has no UI, so it needs none of those. |
| `script-src 'self' 'wasm-unsafe-eval'` | Only same-origin scripts run (`bootstrap.js`, `client.js`, fetched over the private scheme), and `'wasm-unsafe-eval'` is the minimum needed to instantiate a WebAssembly module — without it `wasm_bindgen`'s own compilation step is blocked by CSP, not by anything this document controls. |
| `connect-src 'self' ws://127.0.0.1:*` | The page's own `fetch()` of sibling assets is same-origin, already covered by `'self'`; `ws://127.0.0.1:*` is what lets the wasm client open its WebSocket to the embedded node. No non-loopback destination is ever reachable from inside the page. |

The CSP restricts what the page can reach; it does not by itself validate
what the host tells the page to connect to. The host performs that check
separately and before ever loading the page:
`AtlasWebClient.open(config:)` rejects a config whose `wsURL.scheme != "ws"`
or whose `wsURL.host` (case-insensitively) is not `127.0.0.1`, throwing
`AtlasClientError.invalidConfiguration` if either check fails. Both checks
must hold: the CSP is enforced by the WebView engine, port and host validation
is enforced by the host's own Swift code, and an implementer must not treat
either as a substitute for the other.

## 5. Envelope — host → page

The host calls into the page with `window.<hostObject>.dispatch(envelope)`
(the reference name is `window.atlasHost`), invoked through WebKit's
`callAsyncJavaScript`. `envelope` is a JSON object:

```json
{ "protocolVersion": 1, "session": "<string>", "requestId": "<string>", "command": "<string>", "payload": { } }
```

Reserved commands, handled by the generic layer itself:

| Command | Payload | Meaning |
| --- | --- | --- |
| `initialize` | `{ clientWasmUrl, config }` | Loads and instantiates the wasm-bindgen module at `clientWasmUrl` (once per page load) and opens an application client with `config`. |
| `close` | (none) | Tears the current client down and bumps the page's generation (§8). |

Every other command name belongs to the application layer (see §13 for
Atlas's).

**Envelopes with the wrong `protocolVersion`, or a non-string `requestId` or
`session`, are silently ignored — no reply is posted.** This is a MUST, not
an incidental detail: the page's dispatch function
(`bootstrap.js`, top of `window.atlasHost.dispatch`) returns immediately on
`protocolVersion !== 1 || typeof requestId !== 'string' || typeof
requestedSession !== 'string'`, before anything else runs, and posts nothing.
A host must not wait for a reply to a malformed envelope; if it needs to time
one out, it must use the same request-timeout budget as any other command
(§10), because the page will never distinguish a malformed envelope for the
caller.

The host must also accept **exactly one command in flight** at a time and
refuse a second one itself, without the page's help — the page has no
mechanism to reject a second concurrent envelope, so this discipline is the
host's own responsibility.
`AtlasWebClient.swift`, `dispatch(command:payload:timeout:)`, guards
`pending == nil` and throws `AtlasClientError.requestFailed("Atlas accepts
one command at a time.")` otherwise; `close` is exempt from this rule from
the caller's point of view because `close()` clears `pending` itself (by
manufacturing a `cancelled` result, §8) before issuing its own `close`
envelope.

## 6. Messages — page → host

The page posts messages to the host through
`window.webkit.messageHandlers.<name>.postMessage(...)` (the reference name
is `atlasClient`); `bootstrap.js`'s `post` helper adds `protocolVersion: 1`
to every message so this never has to be repeated at each call site.

Reserved message types:

| Type | Carries | Sent |
| --- | --- | --- |
| `hostReady` | no `session` | Exactly once, when the runtime script has finished evaluating (`bootstrap.js`, the `post({ type: 'hostReady' })` call at the bottom of the IIFE — unconditional, and there is only one call site). A host must wait for this before sending `initialize`. |
| `reply` | `session`, `requestId`, optional `error` (§7), `timing: { jsMs, wasmMemoryBytes }` | Once per dispatched command, whether it succeeded or failed. `wasmMemoryBytes` is `null` if the Wasm module hasn't been instantiated yet. |
| `closed` | `session` | Reserved for a page that wants to signal its own teardown is complete. The reference host already decodes and handles it (`AtlasClientMessages.swift`'s `AtlasClientMessageType.closed`; `AtlasWebClient.receive`'s `case .closed: invalidate(..., emitClosed: true)`), but the reference page (`bootstrap.js`) never actually sends it — the host's own `close()` (§9) tears down on a fixed timeout regardless of what the page does, so nothing in this implementation depends on receiving it. |

Everything else is an application event: it carries `session` and a `type`
the application layer defines (Atlas's four are in §13). The generic layer
does not interpret their payload beyond routing by `type` and `session`.

The host **must** drop:

- Any message whose `session` does not equal the host's current session.
  `AtlasWebClient.receive(_:from:frameURL:)` guards
  `message.session == configuration.session` before dispatching on `type`
  (except `hostReady`, which carries no session and is accepted
  unconditionally while the host is not already closing).
- Any message that did not arrive from the main frame at the entry page's
  URL. Two checks enforce this in the reference host: the
  `WKScriptMessageHandler` callback
  (`AtlasWebClient.swift`, `userContentController(_:didReceive:)`) requires
  `message.frameInfo.isMainFrame`, the message's `webView` to be the one the
  host created, and `message.frameInfo.request.url` to equal the resources'
  `pageURL`; `receive(_:from:frameURL:)` repeats the webView-identity and
  URL checks. A malformed body (fails to decode as
  `AtlasClientInboundMessage`) is treated as a protocol violation, not
  silently dropped — see §7.

## 7. Errors

An `error` object is `{ category: "<string>", message: "<string>" }`
(`AtlasClientMessages.swift`'s `AtlasClientMessageError`; produced page-side
by `bootstrap.js`'s `errorInfo`).

Generic categories, meaningful to any host regardless of application:

| Category | Meaning |
| --- | --- |
| `cancelled` | The command was superseded by a session or generation change (§8), or was invalidated by a `close`. |
| `timeout` | No reply within the host's own budget (§10); this category is host-manufactured, never posted by the page. |
| `resources` | A required bundled asset was missing, unreadable, or otherwise unusable. |
| `invalidInput` | The command or its payload was not recognized/well-formed by the receiving layer. |
| `requestFailed` | Catch-all for a failure that isn't one of the more specific categories, and the category a host must fall back to for anything it does not recognize (see below). |
| `runtimeTerminated` | The WebContent process itself died (§9); host-manufactured. |
| `protocolViolation` | The host detected a malformed message from the page (§6); host-manufactured, never posted by the page. |

Application layers may define their own categories that pass through
opaquely end to end — §13 names the shipped example's three.

**An unknown category must be treated as `requestFailed`.** The reference
host's mapping from a wire category string to its own error type
(`AtlasWebClient.swift`, `clientError(_:)`) only special-cases a fixed
allowlist of strings — `timeout`, `cancelled` and `runtimeTerminated` from
the generic set above, plus the shipped example's three application
categories (§13) — via an explicit switch. Every other string — including a
category this document defines generically, such as `resources` or
`invalidInput`, if a page ever put one on a reply — falls into that
function's `default` arm and becomes `requestFailed`. An
implementer is free to map more of the generic categories to distinct native
error types than this reference host does; it must not fail to handle a
category it doesn't recognize.

## 8. Sessions and generations

The host chooses a fresh `session` string for every `open` (the reference
generates a new `UUID` per call — in `NodeManager.swift`, at the call site
that opens the client — and threads it into `AtlasClientConfig.session`,
which becomes every envelope's `session` field for that opening).

The page keeps its own monotonically increasing counter, `generation`
(`bootstrap.js`, module-level `let generation = 0`). `initialize` and `close`
each increment it once, synchronously, before doing anything that awaits.
Work that resumes after an `await` inside `initialize` re-checks its own
captured epoch against the live `generation` and, if they no longer match,
throws `{ category: 'cancelled' }` instead of proceeding — this is what makes
a `close` that arrives mid-initialization actually take effect rather than
racing a client into existence after the fact.

Three cases, each backed by a passing test in
`bootstrap.test.cjs` (run as part of this workstream; see the accompanying
report for the exact command and summary):

- **Close during Wasm initialization → no client is created.** The
  `initialize` handler checks its epoch immediately after `wasm_bindgen(...)`
  resolves and again after fetching the contract Wasm; if `close` bumped
  `generation` in between, it throws `cancelled` before ever constructing
  `new wasm_bindgen.AtlasClient()`. Covered by
  `bootstrap.test.cjs`'s `'close during Wasm initialization prevents
  creating a client'`, which asserts zero calls were made to the client
  constructor/methods and that the `initialize` reply carries a `cancelled`
  error. The host side of the same case has its own test,
  `AtlasWebClientTests.swift`'s `testCloseCancelsOpenWhileWaitingForHostReady`,
  which calls `close()` while `open()` is still waiting for `hostReady` and
  asserts `open()` throws `AtlasClientError.cancelled`.
- **Close during a read → later events for that session are dropped.** At
  the page level this is the one place the outline's shorthand ("the read
  settles cancelled") does not literally hold, and it is worth being precise
  about, because the difference matters for what a host must do. Once
  `close`'s own `await closing.close()` unblocks the application command's
  pending underlying call, `bootstrap.js`'s dispatch for that original
  command falls through to its ordinary success path — it is not forced
  into an error by the page — and posts an unremarkable `reply` with no
  `error` field, addressed to the (by now stale) session. This is verified
  directly: probing `bootstrap.test.cjs`'s fixture for this exact sequence
  shows the interrupted command's reply carries `result: null` and no
  `error`. What *is* true, and what `bootstrap.test.cjs`'s
  `'close interrupts a read and drops old session events'` verifies, is that
  any application event the old client emits after the close is silently
  dropped (`bootstrap.js` only forwards an event when
  `epoch === generation && event.session === session`, and both have
  already moved on by then), and that a fresh command sent afterward on the
  old session gets `cancelled`.

  A caller on the host side can still legitimately observe `cancelled` for
  the interrupted command despite the
  page's own reply being a bare success — but that has to come from the
  host's own bookkeeping, not from trusting the page's reply. The reference
  host does this in `AtlasWebClient.close()`: it calls `cancelPending(with:
  AtlasClientError.cancelled)` — which resolves whatever continuation is in
  `pending` with `.cancelled`, unconditionally — **before** it ever sends the
  `close` envelope to the page. When the stale `reply` for the original
  command eventually arrives, `settle(requestID, ...)` only resumes a
  continuation whose `requestID` still matches `pending`; since `pending` was
  already cleared (and may already hold a different request by then), the
  stale reply is silently discarded. Unlike the page-side behavior above,
  this exact interaction — a real in-flight post-`initialize` dispatch racing
  a concurrent `close()` — is not exercised by a dedicated automated test in
  either repo; it follows directly from reading `close()` and `settle()`,
  which apply unconditionally to whatever is pending rather than
  special-casing which command it was. **An Android host must reproduce this
  host-side half itself** — cancel the pending continuation locally on
  close, and discard any reply whose request id is no longer the one being
  waited on — rather than relying on the page to mark the stale command as
  an error, because the reference page does not.
- **A command addressed to another session never reaches the live client.**
  `bootstrap.js`'s dispatch throws `{ category: 'cancelled' }` whenever
  `!client || session !== requestedSession`, without calling into the client
  at all. Covered by `bootstrap.test.cjs`'s `'a command for another session
  cannot reach the live client'`, which asserts the client received no
  additional call and the reply's error category is `cancelled`.

## 9. Lifecycle

`hostReady` is the signal that the host may send `initialize`; sending it any
earlier has no defined effect because nothing is listening yet.

`close` is **best-effort**, with a short reference timeout (2 seconds): the
host sends it and gives the page a bounded window to react, but tears the
WebView down regardless of whether a reply arrives in time.
`AtlasWebClient.close()` calls `dispatch(command: "close", ..., timeout: 2)`
inside a `try?` — a timeout or any other failure there is deliberately
swallowed — and unconditionally calls `invalidate(...)` afterward, which
removes the script message handler, drops the WebView, and clears
`configuration`/`resources`. A host must not block indefinitely on a `close`
reply.

If the WebContent process itself terminates, the host must report
`runtimeTerminated` and allow a fresh `open` to recover.
`AtlasWebClient.swift`'s `webViewWebContentProcessDidTerminate` invalidates
the client state and emits `.failed(.runtimeTerminated)`; because `open(...)`
always calls `close()` first and starts a brand new `WKWebView`, a
subsequent `open` after this event is a normal fresh start, not a special
recovery path.

## 10. Timeouts

These are the reference implementation's own defaults, not part of the wire
protocol — nothing about the envelope or message format encodes a timeout,
and each side is free to choose its own budgets. They are listed here so an
Android host has a starting point rather than guessing:

| Timeout | Reference value | Where |
| --- | --- | --- |
| Ready (`hostReady` after page load) | 30 s | `AtlasClientConfig.readyTimeout` default |
| Request (any dispatched command) | 900 s | `AtlasClientConfig.requestTimeout` default |
| Close | 2 s | literal in `AtlasWebClient.close()` |

## 11. Relationship to the node

The wasm client, not the host, opens the WebSocket connection —
`ws://127.0.0.1:<port>/v1/contract/command?encodingProtocol=native` — to the
node's client API. The host's only role is to learn `<port>` and pass it
into the `initialize` envelope's `config.wsUrl` field
(`AtlasClientRuntimeConfig.wsUrl`); the client then dials that URL itself
once it opens.

`<port>` comes from the embedded node, not from a value the host invents:
`FreenetNode::api_port() -> Option<u16>` (added by the companion workstream
that gives the node an ephemeral loopback port instead of a fixed one) is
`None` before the node has started and `Some(port)` once it has bound its
client API. A host must read `api_port()` after starting the node and before
constructing the `initialize` envelope, and must treat `None` at that point
as a startup failure rather than falling back to a guessed port.

## 12. Conformance checklist

An implementation on another platform (e.g. Android) must satisfy each of
the following. Section references point back to the fuller explanation and
citation.

- [ ] `manifest.json` is read by the host and never served (§2).
- [ ] The manifest's `protocolVersion` and `files` keys are required; any
      other key is ignored (§2).
- [ ] Every served file name is a single path component, and the host
      verifies it (as its full-file SHA-256, lowercase hex, 64 characters)
      before loading the page, using streamed hashing (§2).
- [ ] The page is served from a private scheme with a fixed origin, never
      `file:`; only manifest-and-served-set names resolve; the correct MIME
      table is used; bodies are chunked; every response carries
      `Cache-Control: no-store`; navigation away from the entry page is
      denied (§3).
- [ ] The CSP in §4 is applied to the page, and the host independently
      validates that the ws URL it will hand the client is `ws://` on
      `127.0.0.1` (§4).
- [ ] Envelopes match the shape in §5; `initialize` and `close` are reserved;
      an envelope with the wrong `protocolVersion` or a non-string
      `requestId`/`session` gets **no reply**; only one command is ever
      in flight (§5).
- [ ] Messages match the shape in §6; `hostReady`/`reply`/`closed` are
      reserved; the host drops any message with the wrong session or not
      from the main frame at the entry URL (§6).
- [ ] Errors are `{ category, message }`; the generic categories in §7 are
      recognized; an unrecognized category is treated as `requestFailed`
      (§7).
- [ ] Sessions are fresh per open; generations are bumped on `initialize`
      and `close`; the three cases in §8 hold, including that the host
      itself — not the page — is responsible for cancelling a pending
      command's continuation and discarding the page's eventual stale reply
      (§8).
- [ ] `close` is best-effort with a bounded timeout and always tears the
      runtime down; WebContent-process termination is reported as
      `runtimeTerminated` and a fresh `open` recovers (§9).
- [ ] The port passed into `initialize.config` comes from the running node
      (`FreenetNode::api_port()`), read after start, never a fixed guess
      (§11).

## 13. Appendix: Atlas application layer (non-normative)

This section is the only place an Atlas-specific name may appear in this
document. It exists as a worked example of an application layer built on
top of §1–§12, not as part of the generic contract.

Atlas's commands (beyond the reserved `initialize`/`close`):

| Command | Payload | Effect |
| --- | --- | --- |
| `loadIndex` | (none) | Loads the local or network product index. |
| `refresh` | (none) | Re-reads the current index state. |
| `addProducts` | `{ products, nowUnix }` | Seeds or appends products, timestamped by the caller. |

Atlas's application events:

| Event `type` | Payload | Meaning |
| --- | --- | --- |
| `ready` | (none) | The client finished opening and is usable. |
| `snapshot` | `{ snapshot: { indexId, products, statusText, noticeText, errorMessage, isBusy } }` | The current view of the index. |
| `localIndexSaved` | `{ indexId }` | Local mode persisted a new index identity. |
| `failed` | `{ error }` | A client-level failure not tied to a single request/reply. |

Atlas's own error categories, which pass through the generic `error.category`
field opaquely end to end (§7): `notFound`, `decode`, `signing`.

`initialize`'s `config` payload, for Atlas specifically, additionally carries
`mode` (`local`/`network`), `wsUrl`, `savedLocalIndexId`, `rootSeed`,
`onlineSeed`, `contractWasmUrl`, and `sampleProducts` — all application-layer
fields riding inside the generic envelope's `payload`, not part of §5's
generic shape.
