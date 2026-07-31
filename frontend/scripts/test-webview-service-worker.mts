#!/usr/bin/env node

/** Runs unit checks for the Rings WebView service-worker request classifier. */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { MessageChannel } from "node:worker_threads";

import { verifyWebviewHostAsset } from "./test-webview-host.mjs";
import {
  assertJsonEqual,
  bytes,
  captureTimeoutCallbacks,
  frontendProjectRoot,
  gatewayFetchEvent,
  request,
  runtimeGatewayFetchEvent,
  type ServiceWorkerClientFixture,
  type ServiceWorkerMessageEventFixture,
  type ServiceWorkerTestContext,
  text,
} from "./webview-service-worker-fixtures.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const hostAssetPath = resolve(projectRoot, "assets", "webview-host.js");
const workerResponseAssetPath = resolve(projectRoot, "assets", "webview-worker-response.js");
const serviceWorkerPath = resolve(projectRoot, "rings-webview-service-worker.js");
const hostAssetSource = await readFile(hostAssetPath, "utf8");
const workerResponseAssetSource = await readFile(workerResponseAssetPath, "utf8");
const serviceWorkerSource = await readFile(serviceWorkerPath, "utf8");
const clientsById = new Map<string, ServiceWorkerClientFixture>();
const messageListeners: Array<(event: ServiceWorkerMessageEventFixture) => void> = [];
let context: ServiceWorkerTestContext;
context = {
  console,
  AbortController,
  Headers,
  ArrayBuffer,
  URL,
  TextDecoder,
  TextEncoder,
  Response,
  Uint8Array,
  MessageChannel,
  performance,
  setTimeout,
  clearTimeout,
  importScripts(...urls) {
    for (const url of urls) {
      assert.equal(new URL(url, "http://127.0.0.1:8080/").pathname, "/assets/webview-worker-response.js");
      vm.runInContext(workerResponseAssetSource, context, {
        filename: workerResponseAssetPath,
      });
    }
  },
  self: {
    location: new URL("http://127.0.0.1:8080/"),
    addEventListener(type, listener) {
      if (type === "message") {
        messageListeners.push(listener);
      }
    },
    clients: {
      get: async (clientId) => clientsById.get(clientId),
      matchAll: async () => [...clientsById.values()],
    },
  },
};
const globalThisKey = "globalThis";
context[globalThisKey] = context;
vm.createContext(context);

vm.runInContext(
  `${serviceWorkerSource}\nglobalThis.__ringsWebviewServiceWorkerTest = { controlledNavigationBody, emitDebug, gatewayFailureDocument, gatewayHostClient, handleGatewayFetch, handleGatewayFetchWithTimeout, rememberNavigationClientTarget, rememberClientSourceTargetForTest, rememberTrustedShellClientForTest, registerDebugClient, registerGatewayHostClient, requestGatewayResponse, resetGatewayHostForTest, requestKind, sourceTargetForClient };`,
  context,
  {
    filename: serviceWorkerPath,
  },
);

const serviceWorkerApi = context.__ringsWebviewServiceWorkerTest;
assert(serviceWorkerApi, "service worker test API was not exported");
const {
  controlledNavigationBody,
  emitDebug,
  gatewayFailureDocument,
  gatewayHostClient,
  handleGatewayFetch,
  handleGatewayFetchWithTimeout,
  rememberNavigationClientTarget,
  rememberClientSourceTargetForTest,
  rememberTrustedShellClientForTest,
  registerDebugClient,
  registerGatewayHostClient,
  requestGatewayResponse,
  resetGatewayHostForTest,
  requestKind,
  sourceTargetForClient,
} = serviceWorkerApi;

/**
 * Runs the injected history guard in a small browser-like VM.
 */
