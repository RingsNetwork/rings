#!/usr/bin/env node

/** Runs unit checks for the Rings WebView service-worker request classifier. */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { MessageChannel } from "node:worker_threads";

import { runStaticServiceWorkerTests, type WorkerRequestApi } from "./test-webview-service-worker-static.mjs";
import {
  assertJsonEqual,
  bytes,
  captureTimeoutCallbacks,
  frontendProjectRoot,
  gatewayFetchEvent,
  runtimeGatewayFetchEvent,
  type ServiceWorkerClientFixture,
  type ServiceWorkerMessageEventFixture,
  type ServiceWorkerTestContext,
} from "./webview-service-worker-fixtures.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const hostAssetPath = resolve(projectRoot, "assets", "webview-host.js");
const workerResponseAssetPath = resolve(projectRoot, "assets", "webview-worker-response.js");
const workerNavigationAssetPath = resolve(projectRoot, "assets", "webview-worker-navigation.js");
const workerRequestAssetPath = resolve(projectRoot, "assets", "webview-worker-request.js");
const serviceWorkerPath = resolve(projectRoot, "rings-webview-service-worker.js");
const canonicalGatewayCspPath = resolve(projectRoot, "..", "crates", "webview", "gateway-content-security-policy.txt");
const hostAssetSource = await readFile(hostAssetPath, "utf8");
const workerResponseAssetSource = await readFile(workerResponseAssetPath, "utf8");
const workerNavigationAssetSource = await readFile(workerNavigationAssetPath, "utf8");
const workerRequestAssetSource = await readFile(workerRequestAssetPath, "utf8");
const serviceWorkerSource = await readFile(serviceWorkerPath, "utf8");
const canonicalGatewayCsp = (await readFile(canonicalGatewayCspPath, "utf8")).trimEnd();
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
  ReadableStream,
  MessageChannel,
  performance,
  setTimeout,
  clearTimeout,
  importScripts(...urls) {
    for (const url of urls) {
      const pathname = new URL(url, "http://127.0.0.1:8080/").pathname;
      const [source, filename] =
        pathname === "/assets/webview-worker-response.js"
          ? [workerResponseAssetSource, workerResponseAssetPath]
          : pathname === "/assets/webview-worker-navigation.js"
            ? [workerNavigationAssetSource, workerNavigationAssetPath]
            : pathname === "/assets/webview-worker-request.js"
              ? [workerRequestAssetSource, workerRequestAssetPath]
              : assert.fail(`unexpected Service Worker import: ${pathname}`);
      vm.runInContext(source, context, { filename });
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
context.__ringsWebviewGatewayHostLifetimeMs = 0;
vm.createContext(context);

vm.runInContext(
  `${serviceWorkerSource}\nglobalThis.__ringsWebviewServiceWorkerTest = { acquireGatewayBodyPermit, controlledNavigationBody, emitDebug, gatewayContentSecurityPolicy, gatewayFailureDocument, gatewayHostClient, handleGatewayFetch, handleGatewayFetchWithTimeout, holdShellNavigationForHostRegistration, pruneTrackedClientState, rememberNavigationClientTarget, rememberShellNavigationClient, rememberClientSourceTargetForTest, rememberTrustedShellClientForTest, registerDebugClient, registerGatewayHostClient, requestGatewayResponse, resetGatewayHostForTest, requestKind, sourceTargetForClient };`,
  context,
  {
    filename: serviceWorkerPath,
  },
);

const serviceWorkerApi = context.__ringsWebviewServiceWorkerTest;
assert(serviceWorkerApi, "service worker test API was not exported");
const {
  acquireGatewayBodyPermit,
  emitDebug,
  gatewayHostClient,
  handleGatewayFetch,
  handleGatewayFetchWithTimeout,
  holdShellNavigationForHostRegistration,
  pruneTrackedClientState,
  rememberNavigationClientTarget,
  rememberShellNavigationClient,
  rememberClientSourceTargetForTest,
  rememberTrustedShellClientForTest,
  registerDebugClient,
  registerGatewayHostClient,
  requestGatewayResponse,
  resetGatewayHostForTest,
  sourceTargetForClient,
} = serviceWorkerApi;
const workerRequestApi = (
  context.self as unknown as {
    readonly RingsWebviewWorkerRequest: WorkerRequestApi;
  }
).RingsWebviewWorkerRequest;

await runStaticServiceWorkerTests({
  api: serviceWorkerApi,
  canonicalGatewayCsp,
  globalThisKey,
  hostAssetPath,
  hostAssetSource,
  workerRequestApi,
});

{
  const held = await Promise.all(Array.from({ length: 6 }, () => acquireGatewayBodyPermit()));
  await assert.rejects(
    acquireGatewayBodyPermit(),
    (error) => typeof error === "object",
    "the seventh body-bearing request must fail before retaining an unread waiter",
  );
  const bodylessWhileFull = await handleGatewayFetch(
    gatewayFetchEvent("https://bodyless.example/"),
    698,
    performance.now(),
  );
  assert.match(
    await bodylessWhileFull.text(),
    /data-rings-webview-failure-code="local_gateway_unavailable"/,
    "bodyless requests must bypass the body-retention gate",
  );
  const oversizedWhileFull = runtimeGatewayFetchEvent("https://example.test/oversized");
  oversizedWhileFull.request.headers.set("content-length", String(workerRequestApi.gatewayRequestBodyLimitBytes + 1));
  const oversizedResponse = await handleGatewayFetch(oversizedWhileFull, 699, performance.now());
  assert.equal(oversizedResponse.status, 413, "declared oversize must be classified before the full waiter queue");
  const firstRelease = held.shift();
  assert(firstRelease, "one active body permit must exist");
  firstRelease();
  const replacement = await acquireGatewayBodyPermit();
  replacement();
  for (const release of held) release();
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
  assert.equal(rememberTrustedShellClientForTest("host"), true);
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
  resetGatewayHostForTest();
  clientsById.clear();
  const firstCapability = "a".repeat(32);
  clientsById.set("departed-host", {
    id: "departed-host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(rememberTrustedShellClientForTest("departed-host"), true);
  assert.equal(await registerGatewayHostClient("departed-host", firstCapability), true);
  clientsById.clear();
  await pruneTrackedClientState();
  assert.equal(await gatewayHostClient(), undefined);

  const replacementCapability = "b".repeat(32);
  clientsById.set("replacement-host", {
    id: "replacement-host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(rememberTrustedShellClientForTest("replacement-host"), true);
  assert.equal(await registerGatewayHostClient("replacement-host", replacementCapability), true);
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const hostCapability = "e".repeat(32);
  clientsById.set("host", {
    id: "host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(rememberTrustedShellClientForTest("host"), true);
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  const originalGet = context.self.clients.get;
  context.self.clients.get = async () => {
    throw new Error("backend-secret-detail");
  };
  try {
    const response = await handleGatewayFetchWithTimeout(gatewayFetchEvent("https://example.test/"));
    assert.equal(response.status, 502);
    const html = await response.text();
    assert.match(html, /data-rings-webview-failure-code="gateway_internal_failure"/);
    assert.doesNotMatch(html, /backend-secret-detail/);
  } finally {
    context.self.clients.get = originalGet;
  }
}

{
  resetGatewayHostForTest();
  clientsById.clear();
  const hostCapability = "i".repeat(32);
  clientsById.set("host", {
    id: "host",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(_message, transfer) {
      const reply = transfer?.[0] as { postMessage?: (response: unknown) => void } | undefined;
      reply?.postMessage?.({
        ok: true,
        status: 200,
        headers: [{ name: "backend-secret-header\nname", value: "hidden" }],
        body: bytes("ok"),
      });
    },
  });
  assert.equal(rememberTrustedShellClientForTest("host"), true);
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);

  const response = await handleGatewayFetch(
    gatewayFetchEvent("https://invalid-response.example/"),
    703,
    performance.now(),
  );
  assert.equal(response.status, 502);
  const html = await response.text();
  assert.match(html, /data-rings-webview-failure-code="invalid_gateway_response"/);
  assert.doesNotMatch(html, /backend-secret-header/);
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
  assert.equal(rememberTrustedShellClientForTest("host"), true);
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
  assert.equal(rememberTrustedShellClientForTest("host"), true);
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

  const oversized = runtimeGatewayFetchEvent("https://example.test/upload");
  oversized.request.headers.set("content-length", String(workerRequestApi.gatewayRequestBodyLimitBytes + 1));
  const rejected = await handleGatewayFetch(oversized, 702, performance.now());
  assert.equal(rejected.status, 413);
  assert.match(await rejected.text(), /data-rings-webview-failure-code="gateway_request_body_too_large"/);
  assert.equal(hostRequests.length, 1, "oversized bodies must be rejected before host dispatch");
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
  assert.equal(rememberTrustedShellClientForTest("host"), true);
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  assert.equal(rememberClientSourceTargetForTest("target-page", "https://timeout.example/"), true);
  let bodyStarted = false;
  let bodyCancelled = false;
  const baseEvent = runtimeGatewayFetchEvent("https://timeout.example/search");
  const delayedPostEvent = {
    ...baseEvent,
    request: {
      ...baseEvent.request,
      get body() {
        return new ReadableStream<Uint8Array>({
          start() {
            bodyStarted = true;
          },
          cancel() {
            bodyCancelled = true;
          },
        });
      },
    },
  };
  try {
    const responsePromise = handleGatewayFetchWithTimeout(delayedPostEvent);
    const timeoutCallback = timers.callbacks[0];
    assert(timeoutCallback, "Service Worker deadline was not scheduled");
    for (let turn = 0; turn < 8 && !bodyStarted; turn += 1) {
      await Promise.resolve();
    }
    assert.equal(bodyStarted, true, "request body read did not start");
    timeoutCallback();
    const response = await responsePromise;
    assert.equal(response.status, 504);
    const html = await response.text();
    assert.match(html, /data-rings-webview-failure-code="local_gateway_timeout"/);
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.equal(bodyCancelled, true);
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
  assert.equal(await sourceTargetForClient("forged-source"), undefined);
  clientsById.set("history-source", {
    id: "history-source",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Foriginal.test%2Fnext%3Fpage%3D2",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(rememberClientSourceTargetForTest("history-source", "https://original.test/start"), true);
  assert.equal(await sourceTargetForClient("history-source"), "https://original.test/next?page=2");
  clientsById.set("recovered-source", {
    id: "recovered-source",
    url: "http://127.0.0.1:8080/webview/https%3A%2F%2Frecovered.test%2Fpage",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(await sourceTargetForClient("recovered-source"), undefined);
  const hostCapability = "h".repeat(32);
  const hostileCapability = "x".repeat(32);

  assert.equal(await gatewayHostClient(), undefined);
  assert.equal(await registerGatewayHostClient("host", "short"), false);
  assert.equal(await registerGatewayHostClient("host", hostCapability), false);
  assert.equal(rememberTrustedShellClientForTest("host"), true);
  assert.equal(await registerGatewayHostClient("host", hostCapability), true);
  assert.equal((await gatewayHostClient())?.id, "host");

  resetGatewayHostForTest();
  assert.equal(rememberTrustedShellClientForTest("popup"), true);
  clientsById.set("hostile", {
    id: "hostile",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage(message) {
      hostileMessages.push(message);
    },
  });
  assert.equal(await registerGatewayHostClient("hostile", hostileCapability), false);
  assertJsonEqual(
    await dispatchMessage("hostile", {
      type: "rings-webview-host-register",
      capability: hostileCapability,
    }),
    [{ ok: false, error: "untrusted gateway host registration" }],
  );
  assert.equal(await gatewayHostClient(), undefined);
  assert.equal(await registerGatewayHostClient("host", hostCapability), false);
  assert.equal(
    rememberShellNavigationClient(
      {
        resultingClientId: "host",
        request: { mode: "navigate", destination: "document" },
      },
      new URL("http://127.0.0.1:8080/"),
    ),
    true,
  );
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

  clientsById.set("hostile-path", {
    id: "hostile-path",
    url: "http://127.0.0.1:8080/#node",
    frameType: "top-level",
    postMessage() {},
  });
  assert.equal(
    rememberShellNavigationClient(
      {
        resultingClientId: "hostile-path",
        request: { mode: "navigate", destination: "document" },
      },
      new URL("http://127.0.0.1:8080/untrusted-page"),
    ),
    false,
  );
  assert.equal(await registerGatewayHostClient("hostile-path", hostCapability), false);
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
  assert.equal(rememberTrustedShellClientForTest("host"), true);
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
    // WindowClient.url may omit the document fragment even when the application is on #node.
    url: "http://127.0.0.1:8080/",
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
    [{ ok: false, error: "untrusted gateway host registration" }],
  );
  assert.equal(
    rememberShellNavigationClient(
      {
        resultingClientId: "cold-shell-host",
        request: { mode: "navigate", destination: "document" },
      },
      new URL("http://127.0.0.1:8080/"),
    ),
    true,
  );
  let navigationLifetimeSettled = false;
  const navigationLifetime = holdShellNavigationForHostRegistration("cold-shell-host").then(() => {
    navigationLifetimeSettled = true;
  });
  await Promise.resolve();
  assert.equal(navigationLifetimeSettled, false);
  assertJsonEqual(
    await dispatchMessage("cold-shell-host", {
      type: "rings-webview-host-register",
      capability: "c".repeat(32),
    }),
    [{ ok: true }],
  );
  await navigationLifetime;
  assert.equal(navigationLifetimeSettled, true);
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
