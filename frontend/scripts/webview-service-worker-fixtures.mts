/**
 * Shared typed fixtures for WebView service-worker VM tests.
 */

import assert from "node:assert/strict";
import { dirname, resolve } from "node:path";

/** Minimal request shape consumed by the request-kind classifier. */
export type RequestKindFixture = {
  readonly headers: Headers;
  readonly mode: string;
  readonly destination: string;
};

/** Options used to build one request-kind fixture. */
export type RequestKindFixtureOptions = {
  readonly headers?: HeadersInit;
  readonly mode?: string;
  readonly destination?: string;
};

/** Minimal Client shape consumed by gateway host registration. */
export type ServiceWorkerClientFixture = {
  readonly id: string;
  readonly url: string;
  readonly frameType: "auxiliary" | "top-level" | "nested" | "none";
  readonly postMessage: (message: unknown, transfer?: unknown[]) => void;
};

/** Minimal FetchEvent shape used by full gateway fetch tests. */
export type ServiceWorkerFetchEventFixture = {
  readonly clientId?: string;
  readonly resultingClientId?: string;
  readonly request: {
    readonly url: string;
    readonly method: string;
    readonly credentials: string;
    readonly mode: string;
    readonly destination: string;
    readonly headers: Headers;
    readonly body: ReadableStream<Uint8Array> | null;
  };
};

/** FetchEvent client identity used by navigation source-target tests. */
export type ServiceWorkerNavigationEventFixture = {
  readonly clientId?: string;
  readonly resultingClientId?: string;
};

/** Serialized navigation request consumed by source-target tracking. */
export type ServiceWorkerNavigationRequestFixture = {
  readonly kind: string;
  readonly requested: string;
  readonly topLevelNavigation?: boolean;
};

/** Minimal message event used to drive service-worker handlers. */
export type ServiceWorkerMessageEventFixture = {
  readonly source?: { readonly id?: string };
  readonly data?: unknown;
  readonly ports?: Array<{ postMessage: (message: unknown) => void }>;
  waitUntil?: (promise: Promise<unknown>) => void;
};

/** Service-worker symbols exported only inside the test VM. */
export type ServiceWorkerTestApi = {
  readonly acquireGatewayBodyPermit: (signal?: AbortSignal) => Promise<() => void>;
  readonly gatewayContentSecurityPolicy: string;
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
  readonly handleGatewayFetch: (
    event: ServiceWorkerFetchEventFixture,
    requestId: number,
    startedAt: number,
  ) => Promise<Response>;
  readonly handleGatewayFetchWithTimeout: (event: ServiceWorkerFetchEventFixture) => Promise<Response>;
  readonly pruneTrackedClientState: () => Promise<void>;
  readonly rememberNavigationClientTarget: (
    event: ServiceWorkerNavigationEventFixture,
    request: ServiceWorkerNavigationRequestFixture,
  ) => boolean;
  readonly rememberShellNavigationClient: (
    event: {
      readonly clientId?: string;
      readonly resultingClientId?: string;
      readonly request: { readonly mode: string; readonly destination: string };
    },
    url: URL,
  ) => boolean;
  readonly rememberClientSourceTargetForTest: (clientId: string, sourceTarget: string) => boolean;
  readonly rememberTrustedShellClientForTest: (clientId: string) => boolean;
  readonly registerDebugClient: (clientId: string, capability?: string) => Promise<boolean>;
  readonly registerGatewayHostClient: (clientId: string, capability: string) => Promise<boolean>;
  readonly requestGatewayResponse: (
    host: ServiceWorkerClientFixture,
    request: unknown,
    requestId: number,
  ) => Promise<{
    readonly ok: boolean;
    readonly status: number;
    readonly errorCode?: string;
  }>;
  readonly resetGatewayHostForTest: () => void;
  readonly requestKind: (request: RequestKindFixture) => string;
  readonly sourceTargetForClient: (clientId: string | undefined) => Promise<string | undefined>;
};

/** VM global shape needed to load the service worker without a browser. */
export type ServiceWorkerTestContext = Record<string, unknown> & {
  setTimeout: typeof setTimeout;
  clearTimeout: typeof clearTimeout;
  importScripts: (...urls: string[]) => void;
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

/** Replace VM timers with deterministic callbacks until restored. */
export function captureTimeoutCallbacks(context: ServiceWorkerTestContext): {
  readonly callbacks: Array<() => void>;
  readonly restore: () => void;
} {
  const originalSetTimeout = context.setTimeout;
  const originalClearTimeout = context.clearTimeout;
  const callbacks: Array<() => void> = [];
  context.setTimeout = ((callback: TimerHandler) => {
    if (typeof callback === "function") {
      callbacks.push(() => callback());
    }
    return callbacks.length;
  }) as unknown as typeof setTimeout;
  context.clearTimeout = (() => {}) as unknown as typeof clearTimeout;
  return {
    callbacks,
    restore() {
      context.setTimeout = originalSetTimeout;
      context.clearTimeout = originalClearTimeout;
    },
  };
}

/** Resolve the frontend project root from source or generated script paths. */
export function frontendProjectRoot(currentScriptDir: string): string {
  const parentDir = dirname(currentScriptDir);
  if (parentDir.endsWith("/.generated")) {
    return resolve(parentDir, "..");
  }
  return resolve(currentScriptDir, "..");
}

/** Build the minimum request object needed by `requestKind`. */
export function request(options: RequestKindFixtureOptions = {}): RequestKindFixture {
  return {
    headers: new Headers(options.headers),
    mode: options.mode ?? "cors",
    destination: options.destination ?? "",
  };
}

/** Encode one UTF-8 body for response mutation tests. */
export function bytes(value: string): Uint8Array {
  return new TextEncoder().encode(value);
}

/** Build one full gateway fetch event. */
export function gatewayFetchEvent(target = "https://example.test/"): ServiceWorkerFetchEventFixture {
  return {
    request: {
      url: `http://127.0.0.1:8080/webview/${encodeURIComponent(target)}`,
      method: "GET",
      credentials: "same-origin",
      mode: "navigate",
      destination: "document",
      headers: new Headers(),
      body: null,
    },
  };
}

/** Build a runtime fetch/XHR event carrying its target in a trusted header. */
export function runtimeGatewayFetchEvent(target = "https://example.test/api"): ServiceWorkerFetchEventFixture {
  const body = bytes("runtime body");
  return {
    clientId: "target-page",
    request: {
      url: "http://127.0.0.1:8080/webview-runtime/fixture-1",
      method: "POST",
      credentials: "include",
      mode: "same-origin",
      destination: "",
      headers: new Headers({
        "X-Rings-Webview-Kind": "xhr",
        "X-Rings-Webview-Target": target,
        "X-Target-Header": "kept",
      }),
      body: byteStream(body),
    },
  };
}

/** Return one request body stream owned by the fixture request. */
function byteStream(body: Uint8Array): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(body.slice());
      controller.close();
    },
  });
}

/** Decode one UTF-8 body produced by the service worker. */
export function text(value: Uint8Array | null): string {
  assert(value, "expected response body bytes");
  return new TextDecoder().decode(value);
}

/** Compare values after crossing the VM realm boundary. */
export function assertJsonEqual(actual: unknown, expected: unknown): void {
  assert.equal(JSON.stringify(actual), JSON.stringify(expected));
}
