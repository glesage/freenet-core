// Executable unit test for the shell's peer-restart recovery safeguards in
// `path_handlers/assets/shell_bridge.js` (PR #4781, server-4401 design):
//
//   - `isTrustedStaleTokenClose` / `clampProxiedCloseCode` — the recovery is
//     driven by the node's TRUSTED 4401 close code, and a sandboxed contract
//     cannot forge it by asking the shell to close its own socket.
//   - `reloadUrlCapDecision` — a fail-CLOSED, storage-independent cap on
//     recovery reloads: the count lives in the `_freload` URL param (not
//     writable by the contract, always present even with no sessionStorage), so
//     a reload loop is bounded even in private/no-storage mode.
//   - the `command-socket-recovery` region — on that trusted 4401 the shell
//     first renews its credential in place (one renewal per rejected
//     credential, bounded, cancellable); the reload above is its fallback.
//
// Same extract-verbatim-between-markers technique as
// shell_bridge_notifications.test.mjs. Run via `npm test` in
// crates/core/src/server (wired into the lint-assets CI job). Exits non-zero on
// any mismatch.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const assetPath = join(here, 'path_handlers/assets/shell_bridge.js');
const src = readFileSync(assetPath, 'utf8');

// Return the source from the first occurrence of `needle` (a `function` decl)
// up to the END marker — dropping the marker-comment prose that precedes it, so
// `new Function` parses only real code.
function extractFrom(begin, end, needle) {
  const b = src.indexOf(begin);
  const e = src.indexOf(end);
  if (b < 0 || e < 0 || e < b) {
    console.error(
      `FAIL: could not find ${begin}/${end} markers in ${assetPath}. The ` +
        'code must stay bracketed by those markers so this test can extract it.',
    );
    process.exit(1);
  }
  const region = src.slice(b, e);
  const fnStart = region.indexOf(needle);
  if (fnStart < 0) {
    console.error(`FAIL: no \`${needle}\` between the ${begin} markers.`);
    process.exit(1);
  }
  return region.slice(fnStart);
}

// --- Extract the pure functions verbatim ---------------------------------
const reloadUrlCapDecision = new Function(
  `${extractFrom('reload-url-cap:BEGIN', 'reload-url-cap:END', 'function reloadUrlCapDecision(')}\nreturn reloadUrlCapDecision;`,
)();

const { isTrustedStaleTokenClose, clampProxiedCloseCode } = new Function(
  `${extractFrom('close-recovery-decision:BEGIN', 'close-recovery-decision:END', 'function isTrustedStaleTokenClose(')}\nreturn { isTrustedStaleTokenClose, clampProxiedCloseCode };`,
)();

let failures = 0;
function check(name, cond) {
  if (cond) {
    console.log(`  ok   ${name}`);
  } else {
    console.error(`  FAIL ${name}`);
    failures++;
  }
}

// 1. (a) A SERVER-initiated 4401 close triggers recovery; (b) an
//    IFRAME-requested (clientClosed) 4401 close is IGNORED.
{
  check(
    'server-initiated 4401 close triggers recovery',
    isTrustedStaleTokenClose(4401, false) === true,
  );
  check(
    'iframe-initiated 4401 close is IGNORED (cannot forge the trigger)',
    isTrustedStaleTokenClose(4401, true) === false,
  );
  check(
    'a normal (1000) close does not trigger recovery',
    isTrustedStaleTokenClose(1000, false) === false,
  );
  check(
    'an abnormal (1006) close does not trigger recovery',
    isTrustedStaleTokenClose(1006, false) === false,
  );
}

// 2. The close proxy clamps app-range codes so an iframe can't even surface
//    4401 to onclose (second, independent guard).
{
  check('clamps 4401 -> 1000', clampProxiedCloseCode(4401) === 1000);
  check('clamps 4000 -> 1000', clampProxiedCloseCode(4000) === 1000);
  check('clamps 4999 -> 1000', clampProxiedCloseCode(4999) === 1000);
  check('leaves 1000 alone', clampProxiedCloseCode(1000) === 1000);
  check('leaves 3000 (non app-range) alone', clampProxiedCloseCode(3000) === 3000);
  check('leaves undefined alone', clampProxiedCloseCode(undefined) === undefined);
}

