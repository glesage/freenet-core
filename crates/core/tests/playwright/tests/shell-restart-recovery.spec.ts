import {
  test,
  expect,
  type ElementHandle,
  type Frame,
  type Page,
  type Response,
} from "@playwright/test";

// Node-restart recovery for the shell's page auth token.
//
// A node restart wipes the node's in-memory token map, so the token baked into
// an open shell page goes stale. When the sandboxed app reconnects its command
// WebSocket (through the injected shim, which asks the shell to open the real
// socket), the node rejects the stale `authToken` by closing that socket with
// application close code 4401 (AUTH_TOKEN_INVALID_CLOSE_CODE in
// client_events/websocket.rs). The shell then used to recover with a full
// top-level `location.replace(<url>&_freload=...)`, which destroys the iframe
// and with it anything the app had not saved: an unsent draft, scroll
// position, in-memory state.
//
// What these tests require instead: on a server-initiated 4401 the shell
// renews its token IN PLACE, with one same-origin, non-navigation GET of its
// own contract root (`/v{1,2}/contract/web/<key>/`), reads the fresh token from
// the response's `Authorization: Bearer <token>` header, and uses it on the
// app's next command socket. The documents, the iframe and the draft survive.
// A node that is not ready (an error status, as a rejoining peer's 503) is
// asked again later, with the page kept. Only a 2xx without a usable token
// falls back to the existing capped `_freload` reload.
//
// How the stale token is produced. The suite's node cannot be restarted, but a
// restart only reaches the shell through one fact: the node now rejects the
// token the shell sends. So, for the test page only, an init script in the TOP
// document wraps the `WebSocket` constructor and puts a same-shape token the
// node never minted on the first N command sockets the shell opens. Each is
// still a real, native socket to the real node, which really rejects it with a
// 4401 close frame, parsed by each engine's own WebSocket implementation; the
// shell's `onclose` sees a genuine server-initiated close. Later sockets, the
// permission-event socket, and every document are left untouched.
//
// Two simpler routes were tried and do not work:
// - Serving the shell document with a swapped `freenetBridge("<token>")`
//   (`route.fulfill`): Chromium then treats the page as non-local and refuses
//   every socket it opens to the loopback node
//   (ERR_BLOCKED_BY_LOCAL_NETWORK_ACCESS_CHECKS, a 1006 close), so no 4401
//   ever arrives.
// - `page.routeWebSocket`: it swaps `WebSocket` in every frame for a JS mock,
//   wraps the permission-event socket too, and synthesises the close event, so
//   the engine's real close handling is never exercised.

const shellUrl = process.env.FREENET_SHELL_URL;

test.beforeAll(() => {
  if (!shellUrl) {
    throw new Error(
      "FREENET_SHELL_URL is not set. These tests are normally driven by the " +
        "Rust harness (crates/core/tests/playwright_shell.rs), which boots a " +
        "node, publishes the fixture, and exports the shell URL.",
    );
  }
});

const COMMAND_PATH = "/v1/contract/command";

// How long an accepted socket must stay open to count as authenticated. The
// node rejects a stale token as soon as the socket connects (an error frame,
// then the 4401 close), so a socket still open this long after its `open`, and
// after answering a request, was not rejected.
const STAYS_OPEN_MS = 1_500;

// Long enough for the shell to finish reacting to a close (a renewal fetch on
// loopback, or a `location.replace` committing) before the test asserts what
// did and did not happen.
const SETTLE_MS = 1_500;

// `ClientRequest::NodeQueries(NodeQuery::ConnectedPeers)` in the node's native
// (bincode 1, fixint) encoding: enum variant indices as little-endian u32,
// NodeQueries = 4, ConnectedPeers = 0. The same request the Rust tests encode
// with `bincode::serialize` (websocket.rs `encode_req`).
const NODE_QUERY_CONNECTED_PEERS = [4, 0, 0, 0, 0, 0, 0, 0];