function runHistoryGuard(html: string, locationHref: string): unknown[][] {
  const script = html.match(/<script data-rings-webview-history-guard>([\s\S]*?)<\/script>/)?.[1];
  assert(script, "history guard script was not injected");
  const calls: unknown[][] = [];
  class HistoryFixture {
    pushState(...args: unknown[]): void {
      calls.push(["pushState", ...args]);
    }

    replaceState(...args: unknown[]): void {
      calls.push(["replaceState", ...args]);
    }
  }
  const historyContext: Record<string, unknown> = {
    calls,
    DOMException,
    History: HistoryFixture,
    history: new HistoryFixture(),
    location: new URL(locationHref),
    Object,
    Reflect,
    URL,
  };
  historyContext[globalThisKey] = historyContext;
  vm.runInNewContext(script, historyContext, {
    filename: "rings-webview-history-guard.js",
  });
  vm.runInNewContext(
    `
      history.pushState({ page: "search" }, "", "/search?q=test");
      history.replaceState({ page: "hash" }, "", "/#node");
    `,
    historyContext,
    {
      filename: "rings-webview-history-guard-fixture.js",
    },
  );
  return calls;
}

/**
 * Delivers one synthetic message event to every service-worker message listener.
 */
async function dispatchMessage(clientId: string, data: unknown): Promise<unknown[]> {
  const responses: unknown[] = [];
  const waits: Array<Promise<unknown>> = [];
  const event: ServiceWorkerMessageEventFixture = {
    source: { id: clientId },
    data,
    ports: [
      {
        postMessage(message) {
          responses.push(message);
        },
      },
    ],
    waitUntil(promise) {
      waits.push(promise);
    },
  };
  for (const listener of messageListeners) {
    listener(event);
  }
  await Promise.all(waits);
  return responses;
}

await verifyWebviewHostAsset(hostAssetSource, hostAssetPath);

assert.equal(requestKind(request({ mode: "navigate", destination: "document" })), "navigation");
assert.equal(requestKind(request({ destination: "style" })), "subresource");
assert.equal(requestKind(request()), "fetch");
assert.equal(requestKind(request({ headers: { "X-Rings-Webview-Kind": "fetch" } })), "fetch");
assert.equal(requestKind(request({ headers: { "X-Rings-Webview-Kind": "xhr" } })), "xhr");
assert.throws(
  () => requestKind(request({ headers: { "X-Rings-Webview-Kind": "xhr, subresource" } })),
  /invalid X-Rings-Webview-Kind/,
);
assert.throws(
  () => requestKind(request({ headers: { "X-Rings-Webview-Kind": "subresource" } })),
  /invalid X-Rings-Webview-Kind/,
);
assert.throws(
  () => requestKind(request({ headers: { "X-Rings-Webview-Kind": "xhr, xhr" } })),
  /invalid X-Rings-Webview-Kind/,
);

{
  const headers = new Headers({
    "content-encoding": "gzip",
    "content-length": "42",
    "content-security-policy": "default-src 'none'",
    "content-security-policy-report-only": "default-src 'none'",
    "content-type": "text/html; charset=utf-8",
    "x-frame-options": "DENY",
  });
  const body = controlledNavigationBody(
    { kind: "navigation" },
    200,
    headers,
    bytes("<!doctype html><html><head><title>Target</title></head><body>ok</body></html>"),
  );
  const html = text(body);
  assert.match(html, /data-rings-webview-history-guard/);
  assert.match(html, /<script src="\/assets\/webview-overlay\.js"><\/script><\/head>/);
  assert.equal(headers.has("content-length"), false);
  assert.equal(headers.has("content-encoding"), false);
  assert.equal(headers.has("content-security-policy-report-only"), false);
  assert.equal(headers.has("x-frame-options"), false);
  assert.match(headers.get("content-security-policy") ?? "", /script-src 'self'/);
}

{
  const headers = new Headers({
    "content-length": "42",
    "content-type": "text/html",
  });
  const body = controlledNavigationBody(
    { kind: "navigation", topLevelNavigation: false },
    200,
    headers,
    bytes("<!doctype html><html><head><title>Frame</title></head><body>ok</body></html>"),
  );
  const html = text(body);
  assert.match(html, /data-rings-webview-history-guard/);
  assert.doesNotMatch(html, /\/assets\/webview-overlay\.js/);
  assert.equal(headers.has("content-length"), false);
  assert.match(headers.get("content-security-policy") ?? "", /script-src 'self'/);
}

