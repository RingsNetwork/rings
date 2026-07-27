#!/usr/bin/env node

/**
 * Runs unit checks for the Rings WebView service-worker request classifier.
 */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

/**
 * Minimal request shape consumed by the service worker's request-kind classifier.
 */
type RequestKindFixture = {
  readonly headers: Headers;
  readonly mode: string;
  readonly destination: string;
};

/**
 * Options used to build one request-kind fixture.
 */
type RequestKindFixtureOptions = {
  readonly headers?: HeadersInit;
  readonly mode?: string;
  readonly destination?: string;
};

/**
 * Service-worker symbols exported only inside this test VM.
 */
type ServiceWorkerTestApi = {
  readonly controlledNavigationBody: (
    request: { readonly kind: string; readonly topLevelNavigation?: boolean },
    status: number,
    headers: Headers,
    body: Uint8Array | null,
  ) => Uint8Array | null;
  readonly emitDebug: (
    scope: string,
    message: string,
    level?: string,
    resource?: unknown,
    at?: string,
    onion?: unknown,
  ) => Promise<void>;
  readonly gatewayFailureDocument: (status: number, message: string, reason: string, code: string) => string;
  readonly gatewayHostClient: () => Promise<ServiceWorkerClientFixture | undefined>;
  readonly rememberNavigationClientTarget: (
    event: ServiceWorkerNavigationEventFixture,
    request: ServiceWorkerNavigationRequestFixture,
  ) => boolean;
  readonly rememberClientSourceTargetForTest: (clientId: string, sourceTarget: string) => boolean;
  readonly rememberTrustedShellClientForTest: (clientId: string) => boolean;
  readonly registerDebugClient: (clientId: string, capability?: string) => Promise<boolean>;
  readonly registerGatewayHostClient: (clientId: string, capability: string) => Promise<boolean>;
  readonly resetGatewayHostForTest: () => void;
  readonly requestKind: (request: RequestKindFixture) => string;
  readonly sourceTargetForClient: (clientId: string | undefined) => Promise<string | undefined>;
};

/**
 * Minimal Client shape consumed by gateway host registration.
 */
type ServiceWorkerClientFixture = {
  readonly id: string;
  readonly url: string;
  readonly frameType: "auxiliary" | "top-level" | "nested" | "none";
  readonly postMessage: (message: unknown) => void;
};

/**
 * Minimal FetchEvent client identity shape used by navigation source-target tests.
 */
type ServiceWorkerNavigationEventFixture = {
  readonly clientId?: string;
  readonly resultingClientId?: string;
};

/**
 * Minimal serialized navigation request shape consumed by source-target tracking.
 */
type ServiceWorkerNavigationRequestFixture = {
  readonly kind: string;
  readonly requested: string;
  readonly topLevelNavigation?: boolean;
};

/**
 * Minimal message event shape used to drive the service worker registration handlers.
 */
type ServiceWorkerMessageEventFixture = {
  readonly source?: { readonly id?: string };
  readonly data?: unknown;
  readonly ports?: Array<{ postMessage: (message: unknown) => void }>;
  waitUntil?: (promise: Promise<unknown>) => void;
};

/**
 * VM global shape needed to load the service worker without a browser.
 */
type ServiceWorkerTestContext = Record<string, unknown> & {
  self: {
    readonly location: URL;
    addEventListener: (type: string, listener: (event: ServiceWorkerMessageEventFixture) => void) => void;
    clients: {
      get: (clientId: string) => Promise<ServiceWorkerClientFixture | undefined>;
      matchAll: () => Promise<ServiceWorkerClientFixture[]>;
    };
  };
  __ringsWebviewServiceWorkerTest?: ServiceWorkerTestApi;
};

/**
 * Minimal host-asset message event shape used to validate opener handoff.
 */
type HostAssetMessageEventFixture = {
  readonly data?: unknown;
  readonly origin: string;
  readonly source?: {
    readonly location: {
      readonly href: string;
    };
  };
  readonly ports?: Array<{ postMessage: (message: unknown) => void }>;
};

/**
 * VM global shape needed to load the host asset without a browser.
 */