// The node answers that request with this error only on a connection whose
// token it accepted as a CONTRACT WEB APP's page token (websocket.rs: "Scope
// check: contract web apps ... cannot use NodeQueries"). With no token the
// query would succeed, and with a stale token the socket is closed with 4401
// before any request is read. So this reply proves the socket is authenticated
// as this app, without decoding anything more than a substring.
const APP_SCOPED_REPLY =
  "NodeQueries is not available to contract web applications";

function shellPath(): string {
  return new URL(shellUrl as string).pathname;
}

function commandSocketUrl(): string {
  const u = new URL(shellUrl as string);
  return `ws://${u.host}${COMMAND_PATH}?encodingProtocol=native`;
}

// A token with the shape of one the node mints (base58 of 32 random bytes) that
// this node has never issued: what an open tab holds after a restart.
function neverMintedToken(): string {
  const alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
  let s = "";
  for (let i = 0; i < 44; i++) {
    s += alphabet[Math.floor(Math.random() * alphabet.length)];
  }
  return s;
}

type StaleShell = {
  // The token put on the first N command sockets (never minted by the node).
  stale: string;
  // The token the node minted for this shell document (its `Authorization`
  // header and its `freenetBridge("<token>")` argument).
  pageToken: string;
};

// Load the shell; the first `staleSockets` command sockets the TOP document
// opens carry a token the node never minted, as after a node restart.
async function gotoShellWithStaleSockets(page: Page, staleSockets: number): Promise<StaleShell> {
  const stale = neverMintedToken();
  await page.addInitScript(
    ({ stale, budget, path }) => {
      if (window !== window.top) return;
      const Native = window.WebSocket;
      let left = budget;
      const Wrapped = function (this: unknown, url: string | URL, protocols?: string | string[]) {
        const u = new URL(String(url), location.href);
        if (left > 0 && u.pathname === path) {
          left--;
          u.searchParams.set("authToken", stale);
        }
        return new Native(u.toString(), protocols);
      } as unknown as typeof WebSocket;
      Object.defineProperties(Wrapped, {
        prototype: { value: Native.prototype },
        CONNECTING: { value: Native.CONNECTING },
        OPEN: { value: Native.OPEN },
        CLOSING: { value: Native.CLOSING },
        CLOSED: { value: Native.CLOSED },
      });
      window.WebSocket = Wrapped;
    },
    { stale, budget: staleSockets, path: COMMAND_PATH },
  );
  const resp = await page.goto(shellUrl as string);
  expect(resp?.ok(), "the shell must load").toBe(true);
  const header = (await resp!.allHeaders())["authorization"] ?? "";
  const pageToken = /^Bearer\s+(\S+)$/i.exec(header.trim())?.[1] ?? "";
  expect(pageToken, `the shell response must carry its token: ${header}`).not.toBe("");
  return { stale, pageToken };
}

type SeenRequest = {
  url: string;
  method: string;
  navigation: boolean;
  mainFrame: boolean;
  resourceType: string;
};

type SeenSocket = {
  url: string;
  authToken: string | null;
};

type Tracker = {
  requests: SeenRequest[];
  commandSockets: SeenSocket[];
  renewalResponses: Response[];
};

function isRenewal(r: { url: string; method: string; navigation: boolean }): boolean {
  const u = new URL(r.url);
  return (
    !r.navigation &&
    r.method === "GET" &&
    u.pathname === shellPath() &&
    !u.searchParams.has("__sandbox")
  );
}

// Any document load: a navigation of the shell or of the app iframe, or
// anything carrying the shell's top-level recovery stamp.
function isDocumentLoad(r: SeenRequest): boolean {
  return r.navigation || new URL(r.url).searchParams.has("_freload");
}