{
  const html =
    '<!doctype html><html><head><script src="/assets/webview-overlay.js"></script></head><body>ok</body></html>';
  const headers = new Headers({
    "content-length": "42",
    "content-security-policy": "default-src 'none'",
    "content-type": "text/html",
  });
  const body = controlledNavigationBody({ kind: "navigation" }, 200, headers, bytes(html));
  const injected = text(body);
  assert.match(injected, /data-rings-webview-history-guard/);
  assert.match(injected, /<script src="\/assets\/webview-overlay\.js"><\/script><\/head>/);
  assert.equal(headers.has("content-length"), false);
  assert.match(headers.get("content-security-policy") ?? "", /script-src 'self'/);
}

{
  const headers = new Headers({
    "content-length": "42",
    "content-security-policy": "default-src 'none'",
    "content-type": "text/html",
  });
  const body = controlledNavigationBody(
    { kind: "navigation" },
    200,
    headers,
    bytes(
      "<!doctype html><!-- attacker marker: data-rings-webview-history-guard /assets/webview-overlay.js --><html><head><title>Target</title></head><body>ok</body></html>",
    ),
  );
  const html = text(body);
  const guardIndex = html.indexOf("<script data-rings-webview-history-guard>");
  const attackerMarkerIndex = html.indexOf("attacker marker");
  const overlayIndex = html.lastIndexOf('<script src="/assets/webview-overlay.js"></script>');
  assert.ok(guardIndex >= 0);
  assert.ok(attackerMarkerIndex >= 0);
  assert.ok(overlayIndex > attackerMarkerIndex);
  const historyCalls = runHistoryGuard(
    html,
    "http://127.0.0.1:8080/webview/https%3A%2F%2Ftrusted.example%2Fdocs%2Findex.html",
  );
  assert.equal(historyCalls[0]?.[3], "/webview/https%3A%2F%2Ftrusted.example%2Fsearch%3Fq%3Dtest");
  assert.equal(headers.has("content-length"), false);
}

{
  const headers = new Headers({
    "content-length": "42",
    "content-security-policy": "default-src 'none'",
    "content-type": "text/html",
  });
  const body = controlledNavigationBody(
    { kind: "navigation" },
    200,
    headers,
    bytes("\uFEFF<!-- leading comment --><html><head><title>Target</title></head><body>ok</body></html>"),
  );
  const html = text(body);
  assert.match(html, /<script src="\/assets\/webview-overlay\.js"><\/script><\/head>/);
  assert.equal(headers.has("content-length"), false);
  assert.match(headers.get("content-security-policy") ?? "", /script-src 'self'/);
}

{
  const headers = new Headers({
    "content-length": "42",
    "content-security-policy": "default-src 'none'",
    "content-type": "text/html",
  });
  const body = controlledNavigationBody({ kind: "navigation" }, 200, headers, bytes("<!-- comment-only fixture -->"));
  assert.equal(text(body), "<!-- comment-only fixture -->");
  assert.equal(headers.has("content-length"), false);
  assert.match(headers.get("content-security-policy") ?? "", /script-src 'self'/);
}

{
  const headers = new Headers({
    "content-type": "text/html",
  });
  const body = controlledNavigationBody(
    { kind: "navigation" },
    200,
    headers,
    bytes(
      '<!doctype html><html><head><script data-attacker>history.replaceState(null, "", "/#node")</script></head><body>ok</body></html>',
    ),
  );
  const html = text(body);
  const guardIndex = html.indexOf("data-rings-webview-history-guard");
  const attackerIndex = html.indexOf("data-attacker");
  assert.ok(guardIndex >= 0);
  assert.ok(attackerIndex >= 0);
  assert.ok(guardIndex < attackerIndex);
  const historyCalls = runHistoryGuard(
    html,
    "http://127.0.0.1:8080/webview/https%3A%2F%2Ftrusted.example%2Fdocs%2Findex.html",
  );
  assert.equal(historyCalls[0]?.[0], "pushState");
  assert.equal(historyCalls[0]?.[3], "/webview/https%3A%2F%2Ftrusted.example%2Fsearch%3Fq%3Dtest");
  assert.equal(historyCalls[1]?.[0], "replaceState");
  assert.equal(historyCalls[1]?.[3], "/webview/https%3A%2F%2Ftrusted.example%2F%23node");
}

