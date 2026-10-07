import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { runInNewContext } from "node:vm";

// Exercise the script injected into the server webview, not a separate copy.
const rust = readFileSync(
  new URL("../src-tauri/src/lib.rs", import.meta.url),
  "utf8",
);
const script = rust.match(
  /const EXTERNAL_LINK_JS: &str = r#"([\s\S]*?)"#;/,
)?.[1];
assert.ok(script);

const flowKey = "chatto:oauth:flow";
const returnKey = "chatto:tauri:oauth-return:v1";
const popupRedirect = "https://origin.example/servers/callback?mode=popup";

function savedFlow() {
  return {
    state: "csrf-state",
    verifier: "pkce-verifier",
    clientId: "https://origin.example/oauth/frontend-client-metadata.json",
    remoteUrl: "https://remote.example",
  };
}

function harness({
  storageUnavailable = false,
  native = false,
  url = "https://origin.example/chat/remote/room",
  sessionRecords = new Map([[flowKey, JSON.stringify(savedFlow())]]),
  fetchImpl = async () => new Response(null, { status: 200 }),
} = {}) {
  const records = new Map();
  const intervals = new Map();
  const timeouts = new Map();
  const navigations = [];
  const opened = [];
  const invocations = [];
  let timerId = 0;
  let now = 1_000_000;
  let currentUrl = new URL(url);
  const requests = [];
  const historyChanges = [];
  const location = {
    get href() {
      return currentUrl.href;
    },
    set href(url) {
      navigations.push(url);
    },
    get origin() {
      return currentUrl.origin;
    },
    get hostname() {
      return currentUrl.hostname;
    },
    replace: (url) => navigations.push(url),
  };
  const window = {
    location,
    history: {
      state: null,
      replaceState: (state, _title, target) => {
        window.history.state = state;
        currentUrl = new URL(target, currentUrl);
        historyChanges.push(currentUrl.href);
      },
    },
    sessionStorage: {
      getItem: (key) => sessionRecords.get(key) ?? null,
      setItem: (key, value) => sessionRecords.set(key, value),
      removeItem: (key) => sessionRecords.delete(key),
    },
    fetch: (input, init) => {
      requests.push({ input, init });
      return fetchImpl(input, init);
    },
    open: (...args) => {
      opened.push(args);
      return null;
    },
    setInterval: (callback) => {
      intervals.set(++timerId, callback);
      return timerId;
    },
    clearInterval: (id) => intervals.delete(id),
    setTimeout: (callback) => {
      timeouts.set(++timerId, callback);
      return timerId;
    },
    clearTimeout: (id) => timeouts.delete(id),
    localStorage: {
      getItem: (key) => {
        if (storageUnavailable) throw new Error("Storage unavailable");
        return records.get(key) ?? null;
      },
      removeItem: (key) => records.delete(key),
    },
  };
  if (native) {
    window.__TAURI_INTERNALS__ = {
      invoke: async (command, args) => {
        invocations.push({ command, args });
        return false;
      },
    };
  }
  runInNewContext(script, {
    window,
    document: { addEventListener() {} },
    URL,
    Date: { now: () => now },
  });
  return {
    window,
    records,
    intervals,
    timeouts,
    navigations,
    opened,
    invocations,
    requests,
    historyChanges,
    sessionRecords,
    poll: () => [...intervals.values()].forEach((callback) => callback()),
    advance: (ms) => {
      now += ms;
    },
    now: () => now,
  };
}

const launch = "https://origin.example/servers/authorize#launch-id";
const launchKey = "chatto:oauth-launch:launch-id";

function authorizationUrl() {
  const url = new URL("https://remote.example/oauth/authorize");
  url.search = new URLSearchParams({
    response_type: "code",
    client_id: "https://origin.example/oauth/frontend-client-metadata.json",
    redirect_uri: "https://origin.example/servers/callback?mode=popup",
    state: "csrf-state",
    code_challenge: "pkce-challenge",
    code_challenge_method: "S256",
  }).toString();
  return url.href;
}