// Records every HTTP request (both frames) and every physical command socket
// the page opens. Install before the first navigation.
function track(page: Page): Tracker {
  const t: Tracker = { requests: [], commandSockets: [], renewalResponses: [] };
  page.on("request", (r) => {
    let mainFrame = false;
    try {
      mainFrame = r.frame() === page.mainFrame();
    } catch {
      // Not a frame request (e.g. a service worker's).
    }
    t.requests.push({
      url: r.url(),
      method: r.method(),
      navigation: r.isNavigationRequest(),
      mainFrame,
      resourceType: r.resourceType(),
    });
  });
  page.on("response", (resp) => {
    const r = resp.request();
    if (isRenewal({ url: r.url(), method: r.method(), navigation: r.isNavigationRequest() })) {
      t.renewalResponses.push(resp);
    }
  });
  page.on("websocket", (ws) => {
    if (new URL(ws.url()).pathname !== COMMAND_PATH) return;
    t.commandSockets.push({
      url: ws.url(),
      authToken: new URL(ws.url()).searchParams.get("authToken"),
    });
  });
  return t;
}

function describe(rs: SeenRequest[]): string[] {
  return rs.map(
    (r) =>
      `${r.method} ${r.url} [${r.resourceType}${r.navigation ? ", navigation" : ""}${r.mainFrame ? ", main frame" : ", subframe"}]`,
  );
}

type AppEvent = {
  label: string;
  type: "open" | "close" | "error" | "message";
  code?: number;
  reason?: string;
  text?: string;
};

// The sandboxed app frame and its <iframe> element, once the fixture rendered.
async function appFrameOf(page: Page): Promise<{ frame: Frame; element: ElementHandle<HTMLIFrameElement> }> {
  await expect(page.frameLocator("iframe#app").locator("#title")).toBeVisible();
  const element = (await page.$("iframe#app")) as ElementHandle<HTMLIFrameElement>;
  const frame = await element.contentFrame();
  expect(frame, "the sandboxed app frame must exist").toBeTruthy();
  return { frame: frame!, element };
}

// Install, inside the sandboxed app, a tiny "app" that opens command sockets
// through the injected shim (`WebSocket` there IS the shim) and logs what
// each one sees. A socket opened with `reconnectAs` reopens itself under that
// label the moment it is closed with 4401, the way a real app reconnects.
async function installApp(frame: Frame): Promise<void> {
  await frame.evaluate((url: string) => {
    type Ev = { label: string; type: string; code?: number; reason?: string; text?: string };
    const w = window as unknown as {
      __rr: { events: Ev[]; sockets: Record<string, WebSocket> };
      __rrOpen: (label: string, reconnectAs?: string) => void;
      __rrSend: (label: string, bytes: number[]) => void;
      __rrClose: (label: string, code: number, reason: string) => void;
      __rrState: (label: string) => number;
    };
    w.__rr = { events: [], sockets: {} };
    w.__rrOpen = (label: string, reconnectAs?: string) => {
      const ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";
      w.__rr.sockets[label] = ws;
      ws.onopen = () => w.__rr.events.push({ label, type: "open" });
      ws.onerror = () => w.__rr.events.push({ label, type: "error" });
      ws.onmessage = (e: MessageEvent) => {
        const text =
          typeof e.data === "string"
            ? e.data
            : new TextDecoder().decode(new Uint8Array(e.data as ArrayBuffer));
        w.__rr.events.push({ label, type: "message", text });
      };
      ws.onclose = (e: CloseEvent) => {
        w.__rr.events.push({ label, type: "close", code: e.code, reason: e.reason });
        if (reconnectAs && e.code === 4401) w.__rrOpen(reconnectAs);
      };
    };
    w.__rrSend = (label: string, bytes: number[]) =>
      w.__rr.sockets[label].send(new Uint8Array(bytes));
    w.__rrClose = (label: string, code: number, reason: string) =>
      w.__rr.sockets[label].close(code, reason);
    w.__rrState = (label: string) => w.__rr.sockets[label].readyState;
  }, commandSocketUrl());
}