// 3. reloadUrlCapDecision: fail-CLOSED URL cap. No sessionStorage is touched —
//    the cap lives entirely in the `_freload` URL param — so it holds in
//    private/no-storage mode (the fail-open bug the old design had). Simulate a
//    reload loop by feeding each decision's URL back in at a fixed `now`.
{
  const NOW = 1_000_000;
  let href = 'https://node.example/v1/contract/web/KEY/';
  let allowed = 0;
  let refusedAt = -1;
  for (let i = 0; i < 6; i++) {
    const d = reloadUrlCapDecision(href, NOW);
    if (d.allow) {
      allowed++;
      href = d.url; // carry the incremented count forward, as location.replace would
    } else {
      refusedAt = i;
      break;
    }
  }
  check('cap allows exactly MAX=3 reloads in the window', allowed === 3);
  check('cap fails CLOSED on the 4th reload (loop bounded, no storage)', refusedAt === 3);
  check('the count is carried in the _freload URL param', /_freload=\d+-3/.test(href));
}

// 4. Rolling window: once the window elapses, a fresh budget is granted (so a
//    genuine later restart can still recover) — but only then.
{
  const start = 2_000_000;
  // A URL whose window is already maxed (count 3) started at `start`.
  const maxed = 'https://node.example/app/?_freload=' + start + '-3';
  const withinWindow = reloadUrlCapDecision(maxed, start + 59999);
  check('a maxed window within 60s is refused', withinWindow.allow === false);
  const afterWindow = reloadUrlCapDecision(maxed, start + 60000);
  check(
    'a reload whose window elapsed (>=60s) gets a fresh budget',
    afterWindow.allow === true,
  );
  check(
    'the fresh window restarts the count at 1',
    /_freload=\d+-1/.test(afterWindow.url),
  );
}

// 5. A malformed `_freload` (only reachable by a user hand-editing the URL,
//    never by the sandboxed contract) resets to a fresh window rather than
//    wedging — the loop bound still holds because each reload re-increments.
{
  const d = reloadUrlCapDecision(
    'https://node.example/app/?_freload=garbage',
    3_000_000,
  );
  check('malformed _freload resets to a fresh, allowed window', d.allow === true);
  check(
    'malformed _freload is replaced with a well-formed count',
    /_freload=\d+-1/.test(d.url),
  );
}

// 6. A FUTURE-dated window start (backward wall-clock correction) is not
//    trusted — it resets to a fresh window rather than wedging the cap.
{
  const d = reloadUrlCapDecision(
    'https://node.example/app/?_freload=99999999999999-3',
    3_500_000,
  );
  check('future-dated window start resets to a fresh, allowed window', d.allow === true);
}

