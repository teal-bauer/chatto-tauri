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

function harness({ storageUnavailable = false, native = false } = {}) {
  const records = new Map();
  const intervals = new Map();
  const navigations = [];
  const opened = [];
  const invocations = [];
  let timerId = 0;
  let now = 1_000_000;
  const location = {
    get href() {
      return "https://origin.example/chat/remote/room";
    },
    set href(url) {
      navigations.push(url);
    },
    origin: "https://origin.example",
    hostname: "origin.example",
    replace: (url) => navigations.push(url),
  };
  const window = {
    location,
    open: (...args) => {
      opened.push(args);
      return null;
    },
    setInterval: (callback) => {
      intervals.set(++timerId, callback);
      return timerId;
    },
    clearInterval: (id) => intervals.delete(id),
    setTimeout: () => ++timerId,
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
    navigations,
    opened,
    invocations,
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
  assert.equal(
    target.searchParams.get("redirect_uri"),
    "https://origin.example/servers/callback",
  );
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
  assert.equal(
    navigated.searchParams.get("redirect_uri"),
    "https://origin.example/servers/callback",
  );
  assert.equal(h.intervals.size, 0);
});

test("about:blank flows support assignment to location.href", () => {
  const h = harness();
  const popup = h.window.open("about:blank", "chatto-oauth-state", "popup");
  assert.ok(popup);
  popup.location.href = authorizationUrl();
  assert.equal(
    new URL(h.navigations[0]).searchParams.get("redirect_uri"),
    "https://origin.example/servers/callback",
  );
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