// The app's event log, or a description of why the frame can no longer be
// read (it was detached or replaced by a reload).
async function appEvents(frame: Frame): Promise<AppEvent[] | string> {
  try {
    return await frame.evaluate(
      () => (window as unknown as { __rr: { events: AppEvent[] } }).__rr.events,
    );
  } catch (e) {
    return `app frame unusable: ${String((e as Error).message).split("\n")[0]}`;
  }
}

async function eventsOrFail(frame: Frame): Promise<AppEvent[]> {
  const ev = await appEvents(frame);
  if (typeof ev === "string") throw new Error(ev);
  return ev;
}

function closesWith4401(ev: AppEvent[] | string, label: string): boolean {
  return typeof ev !== "string" && ev.some((e) => e.label === label && e.type === "close" && e.code === 4401);
}

// Wait until the app has seen every listed socket closed with 4401 (the shell
// relays the node's close code verbatim), or a document has already started
// loading. Then give the shell's reaction (a renewal, or a reload committing)
// time to finish, so the assertions that follow see all of it.
async function waitForShellReaction(page: Page, frame: Frame, t: Tracker, base: number, labels: string[]) {
  await expect
    .poll(
      async () => {
        const since = t.requests.slice(base);
        if (since.some(isDocumentLoad)) return "navigated";
        const ev = await appEvents(frame);
        if (labels.every((l) => closesWith4401(ev, l))) return "closed";
        return typeof ev === "string" ? ev : `waiting: ${JSON.stringify(ev)}`;
      },
      {
        message: `the stale socket(s) ${labels.join(", ")} were never closed with 4401 and the shell never reacted`,
        timeout: 20_000,
      },
    )
    .toMatch(/^(navigated|closed)$/);
  await page.waitForTimeout(SETTLE_MS);
}

function assertNoDocumentLoad(t: Tracker, base: number, what: string) {
  const loads = t.requests.slice(base).filter(isDocumentLoad);
  expect(
    describe(loads),
    `${what}: no document may load after the stale-token close. A top-level ` +
      `\`_freload\` reload (or any navigation) destroys the app iframe and every ` +
      `unsent draft in it; the shell must renew its token in place.`,
  ).toEqual([]);
}

function renewalsSince(t: Tracker, base: number): SeenRequest[] {
  return t.requests.slice(base).filter(isRenewal);
}

// Prove `label` (an app socket) is authenticated as this app: it opened, a
// real request gets the app-scoped reply rather than an auth error, and it is
// still open STAYS_OPEN_MS later.
async function expectAuthenticated(page: Page, frame: Frame, label: string) {
  await expect
    .poll(async () => {
      const ev = await eventsOrFail(frame);
      const mine = ev.filter((e) => e.label === label);
      if (mine.some((e) => e.type === "open")) return "open";
      return JSON.stringify(mine);
    }, { message: `the app's reconnect socket "${label}" never opened`, timeout: 20_000 })
    .toBe("open");

  await frame.evaluate(
    ([l, bytes]) =>
      (window as unknown as { __rrSend: (l: string, b: number[]) => void }).__rrSend(
        l as string,
        bytes as number[],
      ),
    [label, NODE_QUERY_CONNECTED_PEERS] as const,
  );
  await expect
    .poll(async () => {
      const ev = await eventsOrFail(frame);
      const reply = ev.find((e) => e.label === label && e.type === "message");
      if (reply) return reply.text ?? "";
      const closed = ev.find((e) => e.label === label && e.type === "close");
      return closed ? `closed ${closed.code} ${closed.reason}` : "";
    }, { message: `no reply to a real command on "${label}"`, timeout: 20_000 })
    .not.toBe("");
  const ev = await eventsOrFail(frame);
  const reply = ev.find((e) => e.label === label && e.type === "message");
  expect(
    reply?.text ?? "(no reply)",
    `a real command on "${label}" must be served as this app's authenticated connection ` +
      `(the node's app-scope refusal), not rejected as an invalid token`,
  ).toContain(APP_SCOPED_REPLY);
  expect(reply!.text).not.toContain("AUTH_TOKEN_INVALID");

  await page.waitForTimeout(STAYS_OPEN_MS);
  const after = await eventsOrFail(frame);
  expect(
    after.filter((e) => e.label === label && (e.type === "close" || e.type === "error")),
    `"${label}" must stay open: the node closes a stale-token socket with 4401 at once`,
  ).toEqual([]);
  expect(
    await frame.evaluate(
      (l) => (window as unknown as { __rrState: (l: string) => number }).__rrState(l),
      label,
    ),
  ).toBe(1);
}

