#!/usr/bin/env node

/** Verifies live recursive-renderer routing state in the MV3 service worker. */
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { frontendProjectRoot } from "./extension_webview_test_support.mjs";

/** Minimal navigation identity consumed by the adapter callbacks. */
type NavigationDetails = {
  readonly tabId: number;
  readonly frameId: number;
  readonly parentFrameId: number;
  readonly url: string;
};

/** Generated module surface under test. */
type ExtensionServiceWorkerModule = {
  readonly installExtensionWebviewServiceWorker: () => void;
};

const tabId = 7;
const frameId = 11;
const nestedSession = "30c5b6a5-483e-4c3c-8d13-b4d28f713b01";
const sentMessages: unknown[] = [];
let beforeNavigate: ((details: NavigationDetails) => void) | undefined;
let committed: ((details: NavigationDetails) => void) | undefined;
const blockedResourceTypes = [
  "main_frame",
  "sub_frame",
  "stylesheet",
  "script",
  "image",
  "font",
  "object",
  "xmlhttprequest",
  "ping",
  "csp_report",
  "media",
  "websocket",
  "webtransport",
  "webbundle",
  "other",
] as const;
const extensionOrigin = "chrome-extension://fixture/";
const chromeFixture = {
  runtime: {
    lastError: undefined,
    getURL: (path: string): string => new URL(path, extensionOrigin).href,
    getContexts: async (): Promise<readonly { readonly tabId: number; readonly documentUrl: string }[]> => [
      { tabId, documentUrl: new URL("webview.html", extensionOrigin).href },
    ],
    sendMessage: (message: unknown, callback?: () => void): void => {
      sentMessages.push(message);
      callback?.();
    },
    onMessage: { addListener: (): void => {} },
  },
  tabs: {
    onRemoved: { addListener: (): void => {} },
  },
  webNavigation: {
    onBeforeNavigate: {
      addListener: (listener: (details: NavigationDetails) => void): void => {
        beforeNavigate = listener;
      },
    },
    onCommitted: {
      addListener: (listener: (details: NavigationDetails) => void): void => {
        committed = listener;
      },
    },
    getFrame: async (): Promise<undefined> => undefined,
  },
  declarativeNetRequest: {
    getSessionRules: async (): Promise<readonly unknown[]> => [
      {
        id: tabId,
        priority: 1,
        action: { type: "block" },
        condition: {
          tabIds: [tabId],
          regexFilter: "^https?://",
          resourceTypes: blockedResourceTypes,
        },
      },
    ],
    updateSessionRules: async (): Promise<void> => {},
  },
  storage: {
    session: {
      get: async (): Promise<Record<string, never>> => ({}),
      set: async (): Promise<void> => {},
      remove: async (): Promise<void> => {},
    },
  },
  windows: { create: (): void => {} },
};
Object.defineProperty(globalThis, "chrome", { configurable: true, value: chromeFixture });

const scriptDir = dirname(fileURLToPath(import.meta.url));
const frontendRoot = frontendProjectRoot(scriptDir);
const protocolSource = await readFile(
  resolve(frontendRoot, ".generated", "extension-assets", "webview_protocol.js"),
  "utf8",
);
const protocolUrl = `data:text/javascript;base64,${Buffer.from(protocolSource).toString("base64")}`;
const runtimeSource = await readFile(
  resolve(frontendRoot, ".generated", "extension-assets", "extension_runtime.js"),
  "utf8",
);
const linkedRuntimeSource = runtimeSource.replace('"./webview_protocol.js"', JSON.stringify(protocolUrl));
assert.notEqual(linkedRuntimeSource, runtimeSource, "runtime fixture did not link its generated protocol dependency");
const runtimeUrl = `data:text/javascript;base64,${Buffer.from(linkedRuntimeSource).toString("base64")}`;
const moduleSource = await readFile(
  resolve(frontendRoot, ".generated", "extension-assets", "extension_webview_service_worker.js"),
  "utf8",
);
const linkedModuleSource = moduleSource.replace('from "./extension_runtime.js"', `from ${JSON.stringify(runtimeUrl)}`);
assert.notEqual(
  linkedModuleSource,
  moduleSource,
  "service-worker fixture did not link its generated runtime dependency",
);
const moduleUrl = `data:text/javascript;base64,${Buffer.from(linkedModuleSource).toString("base64")}`;
const serviceWorker = (await import(moduleUrl)) as ExtensionServiceWorkerModule;
serviceWorker.installExtensionWebviewServiceWorker();
assert(beforeNavigate, "onBeforeNavigate listener was not installed");
assert(committed, "onCommitted listener was not installed");

committed({
  tabId,
  frameId,
  parentFrameId: 1,
  url: `${extensionOrigin}webview_frame.html?nestedSession=${nestedSession}`,
});
beforeNavigate({ tabId, frameId, parentFrameId: 1, url: "https://fixture.example/first" });
await waitForMessageCount(sentMessages, 1);
beforeNavigate({ tabId, frameId, parentFrameId: 1, url: "https://fixture.example/second" });
await waitForMessageCount(sentMessages, 2);

assert.deepEqual(sentMessages, [
  {
    type: "rings.webview.navigate",
    tabId,
    url: "https://fixture.example/first",
    sessionId: nestedSession,
  },
  {
    type: "rings.webview.navigate",
    tabId,
    url: "https://fixture.example/second",
    sessionId: nestedSession,
  },
]);
delete (globalThis as typeof globalThis & { chrome?: unknown }).chrome;
console.log("Extension WebView service-worker routing passed");

/** Waits for one bounded asynchronous service-worker callback effect. */
async function waitForMessageCount(messages: readonly unknown[], expected: number): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (messages.length >= expected) return;
    await new Promise((resolve): void => {
      setTimeout(resolve, 10);
    });
  }
  assert.fail(`service worker emitted ${messages.length} of ${expected} expected navigation messages`);
}