{
  const css = bytes("body { color: red; }");
  const body = controlledNavigationBody({ kind: "subresource" }, 200, new Headers({ "content-type": "text/css" }), css);
  assert.equal(body, css);
}

{
  const html = gatewayFailureDocument(
    503,
    'gateway transport failed: no live onion exit offers service "https"',
    "No live HTTPS onion exit is available.",
    "onion_exit_unavailable",
  );
  assert.match(html, /<template[\s\S]*data-rings-webview-failure="true"/);
  assert.match(html, /data-rings-webview-failure-code="onion_exit_unavailable"/);
  assert.doesNotMatch(html, /<h1\b/i);
  assert.doesNotMatch(html, /<main\b/i);
  assert.doesNotMatch(html, /<p\b/i);
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const hostCapability = "h".repeat(32);
  const hostRequests: unknown[] = [];
  clientsById.set("host", {
    id: "host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message, transfer) {
      hostRequests.push(message);
      const reply = transfer?.[0] as { postMessage?: (response: unknown) => void } | undefined;
      reply?.postMessage?.({
        ok: true,
        status: 200,
        headers: [{ name: "content-type", value: "text/html" }],
        body: bytes("<!doctype html><html><head><title>OK</title></head><body>ok</body></html>"),
      });
    },
  });
  clientsById.set("hung-debug", {
    id: "hung-debug",
    url: "http://127.0.0.1:8080/#webview",
    frameType: "auxiliary",
    postMessage() {},
  });
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  assert.equal(rememberTrustedShellClientForTest("hung-debug"), true);
  assert.equal(await registerDebugClient("hung-debug", hostCapability), true);

  const originalGet = context.self.clients.get;
  context.self.clients.get = (clientId) => {
    if (clientId === "hung-debug") {
      return new Promise<ServiceWorkerClientFixture | undefined>(() => {});
    }
    return originalGet(clientId);
  };
  try {
    const response = await handleGatewayFetch(gatewayFetchEvent("https://example.test/"), 700, performance.now());
    assert.equal(response.status, 200);
    assert.equal(hostRequests.length, 1);
    const html = await response.text();
    assert.match(html, /data-rings-webview-history-guard/);
    assert.match(html, /\/assets\/webview-overlay\.js/);
  } finally {
    context.self.clients.get = originalGet;
  }
}

{
  // A reclaimed worker has no durable popup-to-host proof. It must not bind a
  // gateway request to any currently open #node page merely to recover state.
  resetGatewayHostForTest();
  clientsById.clear();
  const hostMessages: unknown[] = [];
  clientsById.set("unassociated-host", {
    id: "unassociated-host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message) {
      hostMessages.push(message);
    },
  });

  assert.equal(await gatewayHostClient(), undefined);
  assertJsonEqual(hostMessages, []);
}

