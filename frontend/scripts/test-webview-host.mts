/**
 * Verifies the WebView host bridge in an isolated browser-like VM.
 */

import assert from "node:assert/strict";
import vm from "node:vm";

import { assertJsonEqual } from "./webview-service-worker-fixtures.mjs";

/** Minimal host message event used to validate opener handoff. */
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

/** VM global shape needed to load the host asset without a browser. */
type HostAssetTestContext = Record<string, unknown> & {
  RingsWebviewGateway?: unknown;
  RingsWebviewHost?: {
    readonly ensureReady: () => Promise<unknown>;
    readonly shellPreparation: Promise<boolean>;
    readonly takeDebugEntries: () => unknown[];
  };
  readonly location: URL;
  readonly navigator: {
    readonly serviceWorker: {
      readonly addEventListener: (type: string, listener: (event: unknown) => void) => void;
      readonly controller?: unknown;
      readonly ready?: Promise<unknown>;
      readonly register?: (url: string, options: { readonly scope: string }) => Promise<unknown>;
      readonly removeEventListener?: (type: string, listener: (event: unknown) => void) => void;
    };
  };
  readonly crypto: {
    readonly getRandomValues: (values: Uint8Array) => Uint8Array;
  };
  readonly addEventListener: (type: string, listener: (event: HostAssetMessageEventFixture) => void) => void;
};

/** Service-worker message event consumed by the host gateway bridge. */
type HostGatewayEvent = {
  readonly data?: unknown;
  readonly ports?: Array<{ postMessage: (message: unknown) => void }>;
};

const globalThisKey = "globalThis";

/** Verify a first installation reloads exactly once after claiming the current page. */
async function verifyFirstInstallClaimsCurrentPage(hostAssetSource: string, hostAssetPath: string): Promise<void> {
  let controller: unknown;
  let controllerChangeListener: ((event: unknown) => void) | undefined;
  let reloadCount = 0;
  let resolveReload: (() => void) | undefined;
  const reloaded = new Promise<void>((resolve) => {
    resolveReload = resolve;
  });
  const activeWorker = { state: "activated" };
  const registration = { active: activeWorker };
  const location = Object.assign(new URL("http://127.0.0.1:8080/#node"), {
    reload() {
      reloadCount += 1;
      resolveReload?.();
    },
  });
  const context: HostAssetTestContext = {
    console,
    URL,
    Uint8Array,
    clearTimeout,
    setInterval: () => 1,
    setTimeout,
    location,
    navigator: {
      serviceWorker: {
        get controller() {
          return controller;
        },
        ready: Promise.resolve(registration),
        register(url, options) {
          assert.equal(url, "/rings-webview-service-worker.js?gateway-host-protocol=5");
          assert.equal(options.scope, "/");
          setTimeout(() => {
            controller = activeWorker;
            controllerChangeListener?.({});
          }, 0);
          return Promise.resolve(registration);
        },
        addEventListener(type, listener) {
          if (type === "controllerchange") {
            controllerChangeListener = listener;
          }
        },
        removeEventListener(type, listener) {
          if (type === "controllerchange" && controllerChangeListener === listener) {
            controllerChangeListener = undefined;
          }
        },
      },
    },
    crypto: {
      getRandomValues(values) {
        values.fill(0x31);
        return values;
      },
    },
    addEventListener() {},
  };
  context[globalThisKey] = context;
  vm.runInNewContext(hostAssetSource, context, {
    filename: hostAssetPath,
  });

  assert(context.RingsWebviewHost, "host bridge was not installed");
  assert.equal(await context.RingsWebviewHost.shellPreparation, true);
  assert.equal(await context.RingsWebviewHost.ensureReady(), registration);
  const reloadObserved = await Promise.race([
    reloaded.then(() => true),
    new Promise<false>((resolve) => setTimeout(() => resolve(false), 100)),
  ]);
  assert.equal(controller, activeWorker);
  assert.equal(
    reloadObserved,
    true,
    `first installation did not reload the claimed shell: ${JSON.stringify(context.RingsWebviewHost.takeDebugEntries())}`,
  );
  assert.equal(reloadCount, 1);
}

/** Verify a controlled shell records its trusted host capability before node startup. */
async function verifyControlledShellPreRegistersGateway(hostAssetSource: string, hostAssetPath: string): Promise<void> {
  const messages: unknown[] = [];
  let reloadCount = 0;
  class HostMessageChannel {
    readonly port1: {
      onmessage?: (event: { readonly data: unknown }) => void;
      close: () => void;
    };
    readonly port2: { postMessage: (message: unknown) => void };

    constructor() {
      this.port1 = { close() {} };
      this.port2 = {
        postMessage: (message) => this.port1.onmessage?.({ data: message }),
      };
    }
  }
  const activeWorker = {
    state: "activated",
    postMessage(message: unknown, ports?: Array<{ postMessage: (message: unknown) => void }>) {
      messages.push(message);
      ports?.[0]?.postMessage({ ok: true });
    },
  };
  const registration = { active: activeWorker };
  const location = Object.assign(new URL("http://127.0.0.1:8080/#node"), {
    reload() {
      reloadCount += 1;
    },
  });
  const context: HostAssetTestContext = {
    console,
    URL,
    Uint8Array,
    clearTimeout,
    setInterval: () => 1,
    setTimeout,
    MessageChannel: HostMessageChannel,
    location,
    navigator: {
      serviceWorker: {
        controller: activeWorker,
        ready: Promise.resolve(registration),
        register: () => Promise.resolve(registration),
        addEventListener() {},
        removeEventListener() {},
      },
    },
    crypto: {
      getRandomValues(values) {
        values.fill(0x52);
        return values;
      },
    },
    addEventListener() {},
  };
  context[globalThisKey] = context;
  vm.runInNewContext(hostAssetSource, context, {
    filename: hostAssetPath,
  });

  assert(context.RingsWebviewHost, "host bridge was not installed");
  assert.equal(await context.RingsWebviewHost.shellPreparation, false);
  assert.equal(reloadCount, 0);
  const registrationMessages = messages.filter(
    (message) => (message as { readonly type?: string }).type === "rings-webview-host-register",
  );
  assert.equal(registrationMessages.length, 1);
  const registrationMessage = registrationMessages[0] as { readonly capability?: string; readonly type?: string };
  assert.equal(registrationMessage.type, "rings-webview-host-register");
  assert.equal(registrationMessage.capability?.length, 64);
}

