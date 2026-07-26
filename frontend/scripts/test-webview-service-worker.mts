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
    addEventListener() {},
  },
};
const globalThisKey = "globalThis";
context[globalThisKey] = context;

vm.runInNewContext(
  `${serviceWorkerSource}\nglobalThis.__ringsWebviewServiceWorkerTest = { controlledNavigationBody, requestKind };`,
  context,
  {
    filename: serviceWorkerPath,
  },
);

const serviceWorkerApi = context.__ringsWebviewServiceWorkerTest;
assert(serviceWorkerApi, "service worker test API was not exported");
const { controlledNavigationBody, requestKind } = serviceWorkerApi;

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
  const css = bytes("body { color: red; }");
  const body = controlledNavigationBody({ kind: "subresource" }, 200, new Headers({ "content-type": "text/css" }), css);
  assert.equal(body, css);
}