{
  const timers = captureTimeoutCallbacks(context);
  const messages: unknown[] = [];
  const host: ServiceWorkerClientFixture = {
    id: "host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message) {
      messages.push(message);
    },
  };
  try {
    const responsePromise = requestGatewayResponse(host, { kind: "navigation", method: "GET" }, 702);
    assertJsonEqual(messages, [
      {
        type: "rings-webview-gateway-request",
        requestId: 702,
        request: { kind: "navigation", method: "GET" },
      },
    ]);
    const timeoutCallback = timers.callbacks[0];
    assert(timeoutCallback, "gateway response timeout was not scheduled");
    timeoutCallback();
    const response = await responsePromise;
    assert.equal(response.status, 504);
    assert.equal(response.errorCode, "local_gateway_timeout");
    assertJsonEqual(messages[1], {
      type: "rings-webview-gateway-cancel",
      requestId: 702,
    });
  } finally {
    timers.restore();
  }
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const timers = captureTimeoutCallbacks(context);
  const hostMessages: unknown[] = [];
  const hostCapability = "u".repeat(32);
  clientsById.set("host", {
    id: "host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message) {
      hostMessages.push(message);
    },
  });
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  try {
    const responsePromise = handleGatewayFetchWithTimeout(gatewayFetchEvent("https://dispatched-timeout.example/"));
    while (hostMessages.length === 0) {
      await Promise.resolve();
    }
    assert.equal(timers.callbacks.length, 2);
    const deadline = timers.callbacks[0];
    assert(deadline, "Service Worker deadline was not scheduled");
    deadline();
    const response = await responsePromise;
    assert.equal(response.status, 504);
    assertJsonEqual(
      hostMessages.map((message) => (message as { readonly type?: string }).type),
      ["rings-webview-gateway-request", "rings-webview-gateway-cancel"],
    );
  } finally {
    timers.restore();
  }
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const hostCapability = "r".repeat(32);
  const longTarget = `https://example.test/async/hpba?payload=${"x".repeat(10_000)}`;
  const hostRequests: unknown[] = [];
  clientsById.set("host", {
    id: "host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message, transfer) {
      hostRequests.push(message);
      const reply = transfer?.[0] as { postMessage?: (response: unknown) => void } | undefined;
      reply?.postMessage?.({
        ok: true,
        status: 204,
        headers: [],
        body: null,
      });
    },
  });
  clientsById.set("target-page", {
    id: "target-page",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Fexample.test%2F",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  assert.equal(rememberClientSourceTargetForTest("target-page", "https://example.test/"), true);

  const response = await handleGatewayFetch(runtimeGatewayFetchEvent(longTarget), 701, performance.now());
  assert.equal(response.status, 204);
  assert.equal(hostRequests.length, 1);
  const requestMessage = hostRequests[0] as {
    readonly request?: {
      readonly body?: ArrayBuffer;
      readonly credentials?: string;
      readonly headers?: Array<{ readonly name: string; readonly value: string }>;
      readonly kind?: string;
      readonly method?: string;
      readonly requested?: string;
      readonly sourceTarget?: string;
    };
  };
  const request = requestMessage.request;
  assert(request, "host did not receive a gateway request");
  assert.equal(request.requested, `http://127.0.0.1:8080/webview/${encodeURIComponent(longTarget)}`);
  assert.equal(request.sourceTarget, "https://example.test/");
  assert.equal(request.kind, "xhr");
  assert.equal(request.method, "POST");
  assert.equal(request.credentials, "include");
  assertJsonEqual(request.headers, [{ name: "x-target-header", value: "kept" }]);
  assert.equal(new TextDecoder().decode(request.body), "runtime body");
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const timers = captureTimeoutCallbacks(context);
  const hostMessages: unknown[] = [];
  const hostCapability = "t".repeat(32);
  clientsById.set("host", {
    id: "host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message) {
      hostMessages.push(message);
    },
  });
  clientsById.set("target-page", {
    id: "target-page",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Ftimeout.example%2F",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  assert.equal(rememberClientSourceTargetForTest("target-page", "https://timeout.example/"), true);
  let resolveBody: ((body: ArrayBuffer) => void) | undefined;
  const body = new Promise<ArrayBuffer>((resolve) => {
    resolveBody = resolve;
  });
  const baseEvent = runtimeGatewayFetchEvent("https://timeout.example/search");
  const delayedPostEvent = {
    ...baseEvent,
    request: {
      ...baseEvent.request,
      clone: () => ({
        arrayBuffer: () => body,
      }),
    },
  };
  try {
    const responsePromise = handleGatewayFetchWithTimeout(delayedPostEvent);
    const timeoutCallback = timers.callbacks[0];
    assert(timeoutCallback, "Service Worker deadline was not scheduled");
    timeoutCallback();
    const response = await responsePromise;
    assert.equal(response.status, 504);
    const html = await response.text();
    assert.match(html, /data-rings-webview-failure-code="local_gateway_timeout"/);
    assert(resolveBody, "delayed request body resolver was not installed");
    resolveBody(new ArrayBuffer(0));
    await Promise.resolve();
    await Promise.resolve();
    assertJsonEqual(hostMessages, []);
  } finally {
    timers.restore();
  }
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const hostMessages: unknown[] = [];
  const popupMessages: unknown[] = [];
  const hostileMessages: unknown[] = [];
  clientsById.set("host", {
    id: "host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message) {
      hostMessages.push(message);
    },
  });
  clientsById.set("popup", {
    id: "popup",
    url: "http://127.0.0.1:8080/#webview",
    frameType: "auxiliary",
    postMessage(message) {
      popupMessages.push(message);
    },
  });
  assert.equal(rememberTrustedShellClientForTest("popup"), true);
  clientsById.set("hostile", {
    id: "hostile",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Fexample.test%2F",
    frameType: "top-level",
    postMessage(message) {
      hostileMessages.push(message);
    },
  });
  assert.equal(rememberClientSourceTargetForTest("hostile", "https://example.test/"), true);
  clientsById.set("forged-source", {
    id: "forged-source",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Fvictim.test%2F",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(rememberClientSourceTargetForTest("forged-source", "https://original.test/"), true);
  assert.equal(await sourceTargetForClient("forged-source"), "https://victim.test/");
  clientsById.set("recovered-source", {
    id: "recovered-source",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Frecovered.test%2Fpage",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(await sourceTargetForClient("recovered-source"), "https://recovered.test/page");
  const hostCapability = "h".repeat(32);
  const hostileCapability = "x".repeat(32);

  assert.equal(await gatewayHostClient(), undefined);
  assert.equal(await registerGatewayHostClient("host", "short"), false);
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  assert.equal((await gatewayHostClient())?.id, "host");

  resetGatewayHostForTest();
  assert.equal(rememberTrustedShellClientForTest("popup"), true);
  assertJsonEqual(
    await dispatchMessage("hostile", {
      type: "rings-webview-host-register",
      capability: hostileCapability,
    }),
    [{ ok: false, error: "untrusted gateway host registration" }],
  );
  assert.equal(await gatewayHostClient(), undefined);
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  assert.equal((await gatewayHostClient())?.id, "host");
  clientsById.set("hostile", {
    id: "hostile",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message) {
      hostileMessages.push(message);
    },
  });
  assert.equal(await registerGatewayHostClient("hostile", hostileCapability), false);
  assert.equal(await registerGatewayHostClient("host", hostileCapability), false);
  assert.equal((await gatewayHostClient())?.id, "host");
  // Cold-start discovery probes are intentionally benign, but this block
  // below verifies that no debug payload is delivered to the host.
  hostMessages.length = 0;

  await emitDebug("worker", "pre-registration secret");
  assertJsonEqual(
    await dispatchMessage("hostile", {
      type: "rings-webview-debug-register",
      capability: hostCapability,
    }),
    [{ ok: false, error: "untrusted debug client registration" }],
  );
  assert.equal(await registerDebugClient("popup", hostileCapability), false);
  assert.equal(await registerDebugClient("popup", hostCapability), true);
  await emitDebug("worker", "trusted-shell secret");
  assert.equal(await sourceTargetForClient("popup"), undefined);
  const popupMessageCountBeforeNavigation = popupMessages.length;
  assert.equal(rememberClientSourceTargetForTest("popup", "https://trusted.example/"), true);
  clientsById.set("popup", {
    id: "popup",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Ftrusted.example%2F",
    frameType: "auxiliary",
    postMessage(message) {
      popupMessages.push(message);
    },
  });
  assert.equal(await registerDebugClient("popup"), true);
  const postNavigationMessages: unknown[] = [];
  clientsById.set("popup-gateway", {
    id: "popup-gateway",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Ftrusted.example%2F",
    frameType: "auxiliary",
    postMessage(message) {
      postNavigationMessages.push(message);
    },
  });
  assert.equal(rememberClientSourceTargetForTest("popup-gateway", "https://trusted.example/"), true);
  assert.equal(await registerDebugClient("popup-gateway"), false);
  await emitDebug("worker", "trusted navigation", "info", {
    requestId: "navigation",
    target: "https://trusted.example/",
    method: "GET",
    kind: "navigation",
    phase: "completed",
    durationMs: 7,
    status: 200,
  });
  assertJsonEqual(
    await dispatchMessage("popup", {
      type: "rings-webview-debug-entry",
      capability: hostCapability,
      entry: { scope: "popup", message: "forged request https://secret.test/" },
    }),
    [{ ok: false, error: "untrusted debug entry" }],
  );
  assertJsonEqual(
    await dispatchMessage("hostile", {
      type: "rings-webview-debug-entry",
      capability: hostCapability,
      entry: { scope: "hostile", message: "stolen request https://secret.test/" },
    }),
    [{ ok: false, error: "untrusted debug entry" }],
  );
  await emitDebug("worker", "post-registration secret", "info", {
    requestId: "trusted-subresource",
    target: "https://trusted.example/app.js",
    sourceTarget: "https://trusted.example/",
    method: "GET",
    kind: "subresource",
    phase: "completed",
    durationMs: 3,
    status: 200,
  });
  await emitDebug("worker", "cross-target secret", "info", {
    requestId: "other-subresource",
    target: "https://other.example/app.js",
    sourceTarget: "https://other.example/",
    method: "GET",
    kind: "subresource",
    phase: "completed",
    durationMs: 3,
    status: 200,
  });
  await emitDebug("worker", "target-overlap secret", "info", {
    requestId: "target-overlap",
    target: "https://trusted.example/secret.js",
    sourceTarget: "https://other.example/",
    method: "GET",
    kind: "subresource",
    phase: "completed",
    durationMs: 4,
    status: 200,
  });
  await emitDebug("onion", "trusted onion route", "info", undefined, undefined, {
    target: "https://trusted.example/api",
    sourceTarget: "https://trusted.example/",
    kind: "fetch",
    phase: "selected",
  });
  await emitDebug("onion", "other onion route", "info", undefined, undefined, {
    target: "https://other.example/api",
    sourceTarget: "https://other.example/",
    kind: "fetch",
    phase: "selected",
  });
  await emitDebug("onion", "onion target-overlap route", "info", undefined, undefined, {
    target: "https://trusted.example/api",
    sourceTarget: "https://other.example/",
    kind: "fetch",
    phase: "selected",
  });
  assert.equal(hostileMessages.length, 0);
  assert.equal(hostMessages.length, 0);
  assert.ok(popupMessages.length >= 2);
  assert.ok(popupMessages.length > popupMessageCountBeforeNavigation);
  assert.equal(postNavigationMessages.length, 0);
  assert.match(JSON.stringify(popupMessages), /pre-registration secret/);
  assert.match(JSON.stringify(popupMessages), /trusted-shell secret/);
  assert.match(JSON.stringify(popupMessages), /trusted navigation/);
  assert.match(JSON.stringify(popupMessages), /post-registration secret/);
  assert.match(JSON.stringify(popupMessages), /trusted onion route/);
  assert.doesNotMatch(JSON.stringify(popupMessages), /cross-target secret/);
  assert.doesNotMatch(JSON.stringify(popupMessages), /other onion route/);
  assert.doesNotMatch(JSON.stringify(popupMessages), /target-overlap secret/);
  assert.doesNotMatch(JSON.stringify(popupMessages), /onion target-overlap route/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /trusted navigation/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /post-registration secret/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /trusted onion route/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /cross-target secret/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /other onion route/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /target-overlap secret/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /onion target-overlap route/);
  assert.doesNotMatch(JSON.stringify(popupMessages), /secret\.test/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /secret\.test/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), new RegExp(hostCapability));
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const sourceMessages: unknown[] = [];
  const resultMessages: unknown[] = [];
  const hostCapability = "h".repeat(32);
  clientsById.set("host", {
    id: "host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage() {},
  });
  clientsById.set("popup-source", {
    id: "popup-source",
    url: "http://127.0.0.1:8080/#webview",
    frameType: "auxiliary",
    postMessage(message) {
      sourceMessages.push(message);
    },
  });
  clientsById.set("popup-result", {
    id: "popup-result",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Fresult.example%2F",
    frameType: "auxiliary",
    postMessage(message) {
      resultMessages.push(message);
    },
  });

  assert.equal(rememberTrustedShellClientForTest("popup-source"), true);
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  assert.equal(await registerDebugClient("popup-source", hostCapability), true);
  await emitDebug("worker", "source shell only");
  assert.equal(await sourceTargetForClient("popup-source"), undefined);
  assert.equal(
    rememberNavigationClientTarget(
      {
        clientId: "popup-source",
        resultingClientId: "popup-result",
      },
      {
        kind: "navigation",
        requested: "http://127.0.0.1:8080/webview/https%3A%2F%2Fresult.example%2F",
        topLevelNavigation: true,
      },
    ),
    true,
  );
  assert.equal(await registerDebugClient("popup-result"), true);
  await emitDebug("worker", "result navigation", "info", {
    requestId: "result-navigation",
    target: "https://result.example/",
    method: "GET",
    kind: "navigation",
    phase: "completed",
    durationMs: 5,
    status: 200,
  });
  await emitDebug("worker", "unrelated result", "info", {
    requestId: "unrelated-result",
    target: "https://other.example/",
    method: "GET",
    kind: "navigation",
    phase: "completed",
    durationMs: 5,
    status: 200,
  });

  assert.match(JSON.stringify(sourceMessages), /source shell only/);
  assert.match(JSON.stringify(resultMessages), /result navigation/);
  assert.doesNotMatch(JSON.stringify(resultMessages), /source shell only/);
  assert.doesNotMatch(JSON.stringify(resultMessages), /unrelated result/);
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const spoofedMessages: unknown[] = [];
  clientsById.set("spoofed-gateway-client", {
    id: "spoofed-gateway-client",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message) {
      spoofedMessages.push(message);
    },
  });
  assert.equal(rememberClientSourceTargetForTest("spoofed-gateway-client", "https://target.example/"), true);

  assertJsonEqual(
    await dispatchMessage("spoofed-gateway-client", {
      type: "rings-webview-host-register",
      capability: "x".repeat(32),
    }),
    [{ ok: false, error: "untrusted gateway host registration" }],
  );
  assert.equal(await gatewayHostClient(), undefined);
  assert.equal(spoofedMessages.length, 0);
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const shellMessages: unknown[] = [];
  clientsById.set("cold-shell-host", {
    id: "cold-shell-host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message) {
      shellMessages.push(message);
    },
  });

  assertJsonEqual(
    await dispatchMessage("cold-shell-host", {
      type: "rings-webview-host-register",
      capability: "c".repeat(32),
    }),
    [{ ok: true }],
  );
  assert.equal((await gatewayHostClient())?.id, "cold-shell-host");
  assert.equal(shellMessages.length, 0);
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const nestedMessages: unknown[] = [];
  clientsById.set("nested-shell-host", {
    id: "nested-shell-host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "nested",
    postMessage(message) {
      nestedMessages.push(message);
    },
  });

  assertJsonEqual(
    await dispatchMessage("nested-shell-host", {
      type: "rings-webview-host-register",
      capability: "n".repeat(32),
    }),
    [{ ok: false, error: "untrusted gateway host registration" }],
  );
  assert.equal(await gatewayHostClient(), undefined);
  assert.equal(nestedMessages.length, 0);
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  clientsById.set("parent", {
    id: "parent",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Fparent.example%2F",
    frameType: "top-level",
    postMessage() {},
  });
  clientsById.set("iframe", {
    id: "iframe",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Fframe.example%2F",
    frameType: "nested",
    postMessage() {},
  });
  assert.equal(rememberClientSourceTargetForTest("parent", "https://parent.example/"), true);

  const iframeNavigationRemembered = rememberNavigationClientTarget(
    {
      clientId: "parent",
      resultingClientId: "iframe",
    },
    {
      kind: "navigation",
      requested: "http://127.0.0.1:8080/webview/https%3A%2F%2Fframe.example%2F",
      topLevelNavigation: false,
    },
  );
  assert.equal(iframeNavigationRemembered, true);
  assert.equal(await sourceTargetForClient("parent"), "https://parent.example/");
  assert.equal(await sourceTargetForClient("iframe"), "https://frame.example/");

  const topLevelFallbackRemembered = rememberNavigationClientTarget(
    {
      clientId: "parent",
    },
    {
      kind: "navigation",
      requested: "http://127.0.0.1:8080/webview/https%3A%2F%2Ftop.example%2F",
      topLevelNavigation: true,
    },
  );
  assert.equal(topLevelFallbackRemembered, true);
  clientsById.set("parent", {
    id: "parent",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Ftop.example%2F",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(await sourceTargetForClient("parent"), "https://top.example/");
}