test("launch-page sign-in returns a window and follows its storage record in the main webview", () => {
  const h = harness();
  const popup = h.window.open(launch, "chatto-oauth-state", "popup,width=560");
  assert.ok(popup, "sign-in must not fail as a blocked popup");
  popup.opener = null;
  h.poll();
  assert.deepEqual(h.navigations, []);
  assert.deepEqual(h.opened, []);

  // Chatto saves the PKCE flow in sessionStorage, then publishes the launch URL.
  h.records.set(
    launchKey,
    JSON.stringify({ url: authorizationUrl(), createdAt: h.now() }),
  );
  h.poll();
  assert.equal(h.navigations.length, 1);
  const target = new URL(h.navigations[0]);
  assert.equal(target.origin, "https://remote.example");
  assert.equal(target.searchParams.get("redirect_uri"), popupRedirect);
  assert.equal(target.searchParams.get("state"), "csrf-state");
  assert.equal(target.searchParams.get("code_challenge"), "pkce-challenge");
  assert.equal(
    target.searchParams.get("client_id"),
    "https://origin.example/oauth/frontend-client-metadata.json",
  );
  assert.equal(h.records.has(launchKey), false);
  assert.equal(h.intervals.size, 0);
});

test("storage fallback supports location.replace without changing unrelated query values", () => {
  const h = harness({ storageUnavailable: true });
  const popup = h.window.open(launch, "chatto-oauth-state", "popup");
  assert.ok(popup);
  const target = new URL(authorizationUrl());
  target.searchParams.set("other", "literal?mode=popup");
  popup.location.replace(target.href);
  const navigated = new URL(h.navigations[0]);
  assert.equal(navigated.searchParams.get("other"), "literal?mode=popup");
  assert.equal(navigated.searchParams.get("redirect_uri"), popupRedirect);
  assert.equal(h.intervals.size, 0);
});

test("about:blank flows support assignment to location.href", () => {
  const h = harness();
  const popup = h.window.open("about:blank", "chatto-oauth-state", "popup");
  assert.ok(popup);
  popup.location.href = authorizationUrl();
  assert.equal(
    new URL(h.navigations[0]).searchParams.get("redirect_uri"),
    popupRedirect,
  );
});

function returnedFlow(options = {}) {
  const opening = harness();
  opening.window
    .open(launch, "chatto-oauth-state", "popup")
    .location.replace(authorizationUrl());
  const callback = harness({
    url: popupRedirect + "&code=issued-code&state=csrf-state",
    sessionRecords: opening.sessionRecords,
    ...options,
  });
  return { opening, callback };
}

function exchangeBody(extra = {}) {
  return {
    grant_type: "authorization_code",
    code: "issued-code",
    code_verifier: savedFlow().verifier,
    client_id: savedFlow().clientId,
    redirect_uri: "https://origin.example/servers/callback",
    ...extra,
  };
}

test("authorization and code exchange use the same exact registered redirect", async () => {
  const registeredRedirects = [popupRedirect];
  const { opening, callback } = returnedFlow({
    fetchImpl: async (_input, init) => {
      const body = JSON.parse(init.body);
      return Response.json(
        registeredRedirects.includes(body.redirect_uri)
          ? {
              access_token: "fixture-access-token",
              refresh_token: "fixture-refresh-token",
            }
          : {
              error: "invalid_request",
              error_description:
                "redirect_uri is not registered for this client",
            },
        { status: registeredRedirects.includes(body.redirect_uri) ? 200 : 400 },
      );
    },
  });
  const authorizedRedirect = new URL(opening.navigations[0]).searchParams.get(
    "redirect_uri",
  );
  assert.ok(registeredRedirects.includes(authorizedRedirect));
  assert.equal(
    new URL(callback.window.location.href).searchParams.has("mode"),
    false,
  );
  assert.equal(
    new URL(callback.window.location.href).searchParams.get("code"),
    "issued-code",
  );
  assert.equal(callback.sessionRecords.has(returnKey), false);

  // The frontend consumes its PKCE state before calling the token endpoint.
  const flow = JSON.parse(callback.sessionRecords.get(flowKey));
  callback.sessionRecords.delete(flowKey);
  const signal = new AbortController().signal;
  const headers = { "Content-Type": "application/json" };
  const response = await callback.window.fetch(
    flow.remoteUrl + "/oauth/token",
    {
      method: "POST",
      headers,
      signal,
      body: JSON.stringify(exchangeBody()),
    },
  );
  assert.equal(response.status, 200);
  assert.equal(
    JSON.parse(callback.requests[0].init.body).redirect_uri,
    authorizedRedirect,
  );
  assert.equal(callback.requests[0].init.headers, headers);
  assert.equal(callback.requests[0].init.signal, signal);
  assert.equal((await response.json()).access_token, "fixture-access-token");
});