// The token in a renewal response's `Authorization: Bearer <token>` header.
async function renewedToken(resp: Response | undefined): Promise<string | null> {
  if (!resp) return null;
  const headers = await resp.allHeaders();
  const v = headers["authorization"] ?? "";
  const m = /^Bearer\s+(\S+)$/i.exec(v.trim());
  return m ? m[1] : null;
}

test("a stale-token close renews the token in place: documents, iframe and an unsent draft survive, and the reconnect is authenticated", async ({
  page,
}) => {
  const t = track(page);
  const tokens = await gotoShellWithStaleSockets(page, 1);
  const { frame, element } = await appFrameOf(page);
  await installApp(frame);

  // Unique markers on both documents: a reload of either replaces its window,
  // so a marker that survives proves the document did.
  const marker = `rr-${Date.now()}-${Math.random().toString(36).slice(2)}`;
  await page.evaluate((m) => {
    (window as unknown as { __rrShellMarker: string }).__rrShellMarker = m;
  }, marker);
  await frame.evaluate((m) => {
    (window as unknown as { __rrFrameMarker: string }).__rrFrameMarker = m;
    document.documentElement.dataset.rrMarker = m;
    const input = document.createElement("input");
    input.id = "rr-draft";
    document.body.appendChild(input);
  }, marker);
  const draft = "half-written message, not sent yet";
  await page.frameLocator("iframe#app").locator("#rr-draft").pressSequentially(draft);
  const input = (await frame.$("#rr-draft")) as ElementHandle<HTMLInputElement>;
  expect(await input.inputValue()).toBe(draft);

  const base = t.requests.length;
  const socketsBase = t.commandSockets.length;

  // The app (re)connects after the "restart": the node rejects the stale
  // token with 4401, and the app reconnects at once, as a real app does.
  await frame.evaluate(() =>
    (window as unknown as { __rrOpen: (l: string, r?: string) => void }).__rrOpen("stale", "reconnect"),
  );
  await waitForShellReaction(page, frame, t, base, ["stale"]);

  assertNoDocumentLoad(t, base, "in-place recovery");

  // The same iframe element and Frame, still attached, still showing the same
  // documents.
  expect(frame.isDetached(), "the app Frame was detached (the iframe was destroyed)").toBe(false);
  expect(await element.evaluate((el) => el.isConnected), "the original <iframe> element left the DOM").toBe(true);
  expect(await element.contentFrame(), "the <iframe> now hosts a different Frame").toBe(frame);
  expect(
    await page.evaluate(() => (window as unknown as { __rrShellMarker?: string }).__rrShellMarker),
    "the shell document was replaced",
  ).toBe(marker);
  expect(
    await frame.evaluate(() => [
      (window as unknown as { __rrFrameMarker?: string }).__rrFrameMarker,
      document.documentElement.dataset.rrMarker,
    ]),
    "the app document was replaced",
  ).toEqual([marker, marker]);
  expect(
    await input.evaluate((el) => [el.isConnected, el.value]),
    "the unsent draft must survive in the same <input>",
  ).toEqual([true, draft]);

  // The shell saw a SERVER-initiated close carrying 4401 (it relays the code to
  // the app verbatim): the node really rejected the stale token.
  expect(closesWith4401(await appEvents(frame), "stale"), "the stale socket must close with 4401").toBe(true);

  // Exactly one renewal: a non-navigation GET of the shell's own contract root.
  const renewals = renewalsSince(t, base);
  expect(
    describe(renewals),
    "exactly one in-place renewal fetch of the contract root",
  ).toHaveLength(1);

  await expectAuthenticated(page, frame, "reconnect");

  // Two physical command sockets: the stale one and the app's reconnect, which
  // carries the token the renewal fetched.
  const sockets = t.commandSockets.slice(socketsBase);
  expect(sockets.map((s) => s.url), "physical command sockets after the baseline").toHaveLength(2);
  expect(sockets[0].authToken, "the first socket carried the stale token").toBe(tokens.stale);
  const fresh = sockets[1].authToken;
  expect(fresh, "the reconnect must carry an authToken").toBeTruthy();
  expect(fresh, "the reconnect must NOT reuse the stale token").not.toBe(tokens.stale);
  expect(
    fresh,
    "the reconnect must carry a freshly minted token, not the token the shell already held",
  ).not.toBe(tokens.pageToken);
  const renewalResp = t.renewalResponses[0];
  expect(
    await renewedToken(renewalResp),
    "the reconnect must use exactly the token the renewal response carried (Authorization: Bearer)",
  ).toBe(fresh);
  for (const s of sockets) {
    expect(
      new URL(s.url).searchParams.get("invalidTokenReport"),
      `every proxied command socket opts into the close-only stale-token report: ${s.url}`,
    ).toBe("close");
  }

  // And still nothing reloaded while all of this happened.
  assertNoDocumentLoad(t, base, "in-place recovery, end of test");
  expect(renewalsSince(t, base), "still exactly one renewal").toHaveLength(1);
});

