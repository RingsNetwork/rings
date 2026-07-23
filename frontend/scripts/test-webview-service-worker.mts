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
  readonly requestKind: (request: RequestKindFixture) => string;
};

/**
 * VM global shape needed to load the service worker without a browser.
 */
type ServiceWorkerTestContext = Record<string, unknown> & {
  self: {
    readonly location: URL;
    addEventListener: () => void;
  };
  __ringsWebviewServiceWorkerTest?: ServiceWorkerTestApi;
};

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const serviceWorkerPath = resolve(projectRoot, "rings-webview-service-worker.js");
const serviceWorkerSource = await readFile(serviceWorkerPath, "utf8");
const context: ServiceWorkerTestContext = {
  console,
  Headers,
  URL,
  Response,
  performance,
  setTimeout,
  clearTimeout,
  self: {
    location: new URL("http://127.0.0.1:8080/"),
    addEventListener() {},
  },
};
context["globalThis"] = context;

vm.runInNewContext(`${serviceWorkerSource}\nglobalThis.__ringsWebviewServiceWorkerTest = { requestKind };`, context, {
  filename: serviceWorkerPath,
});

const serviceWorkerApi = context.__ringsWebviewServiceWorkerTest;
assert(serviceWorkerApi, "service worker test API was not exported");
const { requestKind } = serviceWorkerApi;

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