/** Send one synthetic opener handoff request into the host asset VM. */
function requestHostDebugCapability(
  hostAssetSource: string,
  hostAssetPath: string,
  sourceUrl: string,
  origin = "http://127.0.0.1:8080",
): unknown[] {
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

/** Verify cancellation cleanup and synchronous handler failure conversion. */
async function verifyHostGatewayCancellation(hostAssetSource: string, hostAssetPath: string): Promise<void> {
  const listeners: Array<(event: HostGatewayEvent) => void> = [];
  let resolveHandler: ((response: unknown) => void) | undefined;
  const cancelled: number[] = [];
  const responses: unknown[] = [];
  const context: HostAssetTestContext = {
    console,
    URL,
    Uint8Array,
    clearTimeout,
    setTimeout,
    location: new URL("http://127.0.0.1:8080/#node"),
    navigator: {
      serviceWorker: {
        addEventListener(type, listener) {
          if (type === "message") {
            listeners.push(listener as (event: HostGatewayEvent) => void);
          }
        },
      },
    },
    crypto: {
      getRandomValues(values) {
        values.fill(0x4c);
        return values;
      },
    },
    addEventListener() {},
  };
  context.RingsWebviewGateway = {
    handle(_request: unknown, requestId: number) {
      assert.equal(requestId, 703);
      return new Promise((resolve) => {
        resolveHandler = resolve;
      });
    },
    cancel(requestId: number) {
      cancelled.push(requestId);
      return true;
    },
  };
  context[globalThisKey] = context;
  vm.runInNewContext(hostAssetSource, context, {
    filename: hostAssetPath,
  });

  for (const listener of listeners) {
    listener({
      data: {
        type: "rings-webview-gateway-request",
        requestId: 703,
        request: { kind: "fetch", method: "GET" },
      },
      ports: [{ postMessage: (message) => responses.push(message) }],
    });
  }
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert(resolveHandler, "host gateway handler was not invoked");
  for (const listener of listeners) {
    listener({
      data: {
        type: "rings-webview-gateway-cancel",
        requestId: 703,
      },
    });
  }
  assertJsonEqual(cancelled, [703]);

  resolveHandler({ ok: true, status: 200, headers: [], body: null });
  await new Promise<void>((resolve) => setImmediate(resolve));
  assertJsonEqual(responses, []);

  context.RingsWebviewGateway = {
    handle() {
      throw new Error("synchronous gateway failure");
    },
  };
  for (const listener of listeners) {
    listener({
      data: {
        type: "rings-webview-gateway-request",
        requestId: 704,
        request: { kind: "fetch", method: "GET" },
      },
      ports: [{ postMessage: (message) => responses.push(message) }],
    });
  }
  await new Promise<void>((resolve) => setImmediate(resolve));
  assertJsonEqual(responses, [
    {
      ok: false,
      status: 502,
      errorCode: "gateway_transport_failed",
      errorSummary: "Gateway transport failed.",
      error: "Error: synchronous gateway failure",
    },
  ]);
}

/** Verify trusted handoff, cancellation, and gateway failure behavior. */
export async function verifyWebviewHostAsset(hostAssetSource: string, hostAssetPath: string): Promise<void> {
  await verifyFirstInstallClaimsCurrentPage(hostAssetSource, hostAssetPath);
  await verifyControlledShellPreRegistersGateway(hostAssetSource, hostAssetPath);

  const trustedResponses = requestHostDebugCapability(hostAssetSource, hostAssetPath, "http://127.0.0.1:8080/#webview");
  assert.equal(trustedResponses.length, 1);
  const response = trustedResponses[0] as {
    readonly capability?: string;
    readonly ok?: boolean;
    readonly type?: string;
  };
  assert.equal(response.type, "rings-webview-debug-capability-response");
  assert.equal(response.ok, true);
  assert.equal(response.capability?.length, 64);

  assertJsonEqual(
    requestHostDebugCapability(
      hostAssetSource,
      hostAssetPath,
      "http://127.0.0.1:8080/webview/https%3A%2F%2Fexample.test%2F",
    ),
    [{ type: "rings-webview-debug-capability-response", ok: false }],
  );
  assertJsonEqual(
    requestHostDebugCapability(
      hostAssetSource,
      hostAssetPath,
      "https://attacker.example/#webview",
      "https://attacker.example",
    ),
    [{ type: "rings-webview-debug-capability-response", ok: false }],
  );

  await verifyHostGatewayCancellation(hostAssetSource, hostAssetPath);
}