test("two stale-token closes at once start one renewal and no navigation", async ({ page }) => {
  const t = track(page);
  const tokens = await gotoShellWithStaleSockets(page, 2);
  const { frame } = await appFrameOf(page);
  await installApp(frame);

  // Hold the renewal response until the app has seen BOTH stale closes, so
  // the second close arrives while the first renewal is still in flight, every
  // time, whatever the loopback timing.
  let bothClosed!: () => void;
  const bothClosedSeen = new Promise<void>((resolve) => {
    bothClosed = resolve;
  });
  await page.route(
    (u) => u.pathname === shellPath(),
    async (route) => {
      const r = route.request();
      if (!isRenewal({ url: r.url(), method: r.method(), navigation: r.isNavigationRequest() })) {
        return route.fallback();
      }
      await Promise.race([bothClosedSeen, new Promise((res) => setTimeout(res, 10_000))]);
      return route.fallback();
    },
  );

  const base = t.requests.length;
  const socketsBase = t.commandSockets.length;
  await frame.evaluate(() => {
    const open = (window as unknown as { __rrOpen: (l: string) => void }).__rrOpen;
    open("stale-a");
    open("stale-b");
  });
  await expect
    .poll(
      async () => {
        if (t.requests.slice(base).some(isDocumentLoad)) return "navigated";
        const ev = await appEvents(frame);
        if (closesWith4401(ev, "stale-a") && closesWith4401(ev, "stale-b")) return "closed";
        return typeof ev === "string" ? ev : `waiting: ${JSON.stringify(ev)}`;
      },
      { message: "both stale sockets must be closed with 4401", timeout: 20_000 },
    )
    .toMatch(/^(navigated|closed)$/);
  bothClosed();
  await page.waitForTimeout(SETTLE_MS);

  assertNoDocumentLoad(t, base, "two concurrent stale closes");
  expect(frame.isDetached(), "the app Frame was detached (the iframe was destroyed)").toBe(false);
  expect(
    describe(renewalsSince(t, base)),
    "two stale closes must share ONE renewal fetch",
  ).toHaveLength(1);

  await frame.evaluate(() =>
    (window as unknown as { __rrOpen: (l: string) => void }).__rrOpen("after"),
  );
  await expectAuthenticated(page, frame, "after");

  const sockets = t.commandSockets.slice(socketsBase);
  expect(sockets.map((s) => s.url), "physical command sockets after the baseline").toHaveLength(3);
  expect(sockets.slice(0, 2).map((s) => s.authToken)).toEqual([tokens.stale, tokens.stale]);
  const fresh = sockets[2].authToken;
  expect(fresh, "the next socket must carry a new token").toBeTruthy();
  expect(fresh).not.toBe(tokens.stale);
  expect(fresh, "a freshly minted token, not the one the shell already held").not.toBe(tokens.pageToken);
  const renewalResp = t.renewalResponses[0];
  expect(
    await renewedToken(renewalResp),
    "the next socket must use the token the single renewal fetched",
  ).toBe(fresh);

  assertNoDocumentLoad(t, base, "two concurrent stale closes, end of test");
  expect(renewalsSince(t, base), "still exactly one renewal").toHaveLength(1);
});

