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
    request: { readonly kind: string },
    status: number,
    headers: Headers,
    body: Uint8Array | null,
  ) => Uint8Array | null;
  readonly emitDebug: (scope: string, message: string, level?: string) => Promise<void>;
  readonly gatewayHostClient: () => Promise<ServiceWorkerClientFixture | undefined>;
  readonly registerDebugClient: (clientId: string, capability: string) => Promise<boolean>;
  readonly registerGatewayHostClient: (clientId: string, capability: string) => Promise<boolean>;
  readonly resetGatewayHostForTest: () => void;
  readonly requestKind: (request: RequestKindFixture) => string;
};

/**
 * Minimal Client shape consumed by gateway host registration.
 */
type ServiceWorkerClientFixture = {
  readonly id: string;
  readonly url: string;
  readonly postMessage: (message: unknown) => void;
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
  `${serviceWorkerSource}\nglobalThis.__ringsWebviewServiceWorkerTest = { controlledNavigationBody, emitDebug, gatewayHostClient, registerDebugClient, registerGatewayHostClient, resetGatewayHostForTest, requestKind };`,
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
  gatewayHostClient,
  registerDebugClient,
  registerGatewayHostClient,
  resetGatewayHostForTest,
  requestKind,
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
  assert.match(html, /<script src="\/assets\/webview-overlay\.js"><\/script><\/head>/);
  assert.equal(headers.has("content-length"), false);
  assert.equal(headers.has("content-encoding"), false);
  assert.equal(headers.has("content-security-policy-report-only"), false);
  assert.equal(headers.has("x-frame-options"), false);
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
  assert.equal(text(body), html);
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
  const css = bytes("body { color: red; }");
  const body = controlledNavigationBody({ kind: "subresource" }, 200, new Headers({ "content-type": "text/css" }), css);
  assert.equal(body, css);
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
    postMessage(message) {
      hostMessages.push(message);
    },
  });
  clientsById.set("popup", {
    id: "popup",
    url: "http://127.0.0.1:8080/#webview",
    postMessage(message) {
      popupMessages.push(message);
    },
  });
  clientsById.set("hostile", {
    id: "hostile",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Fexample.test%2F",
    postMessage(message) {
      hostileMessages.push(message);
    },
  });
  const hostCapability = "h".repeat(32);
  const hostileCapability = "x".repeat(32);

  assert.equal(await gatewayHostClient(), undefined);
  assertJsonEqual(
    await dispatchMessage("hostile", {
      type: "rings-webview-host-register",
      capability: hostileCapability,
    }),
    [{ ok: false, error: "untrusted gateway host registration" }],
  );
  assert.equal(await gatewayHostClient(), undefined);
  assert.equal(await registerGatewayHostClient("host", "short"), false);
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  assert.equal((await gatewayHostClient())?.id, "host");
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
  clientsById.set("popup", {
    id: "popup",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Ftrusted.example%2F",
    postMessage(message) {
      popupMessages.push(message);
    },
  });
  const postNavigationMessages: unknown[] = [];
  clientsById.set("popup-gateway", {
    id: "popup-gateway",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Ftrusted.example%2F",
    postMessage(message) {
      postNavigationMessages.push(message);
    },
  });
  assert.equal(await registerDebugClient("popup-gateway", hostCapability), false);
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
  await emitDebug("worker", "post-registration secret");
  assert.equal(hostileMessages.length, 0);
  assert.equal(hostMessages.length, 0);
  assert.ok(popupMessages.length >= 2);
  assert.equal(popupMessages.length, popupMessageCountBeforeNavigation);
  assert.equal(postNavigationMessages.length, 0);
  assert.match(JSON.stringify(popupMessages), /pre-registration secret/);
  assert.match(JSON.stringify(popupMessages), /trusted-shell secret/);
  assert.doesNotMatch(JSON.stringify(popupMessages), /post-registration secret/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /post-registration secret/);
  assert.doesNotMatch(JSON.stringify(popupMessages), /secret\.test/);
  assert.doesNotMatch(JSON.stringify(postNavigationMessages), /secret\.test/);
}