// 7. freloadStripDecision: once the shell is served (the app link answered),
//    `_freload` is dropped from the address bar so a link copied from it later
//    is a FRESH link (a stale stamp sends a top-level open on a joining node
//    straight to the dashboard: connecting.html's expired-window branch). It
//    may only be dropped when doing so cannot change the cap above: a count of
//    0 (the connecting page's own stamp) at once, a live window only once it
//    has expired. Property: from the moment the decision says strip, for every
//    later time, the cap answers the stripped URL no more permissively than
//    the stamped one — never a fresh budget the stamped URL would refuse.
const freloadStripDecision = new Function(
  `${extractFrom('reload-url-cap:BEGIN', 'reload-url-cap:END', 'function reloadUrlCapDecision(')}\nreturn freloadStripDecision;`,
)();
{
  const base = 'http://n.test/v1/contract/web/k/';
  const T = 10_000_000;
  const countOf = (u) => Number(/_freload=\d+-(\d+)/.exec(u)[1]);
  let swept = 0;
  let deferred = 0;
  let violations = 0;
  let firstViolation = null;
  let maxDelay = 0;
  // One stamp: stripped (now or after its delay), every later reload decision
  // compared. A delay is re-decided when it fires (stripFreload), so a clamped
  // delay is followed until the decision says "now" (bounded: a stamp years
  // ahead re-decides every WINDOW_MS, so the walk is capped and counted).
  const sweep = (raw, label) => {
    const href = `${base}?view=chat&_freload=${raw}#m`;
    let at = T;
    let d = freloadStripDecision(href, at);
    if (!d) {
      violations++;
      return;
    }
    if (d.url !== `${base}?view=chat#m`) violations++;
    if (d.delay > 0) deferred++;
    for (let hops = 0; d.delay > 0 && hops < 3; hops++) {
      maxDelay = Math.max(maxDelay, d.delay);
      at += d.delay;
      d = freloadStripDecision(href, at);
    }
    maxDelay = Math.max(maxDelay, d.delay);
    // Every reload decision from the moment of stripping on: the stripped
    // URL is never allowed where the stamped one is refused, and never
    // carries a lower count.
    for (const later of [0, 1, 1_000, 30_000, 59_999, 60_000, 120_000]) {
      const t = at + d.delay + later;
      const a = reloadUrlCapDecision(href, t);
      const b = reloadUrlCapDecision(d.url, t);
      swept++;
      if (b.allow && !a.allow) {
        violations++;
        firstViolation ??= `${label}, stripped after ${at - T + d.delay} ms, reload at +${later} ms`;
      }
      const ca = /_freload=\d+-(\d+)/.test(a.url) ? countOf(a.url) : 0;
      const cb = /_freload=\d+-(\d+)/.test(b.url) ? countOf(b.url) : 0;
      if (a.allow && b.allow && cb < ca) violations++;
    }
  };
  for (const age of [
    0, 1, 5_000, 59_999, 60_000, 60_001, 3_600_000, -5_000, -120_000,
    // Far future (review of #5750): a hand-made link or a stamp written before
    // a large backwards clock correction — years ahead.
    -3 * 365 * 86_400_000,
  ]) {
    for (const count of [0, 1, 2, 3, 4]) sweep(`${T - age}-${count}`, `stamp age ${age} count ${count}`);
  }
  // Malformed stamps (review of #5750): each the cap already reads as fresh.
  for (const raw of ['abc', '5-abc', '', '-', `${T}-NaN`, `${T - 5_000}-`, 'NaN-2']) sweep(raw, `malformed ${JSON.stringify(raw)}`);
  check(
    `stripping never loosens the reload cap (${swept} cases${firstViolation ? `; first: ${firstViolation}` : ''})`,
    violations === 0 && swept > 200,
  );
  check(
    `a live window (count >= 1) is kept until it expires (${deferred} deferred)`,
    deferred > 0,
  );
  check(
    `no strip timer is longer than the window (longest ${maxDelay} ms)`,
    maxDelay <= 60_000,
  );
  check(
    'a stamp years in the future re-decides every window, never a delay past it',
    freloadStripDecision(`${base}?_freload=99999999999999-1`, T).delay === 60_000,
  );
  // ...and is still kept when the clock reaches it, through its whole window,
  // while the cap honours it (Codex on #5750: the sweep's three hops never got
  // there). Dropped only at its end, when the cap gives a fresh budget anyway.
  {
    const S = T + 3 * 365 * 86_400_000;
    const far = `${base}?_freload=${S}-3`;
    const keptThrough = freloadStripDecision(far, S - 1).delay > 0 && [S, S + 1, S + 30_000, S + 59_999].every(
      (t) => freloadStripDecision(far, t).delay > 0 && !reloadUrlCapDecision(far, t).allow,
    );
    check(
      'a far-future maxed stamp is kept through its own window (the cap refuses there)',
      keptThrough && freloadStripDecision(far, S + 60_000).delay === 0,
    );
  }
  // The two windows are one value: the strip is cap-safe only because it
  // expires a stamp exactly when the cap stops honouring it.
  {
    const windowOf = (fn) => {
      const at = src.indexOf(`function ${fn}(`);
      const m = /var WINDOW_MS = (\d+);/.exec(src.slice(at));
      return m && Number(m[1]);
    };
    const cap = windowOf('reloadUrlCapDecision');
    const strip = windowOf('freloadStripDecision');
    check(
      `freloadStripDecision's WINDOW_MS equals reloadUrlCapDecision's (${strip} vs ${cap})`,
      cap !== null && cap === strip,
    );
  }
  check(
    "the connecting page's own stamp (count 0) is dropped at once",
    freloadStripDecision(`${base}?_freload=${T - 5_000}-0`, T).delay === 0,
  );
  check(
    'a live cap window is dropped only when it expires',
    freloadStripDecision(`${base}?_freload=${T - 5_000}-2`, T).delay === 55_000,
  );
  check(
    'no _freload, nothing to do',
    freloadStripDecision(`${base}?view=chat`, T) === null,
  );
  check(
    'query and fragment survive',
    freloadStripDecision(`${base}?a=1&_freload=${T}-0&b=2#h`, T).url ===
      `${base}?a=1&b=2#h`,
  );
  // The app's own params keep their bytes (review of #5750): re-serialising
  // through URLSearchParams would turn `%20` into `+`, `~` into `%7E` and a
  // bare `flag` into `flag=` in the URL a person copies, and shell_page
  // forwards those bytes to the app unchanged.
  check(
    'the other params keep their exact bytes',
    freloadStripDecision(
      `${base}?sig=a~b&q=x%20y&flag&_freload=${T}-0&p=1%2B1#h`,
      T,
    ).url === `${base}?sig=a~b&q=x%20y&flag&p=1%2B1#h`,
  );
  check(
    'a pair starting with a literal ? is an app param, as the full query reads it',
    freloadStripDecision(`${base}??a=1&?_freload=x&_freload=${T}-0`, T).url ===
      `${base}??a=1&?_freload=x`,
  );
  check(
    'every _freload pair goes, an encoded name included',
    freloadStripDecision(`${base}?_fre%6Coad=1-0&a=1&_freload=2-0`, T).url ===
      `${base}?a=1`,
  );
  // THE CONTROL: stripping a live count-2 window at once WOULD loosen the
  // cap — it hands out a fresh budget of 3 where the stamped URL has 1 left.
  const live = `${base}?_freload=${T - 5_000}-3`;
  check(
    'control: an early strip of a maxed live window would be allowed where the cap refuses',
    reloadUrlCapDecision(live, T).allow === false &&
      reloadUrlCapDecision(base, T).allow === true,
  );
}