// A restarted network peer serves its contract root as 503 "Connecting to
// Freenet" until it has rejoined, even for a contract it holds. A reload then
// would swap the app for that page: the shell must keep the page, back off,
// and renew on the app's next attempt once the node answers.
for (const failure of ["HTTP 503", "network error"] as const) {
  test(`a node that is not ready yet (${failure}) keeps the page, and the next attempt after it answers recovers in place`, async ({
    page,
  }) => {
    const t = track(page);
    // Two sockets carry the refused token: the first, and the app's retry
    // during the backoff. The third goes out with whatever the shell holds.
    const tokens = await gotoShellWithStaleSockets(page, 2);
    const { frame, element } = await appFrameOf(page);
    await installApp(frame);
    await frame.evaluate(() => {
      (window as unknown as { __rrFrameMarker: string }).__rrFrameMarker = "kept";
    });

    let notReady = true;
    let refusedRenewals = 0;
    await page.route(
      (u) => u.pathname === shellPath(),
      async (route) => {
        const r = route.request();
        if (!notReady || !isRenewal({ url: r.url(), method: r.method(), navigation: r.isNavigationRequest() })) {
          return route.fallback();
        }
        refusedRenewals++;
        if (failure === "HTTP 503") {
          return route.fulfill({
            status: 503,
            contentType: "text/html",
            body: "<!doctype html><title>Connecting to Freenet</title>",
          });
        }
        return route.abort("failed");
      },
    );

    const base = t.requests.length;
    const socketsBase = t.commandSockets.length;
    await frame.evaluate(() =>
      (window as unknown as { __rrOpen: (l: string) => void }).__rrOpen("stale"),
    );
    await expect
      .poll(() => refusedRenewals, { message: "the shell must try to renew", timeout: 10_000, intervals: [20] })
      .toBe(1);

    // Inside the backoff (1 s from the refused renewal) the app's retry is
    // refused again, and asks nothing.
    await frame.evaluate(() =>
      (window as unknown as { __rrOpen: (l: string) => void }).__rrOpen("backoff"),
    );
    await expect
      .poll(async () => closesWith4401(await appEvents(frame), "backoff"), {
        message: "the retry during the backoff is refused with 4401",
        timeout: 10_000,
      })
      .toBe(true);
    expect(renewalsSince(t, base), "no renewal during the backoff").toHaveLength(1);
    expect(closesWith4401(await appEvents(frame), "stale"), "the first socket was refused with 4401").toBe(true);
    await page.waitForTimeout(SETTLE_MS);
    assertNoDocumentLoad(t, base, `renewal answered ${failure}`);
    expect(frame.isDetached(), "the app Frame was detached").toBe(false);
    expect(await element.contentFrame()).toBe(frame);
    expect(await frame.evaluate(() => (window as unknown as { __rrFrameMarker?: string }).__rrFrameMarker)).toBe("kept");

    // The node is ready; past the backoff, the app's next attempt renews in
    // place and connects with the fresh credential.
    notReady = false;
    await page.waitForTimeout(1_200);
    await frame.evaluate(() =>
      (window as unknown as { __rrOpen: (l: string) => void }).__rrOpen("after"),
    );
    await expectAuthenticated(page, frame, "after");
    expect(renewalsSince(t, base), "one refused renewal, then one that worked").toHaveLength(2);
    const sockets = t.commandSockets.slice(socketsBase);
    expect(sockets.map((s) => s.authToken)).toEqual([tokens.stale, tokens.stale, sockets[2].authToken]);
    expect(sockets[2].authToken).not.toBe(tokens.pageToken);
    expect(await renewedToken(t.renewalResponses[t.renewalResponses.length - 1])).toBe(sockets[2].authToken);
    assertNoDocumentLoad(t, base, `recovery after ${failure}`);
  });
}