test("the callback adapter affects only one matching authorization-code exchange", async () => {
  const { callback } = returnedFlow();
  const original = callback.window.fetch;
  for (const [input, body] of [
    ["https://other.example/oauth/token", exchangeBody()],
    [
      "https://remote.example/oauth/token",
      exchangeBody({ grant_type: "refresh_token" }),
    ],
    [
      "https://remote.example/oauth/token",
      exchangeBody({ client_id: "other-client" }),
    ],
    [
      "https://remote.example/oauth/token",
      exchangeBody({ code_verifier: "other-verifier" }),
    ],
    [
      "https://remote.example/oauth/token",
      exchangeBody({ code: "other-code" }),
    ],
    [
      "https://remote.example/oauth/token",
      exchangeBody({ redirect_uri: "https://other.example/servers/callback" }),
    ],
  ]) {
    const init = { method: "POST", body: JSON.stringify(body) };
    await callback.window.fetch(input, init);
    assert.equal(callback.requests.at(-1).init, init);
    assert.equal(callback.window.fetch, original);
  }
  await callback.window.fetch("https://remote.example/oauth/token", {
    method: "POST",
    body: JSON.stringify(exchangeBody()),
  });
  assert.notEqual(callback.window.fetch, original);
  const second = { method: "POST", body: JSON.stringify(exchangeBody()) };
  await callback.window.fetch("https://remote.example/oauth/token", second);
  assert.equal(callback.requests.at(-1).init, second);
});

for (const [name, query] of [
  ["wrong state", "mode=popup&code=issued-code&state=other-state"],
  [
    "duplicate state",
    "mode=popup&code=issued-code&state=csrf-state&state=csrf-state",
  ],
  [
    "duplicate code",
    "mode=popup&code=issued-code&code=another-code&state=csrf-state",
  ],
  [
    "code with error",
    "mode=popup&code=issued-code&error=access_denied&state=csrf-state",
  ],
]) {
  test(`does not adapt a callback with ${name}`, () => {
    const { callback } = returnedFlow({
      url: "https://origin.example/servers/callback?" + query,
    });
    assert.equal(callback.historyChanges.length, 0);
    assert.equal(callback.sessionRecords.has(returnKey), false);
    assert.equal(callback.sessionRecords.has(flowKey), true);
  });
}

test("does not adapt a callback without a recorded main-window flow", () => {
  const callback = harness({
    url: popupRedirect + "&code=issued-code&state=csrf-state",
  });
  assert.equal(callback.historyChanges.length, 0);
});

test("does not adapt an expired or altered return record", () => {
  for (const patch of [
    { createdAt: 0 },
    { clientId: "other-client" },
    { remoteUrl: "https://other.example" },
    { redirectUri: "https://other.example/servers/callback?mode=popup" },
  ]) {
    const opening = harness();
    opening.window
      .open(launch, "chatto-oauth-state", "popup")
      .location.replace(authorizationUrl());
    const record = JSON.parse(opening.sessionRecords.get(returnKey));
    opening.sessionRecords.set(
      returnKey,
      JSON.stringify({ ...record, ...patch }),
    );
    const callback = harness({
      url: popupRedirect + "&code=issued-code&state=csrf-state",
      sessionRecords: opening.sessionRecords,
    });
    assert.equal(callback.historyChanges.length, 0);
  }
});

test("an unused callback exchange adapter expires", async () => {
  const { callback } = returnedFlow();
  const adapted = callback.window.fetch;
  for (const expire of callback.timeouts.values()) expire();
  assert.notEqual(callback.window.fetch, adapted);
  const init = { method: "POST", body: JSON.stringify(exchangeBody()) };
  await callback.window.fetch("https://remote.example/oauth/token", init);
  assert.equal(callback.requests[0].init, init);
});