// 8. A same-contract navigation's frame path (iframe.src and the
//    history.state.iframePath Back/Forward restore) never carries the shell's
//    `_freload` stamp, whatever the encoding of its name, and keeps the app's
//    own query, the routing flag and the fragment (Codex on #5750).
{
  const sameContractFramePath = new Function(
    `${extractFrom('reload-url-cap:BEGIN', 'reload-url-cap:END', 'function reloadUrlCapDecision(')}\nreturn sameContractFramePath;`,
  )();
  const p = sameContractFramePath(
    'http://n.test/v1/contract/web/k/page?x=1&_freload=1000-0&_fre%6Coad=2-1&y=2',
    '#h',
  );
  check(
    `a same-contract frame path drops _freload (${p})`,
    !/_fre(load|%6Coad)/i.test(p),
  );
  check(
    'a same-contract frame path keeps the app query, __sandbox=1 and the hash',
    p === '/v1/contract/web/k/page?x=1&y=2&__sandbox=1#h',
  );
}

// 9. In-place recovery from a stale page credential (a node restart). On the
//    node's trusted 4401 the shell renews its credential with a same-origin
//    fetch of its own contract root and uses the fresh one on the app's NEXT
//    command socket: no top-level navigation, so the app's document survives.
//    The whole command-socket region runs here as shipped, with every free
//    name it uses (the bridge's closure variables and the browser globals)
//    supplied by this harness.
{
  const region = extractFrom(
    'command-socket-recovery:BEGIN',
    'command-socket-recovery:END',
    'var CREDENTIAL_RENEWAL_MAX',
  );
  const FREE_NAMES = [
    'authToken',
    'userToken',
    'contractPrefix',
    'location',
    'connections',
    'iframe',
    'sendToIframe',
    'isTrustedStaleTokenClose',
    'clampProxiedCloseCode',
    'triggerRecoveryReload',
    'WebSocket',
    'fetch',
    'AbortController',
    'setTimeout',
    'clearTimeout',
    'Date',
  ];
  const build = new Function(
    ...FREE_NAMES,
    `${region}\nreturn { openCommandSocket, closeCommandSocket, parseBearerToken, ` +
      'CREDENTIAL_RENEWAL_MAX, CREDENTIAL_RENEWAL_WINDOW_MS, CREDENTIAL_RENEWAL_TIMEOUT_MS };',
  );

  // Tokens shaped like the node's (base58 of 32 bytes).
  const tok = (c) => c.repeat(44);
  const T0 = tok('A');
  const T1 = tok('B');
  const T2 = tok('C');
  const T3 = tok('D');
  const PREFIX = '/v1/contract/web/KEY/';
  const WS_URL = 'ws://node.test/v1/contract/command?encodingProtocol=native';

  const flush = async () => {
    for (let i = 0; i < 20; i++) await Promise.resolve();
  };

  function harness(opts = {}) {
    const h = {
      now: 1_000_000,
      posted: [], // every message the shell sent to the app frame
      sockets: [], // every physical WebSocket the shell constructed
      fetches: [], // every renewal request
      fallbacks: 0, // top-level recovery reloads asked for
      aborted: 0,
      bodiesCancelled: 0,
      timers: [],
      connections: new Map(),
    };
    class FakeWebSocket {
      constructor(url, protocols) {
        this.url = url;
        this.protocols = protocols;
        this.readyState = 0;
        this.closeCalls = [];
        h.sockets.push(this);
      }
      close(code, reason) {
        this.closeCalls.push({ code, reason });
        this.readyState = 2;
      }
      // Drivers: what the node (or the network) does to this socket.
      accept() {
        this.readyState = 1;
        this.onopen({});
      }
      serverClose(code, reason = '') {
        this.readyState = 3;
        this.onclose({ code, reason });
      }
      networkFailure() {
        this.onerror({});
        this.serverClose(1006);
      }
    }
    FakeWebSocket.OPEN = 1;
    class FakeAbortController {
      constructor() {
        this.signal = { aborted: false };
      }
      abort() {
        this.signal.aborted = true;
        h.aborted++;
      }
    }
    const post = (msg) => h.posted.push(msg);
    h.api = build(
      opts.authToken ?? T0,
      opts.userToken,
      'contractPrefix' in opts ? opts.contractPrefix : PREFIX,
      { protocol: opts.protocol ?? 'http:' },
      h.connections,
      { contentWindow: { postMessage: post } },
      post,
      isTrustedStaleTokenClose,
      clampProxiedCloseCode,
      () => {
        h.fallbacks++;
      },
      FakeWebSocket,
      (url, init) =>
        new Promise((resolve, reject) => h.fetches.push({ url, init, resolve, reject })),
      FakeAbortController,
      (fn, ms) => {
        const t = { fn, at: h.now + ms, live: true };
        h.timers.push(t);
        return t;
      },
      (t) => {
        if (t) t.live = false;
      },
      { now: () => h.now },
    );
    let ids = 0;
    // The app asks for a socket (the open handler has validated the URL).
    h.open = (url = WS_URL) => {
      const id = `__fws_${++ids}`;
      const before = h.sockets.length;
      h.api.openCommandSocket({ id, url }, new URL(url));
      return { id, ws: h.sockets.length > before ? h.sockets[before] : null };
    };
    h.close = (id, code, reason) => h.api.closeCommandSocket({ id, code, reason });
    h.advance = (ms) => {
      h.now += ms;
      for (const t of h.timers.filter((x) => x.live && x.at <= h.now)) {
        t.live = false;
        t.fn();
      }
    };
    h.response = (token, over = {}) => ({
      ok: true,
      status: 200,
      redirected: false,
      headers: {
        get: (n) =>
          n.toLowerCase() === 'authorization' && token !== null ? `Bearer ${token}` : null,
      },
      body: {
        cancel: () => {
          h.bodiesCancelled++;
          return Promise.resolve();
        },
      },
      ...over,
    });
    // Answer the n-th renewal request with a fresh token.
    h.renew = async (n, token) => {
      h.fetches[n].resolve(h.response(token));
      await flush();
    };
    // One socket of the current credential, accepted then refused as stale.
    h.rejectStale = async () => {
      const s = h.open();
      s.ws.accept();
      s.ws.serverClose(4401, 'AUTH_TOKEN_INVALID');
      await flush();
      return s;
    };
    h.tokenOf = (ws) => new URL(ws.url).searchParams.get('authToken');
    h.messagesFor = (id) => h.posted.filter((m) => m.id === id).map((m) => m.type);
    return h;
  }

  // 9a. A successful renewal.
  {
    const h = harness();
    const first = h.open();
    check(
      'a command socket carries the page credential and the close-only opt-in',
      h.tokenOf(first.ws) === T0 &&
        new URL(first.ws.url).searchParams.getAll('invalidTokenReport').join() === 'close' &&
        new URL(first.ws.url).searchParams.get('encodingProtocol') === 'native',
    );
    first.ws.accept();
    first.ws.serverClose(4401, 'AUTH_TOKEN_INVALID');
    await flush();
    check(
      'the app sees the refused socket open and then close (its lifecycle is not hidden)',
      h.messagesFor(first.id).join() === 'open,close' &&
        h.posted.some((m) => m.type === 'close' && m.code === 4401),
    );
    const req = h.fetches[0];
    check(
      'a trusted 4401 starts ONE renewal of the contract root, and no navigation',
      h.fetches.length === 1 && req.url === PREFIX && h.fallbacks === 0,
    );
    check(
      'the renewal is a same-origin, uncached GET that rejects redirects and can be aborted',
      req.init.method === 'GET' &&
        req.init.credentials === 'same-origin' &&
        req.init.cache === 'no-store' &&
        req.init.redirect === 'error' &&
        req.init.signal &&
        req.init.signal.aborted === false,
    );
    await h.renew(0, T1);
    const next = h.open();
    check(
      "the app's next socket uses the fresh credential",
      next.ws !== null && h.tokenOf(next.ws) === T1,
    );
    check('the renewal response body is disposed of, not read', h.bodiesCancelled === 1);
    check(
      'the renewal timer is cleared once the response arrives',
      h.timers.every((t) => !t.live),
    );
    check('a successful renewal never asks for the top-level reload', h.fallbacks === 0);
    check(
      'no credential reaches the app frame',
      !JSON.stringify(h.posted).includes(T0) && !JSON.stringify(h.posted).includes(T1),
    );
    // The fresh credential works from here on: an ordinary close is ordinary.
    next.ws.accept();
    next.ws.serverClose(1000);
    await flush();
    check('a normal close afterwards starts nothing', h.fetches.length === 1 && h.fallbacks === 0);
  }

  // 9b. Simultaneous closes: one renewal, and opens asked for meanwhile wait.
  {
    const h = harness();
    const a = h.open();
    const b = h.open();
    a.ws.accept();
    b.ws.accept();
    a.ws.serverClose(4401);
    b.ws.serverClose(4401);
    await flush();
    check('two sockets refused at once start ONE renewal', h.fetches.length === 1);
    const c = h.open();
    const d = h.open();
    check(
      'an open asked for during the renewal waits for it (no socket with the stale credential)',
      c.ws === null && d.ws === null && h.sockets.length === 2,
    );
    check(
      'a waiting open holds its place in the connection limit',
      h.connections.size === 2 && h.connections.has(c.id) && h.connections.has(d.id),
    );
    await h.renew(0, T1);
    check(
      'the waiting opens connect with the fresh credential once it arrives',
      h.sockets.length === 4 && h.tokenOf(h.sockets[2]) === T1 && h.tokenOf(h.sockets[3]) === T1,
    );
    check(
      'each waiting open is now its real socket in the connection map',
      h.connections.get(c.id) === h.sockets[2] && h.connections.get(d.id) === h.sockets[3],
    );
    h.sockets[2].accept();
    check('a socket that waited opens for the app as usual', h.messagesFor(c.id).join() === 'open');
    check('one renewal, no reload', h.fetches.length === 1 && h.fallbacks === 0);
  }

  // 9c. A late close for a credential that has already been replaced.
  {
    const h = harness();
    const slow = h.open(); // opened with the old credential, refused late
    slow.ws.accept();
    await h.rejectStale();
    await h.renew(0, T1);
    slow.ws.serverClose(4401);
    await flush();
    check(
      'a late 4401 for the replaced credential starts no renewal and no reload',
      h.fetches.length === 1 && h.fallbacks === 0,
    );
    check('...and leaves the fresh credential in place', h.tokenOf(h.open().ws) === T1);
  }

  // 9d. Opens cancelled while they wait.
  {
    const h = harness();
    await h.rejectStale();
    const c = h.open();
    h.close(c.id, 1000, 'bye');
    check(
      'closing a waiting open ends it for the app and frees its slot',
      h.messagesFor(c.id).join() === 'close' && !h.connections.has(c.id),
    );
    // The shell dropping every connection (an in-place navigation does this).
    const e = h.open();
    h.connections.forEach((ws) => ws.close());
    h.connections.clear();
    await h.renew(0, T1);
    check(
      'an open cancelled during the renewal never reaches the node',
      h.sockets.length === 1 && h.messagesFor(c.id).join() === 'close',
    );
    check('...nor does one dropped with the connection map', h.messagesFor(e.id).length === 0);
    check('the renewal itself still completed', h.tokenOf(h.open().ws) === T1);
  }

  // 9e. Ordinary network failure: the app's own reconnect/backoff path.
  {
    const h = harness();
    let lifecycles = 0;
    for (let i = 0; i < 5; i++) {
      const s = h.open();
      s.ws.networkFailure();
      await flush();
      if (h.messagesFor(s.id).join() === 'error,close') lifecycles++;
    }
    check('a failed connection reaches the app as error then close', lifecycles === 5);
    check(
      'five 1006 closes (node down) start no renewal and no reload',
      h.fetches.length === 0 && h.fallbacks === 0 && h.connections.size === 0,
    );
    check(
      'every reconnect still uses the original credential',
      h.sockets.every((ws) => h.tokenOf(ws) === T0),
    );
  }

  // 9f. A client-requested close can never start a renewal.
  {
    const h = harness();
    const s = h.open();
    s.ws.accept();
    h.close(s.id, 4401, 'forged');
    check(
      'an app-range close code the app asks for is clamped',
      s.ws.closeCalls.length === 1 && s.ws.closeCalls[0].code === 1000,
    );
    s.ws.serverClose(4401);
    await flush();
    check(
      'a 4401 on a close the app asked for is not the trusted signal',
      h.fetches.length === 0 && h.fallbacks === 0,
    );
  }

  // 9g. Renewal that cannot produce a usable credential: the capped top-level
  //     reload stays as the fallback, exactly once per failure.
  {
    const failing = [
      ['a network error', (h) => h.fetches[0].reject(new TypeError('failed'))],
      ['a 500', (h) => h.fetches[0].resolve(h.response(T1, { ok: false, status: 500 }))],
      ['a redirected response', (h) => h.fetches[0].resolve(h.response(T1, { redirected: true }))],
      ['no Authorization header', (h) => h.fetches[0].resolve(h.response(null))],
      ['a malformed token', (h) => h.fetches[0].resolve(h.response('not a token!'))],
      ['the same credential again', (h) => h.fetches[0].resolve(h.response(T0))],
    ];
    for (const [label, fail] of failing) {
      const h = harness();
      await h.rejectStale();
      const waiting = h.open();
      fail(h);
      await flush();
      check(
        `${label}: falls back to the top-level reload once, keeps the old credential`,
        h.fallbacks === 1 && h.fetches.length === 1,
      );
      check(
        `${label}: an open that waited fails for the app without reaching the node`,
        h.messagesFor(waiting.id).join() === 'error,close' &&
          h.sockets.length === 1 &&
          !h.connections.has(waiting.id),
      );
    }
    // A stalled renewal is bounded by its timeout.
    const h = harness();
    await h.rejectStale();
    h.advance(h.api.CREDENTIAL_RENEWAL_TIMEOUT_MS - 1);
    await flush();
    check('a renewal still inside its timeout is left alone', h.fallbacks === 0);
    h.advance(1);
    await flush();
    check(
      'a stalled renewal is aborted at its timeout and falls back once',
      h.fallbacks === 1 && h.aborted === 1,
    );
    await h.renew(0, T1); // the stalled response arrives after all
    check(
      'a response arriving after the timeout installs nothing and reloads nothing more',
      h.fallbacks === 1 && h.tokenOf(h.open().ws) === T0,
    );
    // No contract prefix (the shell could not tell which root is its own).
    const n = harness({ contractPrefix: null });
    await n.rejectStale();
    check(
      'a shell that does not know its contract root makes no request and falls back',
      n.fetches.length === 0 && n.fallbacks === 1,
    );
  }

  // 9h. Successive rejected credentials are bounded, not only simultaneous
  //     sockets: a node that refuses every credential it has just issued.
  {
    const h = harness();
    const MAX = h.api.CREDENTIAL_RENEWAL_MAX;
    const fresh = [T1, T2, T3];
    for (let i = 0; i < MAX; i++) {
      await h.rejectStale();
      await h.renew(i, fresh[i]);
    }
    check(`${MAX} renewals in one window are allowed`, h.fetches.length === MAX && h.fallbacks === 0);
    await h.rejectStale();
    check(
      'the next rejection inside the window renews nothing and falls back to the capped reload',
      h.fetches.length === MAX && h.fallbacks === 1,
    );
    // Once the window has passed, a genuine restart can recover in place again.
    h.advance(h.api.CREDENTIAL_RENEWAL_WINDOW_MS);
    await h.rejectStale();
    check('a rejection after the window renews again', h.fetches.length === MAX + 1);
    check(
      'the renewal window is the reload cap window',
      h.api.CREDENTIAL_RENEWAL_WINDOW_MS === 60_000 && MAX === 3,
    );
  }

  // 9i. A second legitimate node restart later in the same tab.
  {
    const h = harness();
    await h.rejectStale();
    await h.renew(0, T1);
    const live = h.open();
    live.ws.accept();
    h.advance(10 * 60_000);
    live.ws.serverClose(1006); // the node goes down again
    const down = h.open();
    down.ws.networkFailure();
    await flush();
    check('the second outage itself starts nothing', h.fetches.length === 1);
    await h.rejectStale(); // the node is back and refuses T1
    check('the second restart starts a second renewal', h.fetches.length === 2);
    await h.renew(1, T2);
    check(
      'the second restart recovers in place too',
      h.tokenOf(h.open().ws) === T2 && h.fallbacks === 0,
    );
  }

  // 9j. The app never chooses a credential or the report mode.
  {
    const h = harness();
    const s = h.open(`${WS_URL}&authToken=evil&userToken=evil&invalidTokenReport=never`);
    const p = new URL(s.ws.url).searchParams;
    check(
      'caller-supplied authToken, userToken and invalidTokenReport are replaced',
      p.getAll('authToken').join() === T0 &&
        p.get('userToken') === null &&
        p.getAll('invalidTokenReport').join() === 'close',
    );
    const hosted = harness({ userToken: 'user-secret', protocol: 'https:' });
    check(
      'a hosted shell over https still presents its user token',
      new URL(hosted.open().ws.url).searchParams.get('userToken') === 'user-secret',
    );
    const plain = harness({ userToken: 'user-secret', protocol: 'http:' });
    check(
      'the user token is still withheld over plain http',
      new URL(plain.open().ws.url).searchParams.get('userToken') === null,
    );
  }

  // 9k. Only a well-formed bearer header yields a credential.
  {
    const { parseBearerToken } = harness().api;
    check('a base58 bearer token is accepted', parseBearerToken(`Bearer ${T1}`) === T1);
    const accepted = [
      null,
      undefined,
      '',
      T1,
      `bearer ${T1}`,
      `Bearer  ${T1}`,
      `Bearer ${T1} `,
      `Bearer ${T1}, Bearer ${T2}`,
      'Bearer short',
      `Bearer ${'A'.repeat(200)}`,
      `Bearer ${T1.slice(0, 43)}0`, // 0 is not in the base58 alphabet
      `Basic ${T1}`,
    ].filter((bad) => parseBearerToken(bad) !== null);
    check(
      `malformed Authorization headers yield no credential (accepted: ${JSON.stringify(accepted)})`,
      accepted.length === 0,
    );
  }
}

if (failures > 0) {
  console.error(`\nshell-bridge-reload: ${failures} check(s) FAILED`);
  process.exit(1);
}
console.log('shell-bridge-reload: all checks passed');