// A 2xx without a usable credential (for instance a proxy that strips the
// response header) is something a reload of the shell can fix: the capped
// `_freload` reload stays as the fallback for it.
test("a renewal answered 2xx without a token falls back to the capped _freload reload", async ({ page }) => {
  await gotoShellWithStaleSockets(page, 1);
  const { frame } = await appFrameOf(page);
  await installApp(frame);

  let renewalAttempts = 0;
  await page.route(
    (u) => u.pathname === shellPath(),
    async (route) => {
      const r = route.request();
      if (!isRenewal({ url: r.url(), method: r.method(), navigation: r.isNavigationRequest() })) {
        return route.fallback();
      }
      renewalAttempts++;
      return route.fulfill({ status: 200, contentType: "text/html", body: "<!doctype html><title>shell</title>" });
    },
  );

  const reload = page.waitForRequest(
    (r) =>
      r.isNavigationRequest() &&
      r.frame() === page.mainFrame() &&
      new URL(r.url()).searchParams.has("_freload"),
    { timeout: 20_000 },
  );
  await frame.evaluate(() =>
    (window as unknown as { __rrOpen: (l: string) => void }).__rrOpen("stale"),
  );
  const req = await reload;
  expect(new URL(req.url()).pathname, "the fallback reloads the shell itself").toBe(shellPath());
  // The reloaded shell (the node's own document, with a token it minted)
  // brings the app back.
  await expect(page.frameLocator("iframe#app").locator("#title")).toBeVisible();
  expect(renewalAttempts, "one renewal attempt before the fallback reload").toBe(1);
});

test("an app-requested close(4401) is not a stale-token signal: no renewal, no reload", async ({ page }) => {
  const t = track(page);
  // A normal, valid shell: the socket opens, then the APP closes it with 4401.
  await page.goto(shellUrl as string);
  const { frame } = await appFrameOf(page);
  await installApp(frame);
  await frame.evaluate(() => {
    (window as unknown as { __rrFrameMarker: string }).__rrFrameMarker = "kept";
    (window as unknown as { __rrOpen: (l: string) => void }).__rrOpen("app");
  });
  await expect
    .poll(async () => (await eventsOrFail(frame)).some((e) => e.label === "app" && e.type === "open"), {
      message: "the app socket must open on a valid shell (positive control)",
      timeout: 20_000,
    })
    .toBe(true);

  const base = t.requests.length;
  await frame.evaluate(() =>
    (window as unknown as { __rrClose: (l: string, c: number, r: string) => void }).__rrClose(
      "app",
      4401,
      "app asked",
    ),
  );
  await expect
    .poll(async () => {
      if (t.requests.slice(base).some(isDocumentLoad)) return "navigated";
      const ev = await appEvents(frame);
      if (typeof ev === "string") return ev;
      return ev.some((e) => e.label === "app" && e.type === "close") ? "closed" : "waiting";
    }, { message: "the app socket's close must complete", timeout: 20_000 })
    .toMatch(/^(navigated|closed)$/);
  await page.waitForTimeout(SETTLE_MS);

  assertNoDocumentLoad(t, base, "app-requested close");
  expect(
    describe(renewalsSince(t, base)),
    "a close the app asked for must not renew the token (a contract must not be able to trigger it)",
  ).toEqual([]);
  expect(
    await frame.evaluate(() => (window as unknown as { __rrFrameMarker?: string }).__rrFrameMarker),
  ).toBe("kept");
});