type HostAssetTestContext = Record<string, unknown> & {
  readonly location: URL;
  readonly navigator: {
    readonly serviceWorker: {
      readonly addEventListener: (type: string, listener: (event: unknown) => void) => void;
    };
  };
  readonly crypto: {
    readonly getRandomValues: (values: Uint8Array) => Uint8Array;
  };
  readonly addEventListener: (type: string, listener: (event: HostAssetMessageEventFixture) => void) => void;
};

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const hostAssetPath = resolve(projectRoot, "assets", "webview-host.js");
const serviceWorkerPath = resolve(projectRoot, "rings-webview-service-worker.js");
const hostAssetSource = await readFile(hostAssetPath, "utf8");
const serviceWorkerSource = await readFile(serviceWorkerPath, "utf8");
const clientsById = new Map<string, ServiceWorkerClientFixture>();
const messageListeners: Array<(event: ServiceWorkerMessageEventFixture) => void> = [];
const context: ServiceWorkerTestContext = {
  console,
  Headers,
  ArrayBuffer,
  URL,
  TextDecoder,
  TextEncoder,
  Response,
  Uint8Array,
  performance,
  setTimeout,
  clearTimeout,
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

vm.runInNewContext(
  `${serviceWorkerSource}\nglobalThis.__ringsWebviewServiceWorkerTest = { controlledNavigationBody, emitDebug, gatewayFailureDocument, gatewayHostClient, rememberNavigationClientTarget, rememberClientSourceTargetForTest, rememberTrustedShellClientForTest, registerDebugClient, registerGatewayHostClient, resetGatewayHostForTest, requestKind, sourceTargetForClient };`,
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
  rememberNavigationClientTarget,
  rememberClientSourceTargetForTest,
  rememberTrustedShellClientForTest,
  registerDebugClient,
  registerGatewayHostClient,
  resetGatewayHostForTest,
  requestKind,
  sourceTargetForClient,
} = serviceWorkerApi;

/**
 * Resolves the frontend project root from either source or generated script paths.
 */
function frontendProjectRoot(currentScriptDir: string): string {
  const parentDir = dirname(currentScriptDir);
  if (parentDir.endsWith("/.generated")) {
    return resolve(parentDir, "..");
  }
  return resolve(currentScriptDir, "..");
}

/**
 * Builds the minimum request object needed by `requestKind`.
 */
function request(options: RequestKindFixtureOptions = {}): RequestKindFixture {
  return {
    headers: new Headers(options.headers),
    mode: options.mode ?? "cors",
    destination: options.destination ?? "",
  };
}

/**
 * Encodes one UTF-8 body for service-worker response mutation tests.
 */
function bytes(value: string): Uint8Array {
  return new TextEncoder().encode(value);
}

/**
 * Decodes one UTF-8 body produced by the service worker.
 */
function text(value: Uint8Array | null): string {
  assert(value, "expected response body bytes");
  return new TextDecoder().decode(value);
}

/**
 * Compares service-worker responses after crossing the VM realm boundary.
 */
function assertJsonEqual(actual: unknown, expected: unknown): void {
  assert.equal(JSON.stringify(actual), JSON.stringify(expected));
}

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

/**
 * Sends one synthetic opener handoff request into the host asset VM.
 */
function requestHostDebugCapability(sourceUrl: string, origin = "http://127.0.0.1:8080"): unknown[] {
  const listeners: Array<(event: HostAssetMessageEventFixture) => void> = [];
  const context: HostAssetTestContext = {
    console,
    URL,
    Uint8Array,
    clearTimeout,
    setTimeout,
    location: new URL("http://127.0.0.1:8080/#node"),
    navigator: {
      serviceWorker: {
        addEventListener() {},
      },
    },
    crypto: {
      getRandomValues(values) {
        values.fill(0x7b);
        return values;
      },
    },
    addEventListener(type, listener) {
      if (type === "message") {
        listeners.push(listener);
      }
    },
  };
  context[globalThisKey] = context;
  vm.runInNewContext(hostAssetSource, context, {
    filename: hostAssetPath,
  });

  const responses: unknown[] = [];
  for (const listener of listeners) {
    listener({
      data: { type: "rings-webview-debug-capability-request" },
      origin,
      source: {
        location: {
          href: sourceUrl,
        },
      },
      ports: [
        {
          postMessage(message) {
            responses.push(message);
          },
        },
      ],
    });
  }
  return responses;
}

{
  const trustedResponses = requestHostDebugCapability("http://127.0.0.1:8080/#webview");
  assert.equal(trustedResponses.length, 1);
  const response = trustedResponses[0] as {
    readonly capability?: string;
    readonly ok?: boolean;
    readonly type?: string;
  };
  assert.equal(response.type, "rings-webview-debug-capability-response");
  assert.equal(response.ok, true);
  assert.equal(response.capability?.length, 64);

  assertJsonEqual(requestHostDebugCapability("http://127.0.0.1:8080/webview/https%3A%2F%2Fexample.test%2F"), [
    { type: "rings-webview-debug-capability-response", ok: false },
  ]);
  assertJsonEqual(requestHostDebugCapability("https://attacker.example/#webview", "https://attacker.example"), [
    { type: "rings-webview-debug-capability-response", ok: false },
  ]);
}

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
  assert.equal(popupMessages.length, popupMessageCountBeforeNavigation);
  assert.equal(postNavigationMessages.length, 0);
  assert.match(JSON.stringify(popupMessages), /pre-registration secret/);
  assert.match(JSON.stringify(popupMessages), /trusted-shell secret/);
  assert.doesNotMatch(JSON.stringify(popupMessages), /trusted navigation/);
  assert.doesNotMatch(JSON.stringify(popupMessages), /post-registration secret/);
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