test("an authorization denial uses the full-page error path without an exchange adapter", () => {
  const { callback } = returnedFlow({
    url: popupRedirect + "&error=access_denied&state=csrf-state",
  });
  const url = new URL(callback.window.location.href);
  assert.equal(url.searchParams.has("mode"), false);
  assert.equal(url.searchParams.get("error"), "access_denied");
  assert.equal(callback.requests.length, 0);
});

test("sign-in cannot navigate when its return record cannot be persisted", () => {
  const opening = harness();
  opening.window.sessionStorage.setItem = () => {};
  const popup = opening.window.open(launch, "chatto-oauth-state", "popup");
  assert.throws(
    () => popup.location.replace(authorizationUrl()),
    /could not be saved/,
  );
  assert.equal(opening.navigations.length, 0);
});

test("close cancels a pending launch and removes its record", () => {
  const h = harness();
  const popup = h.window.open(launch, "chatto-oauth-state", "popup");
  assert.ok(popup);
  h.records.set(
    launchKey,
    JSON.stringify({ url: authorizationUrl(), createdAt: h.now() }),
  );
  popup.close();
  h.poll();
  assert.equal(popup.closed, true);
  assert.equal(h.intervals.size, 0);
  assert.equal(h.records.has(launchKey), false);
  assert.deepEqual(h.navigations, []);
});

for (const [label, record] of [
  ["expired", { url: authorizationUrl(), createdAt: 0 }],
  ["non-HTTP", { url: "javascript:alert(1)", createdAt: 1_000_000 }],
  ["invalid", { createdAt: 1_000_000 }],
]) {
  test(`rejects ${label} launch records`, () => {
    const h = harness();
    const popup = h.window.open(launch, "chatto-oauth-state", "popup");
    assert.ok(popup);
    h.records.set(launchKey, JSON.stringify(record));
    h.poll();
    assert.equal(popup.closed, true);
    assert.equal(h.records.has(launchKey), false);
    assert.equal(h.intervals.size, 0);
    assert.deepEqual(h.navigations, []);
  });
}

test("a launch with no record times out", () => {
  const h = harness();
  const popup = h.window.open(launch, "chatto-oauth-state", "popup");
  assert.ok(popup);
  h.advance(5 * 60 * 1000 + 1);
  h.poll();
  assert.equal(popup.closed, true);
  assert.equal(h.intervals.size, 0);
});

test("native initialization keeps authorization internal and ordinary external windows in the browser", async () => {
  const h = harness({ native: true });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(h.invocations[0].command, "check_instance_flow");
  assert.ok(h.window.open(launch, "chatto-oauth-state", "popup"));
  assert.equal(h.invocations.length, 1);
  assert.equal(h.window.open("https://other.example/article", "_blank"), null);
  assert.equal(h.invocations[1].command, "plugin:opener|open_url");
  assert.equal(h.invocations[1].args.url, "https://other.example/article");
});

test("provider callbacks retain their mode for the cookie-session return path", () => {
  const h = harness();
  const popup = h.window.open(launch, "chatto-oauth-state", "popup");
  assert.ok(popup);
  const url = new URL(authorizationUrl());
  url.searchParams.set(
    "redirect_uri",
    "https://origin.example/servers/callback?mode=provider",
  );
  popup.location.replace(url.href);
  assert.equal(
    new URL(h.navigations[0]).searchParams.get("redirect_uri"),
    "https://origin.example/servers/callback?mode=provider",
  );
});

test("only same-origin launch-page popups use the main-window fallback", () => {
  const h = harness();
  for (const url of [
    "https://origin.example/unrelated#launch-id",
    "https://origin.example/servers/authorize",
    "https://origin.example/servers/authorize?unexpected=1#launch-id",
  ]) {
    assert.equal(h.window.open(url, "_blank", "popup"), null);
  }
  assert.equal(h.window.open(launch, "_blank", ""), null);
  assert.equal(
    h.window.open(
      "https://foreign.example/servers/authorize#launch-id",
      "_blank",
      "popup",
    ),
    null,
  );
  assert.equal(h.opened.length, 4);
  assert.equal(h.intervals.size, 0);
});
