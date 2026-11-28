#!/usr/bin/env node

/**
 * Verifies the cross-language Extension WebView protocol constants and redirect laws.
 */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** Runtime surface imported from the generated shared protocol module. */
type ProtocolModule = {
  readonly CONTROLLED_WEBVIEW_ORIGIN: string;
  readonly WEBVIEW_GATEWAY_PREFIX: string;
  readonly redirectedWebviewRequest: (
    request: {
      readonly target: string;
      readonly method: string;
      readonly headers: readonly { readonly name: string; readonly value: string }[];
      readonly body: readonly number[];
      readonly credentials: "same-origin";
      readonly kind: "fetch";
      readonly topLevelNavigation: false;
      readonly redirect: "follow";
    },
    status: number,
    target: string,
  ) => {
    readonly target: string;
    readonly method: string;
    readonly headers: readonly { readonly name: string; readonly value: string }[];
    readonly body: readonly number[];
  };
  readonly parseRendererFrameMessage: (value: unknown) => Record<string, unknown> | undefined;
  readonly parseRendererRenderCommand: (value: unknown) => Record<string, unknown> | undefined;
  readonly parseRendererGatewayRequestMessage: (value: unknown) => Record<string, unknown> | undefined;
  readonly parseRendererPortMessage: (value: unknown) => Record<string, unknown> | undefined;
  readonly parseFrameGatewayResponse: (value: unknown) => {
    readonly status: number;
    readonly headers: readonly { readonly name: string; readonly value: string }[];
    readonly body: readonly number[];
    readonly url: string;
    readonly redirected: boolean;
  };
};

/** Runtime surface imported from the generated shared renderer-session module. */
type RendererSessionModule = {
  readonly createRendererLifecycle: () => RendererLifecycleFixture;
  readonly advanceRendererLifecycle: (
    state: RendererLifecycleStateFixture,
    action: RendererLifecycleActionFixture,
  ) => RendererLifecycleStepFixture;
  readonly createRendererGatewayLease: (
    hostPort: MessagePort,
    rendererPort: MessagePort,
    closePort: (port: MessagePort) => void,
  ) => RendererGatewayLeaseFixture;
  readonly renderRendererDocument: (request: {
    readonly lifecycle: RendererLifecycleFixture;
    readonly rendererGeneration: number;
    readonly target: string;
    readonly html: string;
    readonly owner: string;
    readonly effects: {
      readonly createCapability: () => string;
      readonly createGateway: () => RendererGatewayLeaseFixture;
      readonly installGateway: (port: MessagePort) => void;
      readonly postRender: (message: Record<string, unknown>, port: MessagePort) => void;
    };
  }) => Promise<void>;
};

/** Runtime surface imported from the shared extension effect boundary. */
type ExtensionRuntimeModule = {
  readonly createSerializedEffectQueue: () => {
    readonly enqueue: (effect: () => Promise<void>) => Promise<void>;
  };
};

/** Structural states exercised by the pure lifecycle law tests. */
type RendererLifecycleStateFixture =
  | { readonly kind: "created" | "awaiting-ready" | "ready" | "rendered" | "failed" | "released" }
  | { readonly kind: "awaiting-render"; readonly capability: string };

/** Structural inputs exercised by the pure lifecycle law tests. */
type RendererLifecycleActionFixture =
  | { readonly kind: "begin-ready" | "ready" | "pending-failed" | "release" }
  | { readonly kind: "begin-render" | "rendered" | "render-failed"; readonly capability: string };

/** Structural result of one pure lifecycle transition. */
type RendererLifecycleStepFixture =
  | { readonly kind: "accepted" | "ignored"; readonly state: RendererLifecycleStateFixture }
  | { readonly kind: "invalid"; readonly state: RendererLifecycleStateFixture; readonly message: string };

/** Effect interpreter surface exercised by lifecycle law tests. */
type RendererLifecycleFixture = {
  readonly waitUntilReady: (owner: string) => Promise<void>;
  readonly waitUntilRendered: (capability: string, owner: string) => Promise<void>;
  readonly apply: (
    event:
      | { readonly kind: "ready" }
      | { readonly kind: "rendered"; readonly capability: string }
      | { readonly kind: "render-failed"; readonly capability: string; readonly error: Error },
  ) => boolean;
  readonly release: (error: Error) => void;
};

/** Locally owned MessageChannel capability used by document-transfer tests. */
type RendererGatewayLeaseFixture = {
  readonly hostPort: MessagePort;
  readonly rendererPort: MessagePort;
  readonly release: () => void;
};

/** Runtime surface imported from the generated pure navigation module. */
type NavigationModule = {
  readonly initialNavigationState: () => NavigationStateFixture;
  readonly beginNavigation: (
    state: NavigationStateFixture,
    target: string,
    intent: { readonly kind: "push" | "reload" } | { readonly kind: "history"; readonly index: number },
  ) => NavigationStateFixture;
  readonly beginRendering: (
    state: NavigationStateFixture,
    generation: number,
    target: string,
  ) => NavigationStateFixture;
  readonly commitNavigation: (state: NavigationStateFixture, generation: number) => NavigationStateFixture;
  readonly failNavigation: (
    state: NavigationStateFixture,
    generation: number,
    message: string,
  ) => NavigationStateFixture;
};

/** Structural state fields asserted by the pure transition tests. */
type NavigationStateFixture = {
  readonly generation: number;
  readonly committedTarget?: string;
  readonly history: { readonly entries: readonly string[]; readonly index: number };
  readonly phase: { readonly kind: string; readonly target?: string; readonly message?: string };
};

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const rustHost = await readFile(resolve(projectRoot, "src", "webview.rs"), "utf8");
const protocolSource = await readFile(
  resolve(projectRoot, ".generated", "extension-assets", "webview_protocol.js"),
  "utf8",
);
const protocolUrl = `data:text/javascript;base64,${Buffer.from(protocolSource).toString("base64")}`;
const protocol = (await import(protocolUrl)) as ProtocolModule;
const rendererSessionSource = await readFile(
  resolve(projectRoot, ".generated", "extension-assets", "webview_renderer_session.js"),
  "utf8",
);
const rendererSessionUrl = `data:text/javascript;base64,${Buffer.from(rendererSessionSource).toString("base64")}`;
const rendererSession = (await import(rendererSessionUrl)) as RendererSessionModule;
const extensionRuntimeSource = await readFile(
  resolve(projectRoot, ".generated", "extension-assets", "extension_runtime.js"),
  "utf8",
);
const linkedExtensionRuntimeSource = extensionRuntimeSource.replace(
  '"./webview_protocol.js"',
  JSON.stringify(protocolUrl),
);
assert.notEqual(
  linkedExtensionRuntimeSource,
  extensionRuntimeSource,
  "extension runtime fixture did not link its generated protocol dependency",
);
const extensionRuntimeUrl = `data:text/javascript;base64,${Buffer.from(linkedExtensionRuntimeSource).toString("base64")}`;
const extensionRuntime = (await import(extensionRuntimeUrl)) as ExtensionRuntimeModule;
const navigationSource = await readFile(
  resolve(projectRoot, ".generated", "extension-assets", "webview_navigation_state.js"),
  "utf8",
);
const navigationUrl = `data:text/javascript;base64,${Buffer.from(navigationSource).toString("base64")}`;
const navigation = (await import(navigationUrl)) as NavigationModule;

assert.equal(protocol.WEBVIEW_GATEWAY_PREFIX, "/webview/");
assert.equal(protocol.CONTROLLED_WEBVIEW_ORIGIN, "https://rings-webview.invalid");
assert.match(rustHost, /GATEWAY_PREFIX:\s*&str\s*=\s*"\/webview\/"/);
assert.match(rustHost, /Url::parse\("https:\/\/rings-webview\.invalid\/"\)/);

assert.deepEqual(
  protocol.parseRendererFrameMessage({
    type: "rings.webview.frame.rendered",
    rendererGeneration: 7,
    renderCapability: "render-7",
  }),
  {
    type: "rings.webview.frame.rendered",
    rendererGeneration: 7,
    renderCapability: "render-7",
  },
);
assert.equal(
  protocol.parseRendererFrameMessage({
    type: "rings.webview.frame.rendered",
    rendererGeneration: 7,
    renderCapability: 7,
  }),
  undefined,
  "a malformed capability entered the renderer lifecycle algebra",
);
assert.equal(
  protocol.parseRendererRenderCommand({
    type: "rings.webview.render",
    rendererGeneration: 7,
    renderCapability: "render-7",
    target: "https://example.test/",
  }),
  undefined,
  "an incomplete render command crossed the opaque boundary",
);
assert.equal(
  protocol.parseRendererGatewayRequestMessage({
    type: "rings.webview.gateway.request",
    requestId: 0,
    request: {},
  }),
  undefined,
  "a non-positive request identity entered the private gateway",
);
assert.equal(
  protocol.parseRendererPortMessage({ type: "rings.webview.gateway.response", requestId: 1 }),
  undefined,
  "a response without a value or error settled a gateway request",
);
assert.deepEqual(
  protocol.parseFrameGatewayResponse({
    status: 200,
    headers: [],
    body: [],
    url: "https://user:secret@example.test/path",
    redirected: false,
  }),
  { status: 200, headers: [], body: [], url: "https://example.test/path", redirected: false },
  "a validated gateway URL retained credentials",
);
for (const status of [99, 600, -1, 1.5]) {
  assert.throws(
    (): unknown =>
      protocol.parseFrameGatewayResponse({
        status,
        headers: [],
        body: [],
        url: "https://example.test/",
        redirected: false,
      }),
    /invalid renderer gateway response/,
    `invalid HTTP status ${status} entered the response domain`,
  );
}
for (const status of [100, 599]) {
  assert.equal(
    protocol.parseFrameGatewayResponse({
      status,
      headers: [],
      body: [],
      url: "https://example.test/",
      redirected: false,
    }).status,
    status,
    `HTTP boundary status ${status} was rejected`,
  );
}
for (const url of ["relative/path", "http://example.test/", "not a URL"]) {
  assert.throws(
    (): unknown => protocol.parseFrameGatewayResponse({ status: 200, headers: [], body: [], url, redirected: false }),
    /Rings onion WebView accepts HTTPS targets only|Invalid URL/,
    `invalid response URL ${url} entered the HTTPS response domain`,
  );
}

const urlConstructorDescriptor = Object.getOwnPropertyDescriptor(globalThis, "URL");
const numberIntegerDescriptor = Object.getOwnPropertyDescriptor(Number, "isInteger");
assert(urlConstructorDescriptor);
assert(numberIntegerDescriptor);
try {
  Object.defineProperty(globalThis, "URL", {
    configurable: true,
    value: class ForgedUrl {
      readonly protocol = "https:";
      readonly href = "https://forged.invalid/";
      username = "";
      password = "";
    },
  });
  Object.defineProperty(Number, "isInteger", { configurable: true, value: (): boolean => true });
  assert.throws(
    (): unknown =>
      protocol.parseFrameGatewayResponse({
        status: 700,
        headers: [],
        body: [],
        url: "https://example.test/",
        redirected: false,
      }),
    /invalid renderer gateway response/,
    "a patched Number.isInteger bypassed the captured response boundary",
  );
  assert.throws(
    (): unknown =>
      protocol.parseFrameGatewayResponse({
        status: 200,
        headers: [],
        body: [],
        url: "http://example.test/",
        redirected: false,
      }),
    /Rings onion WebView accepts HTTPS targets only/,
    "a patched URL constructor bypassed the captured HTTPS boundary",
  );
} finally {
  Object.defineProperty(globalThis, "URL", urlConstructorDescriptor);
  Object.defineProperty(Number, "isInteger", numberIntegerDescriptor);
}

const createdState = { kind: "created" } as const;
const awaitingReadyStep = rendererSession.advanceRendererLifecycle(createdState, { kind: "begin-ready" });
assert.deepEqual(awaitingReadyStep, { kind: "accepted", state: { kind: "awaiting-ready" } });
const readyStep = rendererSession.advanceRendererLifecycle(awaitingReadyStep.state, { kind: "ready" });
assert.deepEqual(readyStep, { kind: "accepted", state: { kind: "ready" } });
const awaitingRenderStep = rendererSession.advanceRendererLifecycle(readyStep.state, {
  kind: "begin-render",
  capability: "expected",
});
assert.deepEqual(awaitingRenderStep, {
  kind: "accepted",
  state: { kind: "awaiting-render", capability: "expected" },
});
assert.deepEqual(
  rendererSession.advanceRendererLifecycle(awaitingRenderStep.state, {
    kind: "rendered",
    capability: "forged",
  }),
  { kind: "ignored", state: awaitingRenderStep.state },
  "a forged capability changed the pure renderer lifecycle",
);
assert.deepEqual(
  rendererSession.advanceRendererLifecycle(awaitingRenderStep.state, {
    kind: "rendered",
    capability: "expected",
  }),
  { kind: "accepted", state: { kind: "rendered" } },
);

const lifecycleStates: readonly RendererLifecycleStateFixture[] = [
  { kind: "created" },
  { kind: "awaiting-ready" },
  { kind: "ready" },
  { kind: "awaiting-render", capability: "expected" },
  { kind: "rendered" },
  { kind: "failed" },
  { kind: "released" },
];
for (const state of lifecycleStates) {
  assert.equal(
    rendererSession.advanceRendererLifecycle(state, { kind: "begin-ready" }).kind,
    state.kind === "created" ? "accepted" : "invalid",
    `begin-ready admitted ${state.kind}`,
  );
  assert.equal(
    rendererSession.advanceRendererLifecycle(state, { kind: "ready" }).kind,
    state.kind === "awaiting-ready" ? "accepted" : "ignored",
    `ready acknowledgement admitted ${state.kind}`,
  );
  assert.equal(
    rendererSession.advanceRendererLifecycle(state, { kind: "begin-render", capability: "expected" }).kind,
    state.kind === "ready" ? "accepted" : "invalid",
    `begin-render admitted ${state.kind}`,
  );
  assert.equal(
    rendererSession.advanceRendererLifecycle(state, { kind: "rendered", capability: "expected" }).kind,
    state.kind === "awaiting-render" ? "accepted" : "ignored",
    `rendered acknowledgement admitted ${state.kind}`,
  );
  assert.equal(
    rendererSession.advanceRendererLifecycle(state, { kind: "render-failed", capability: "expected" }).kind,
    state.kind === "awaiting-render" ? "accepted" : "ignored",
    `render failure admitted ${state.kind}`,
  );
  assert.equal(
    rendererSession.advanceRendererLifecycle(state, { kind: "pending-failed" }).kind,
    state.kind === "awaiting-ready" || state.kind === "awaiting-render" ? "accepted" : "ignored",
    `pending failure admitted ${state.kind}`,
  );
  assert.equal(
    rendererSession.advanceRendererLifecycle(state, { kind: "release" }).kind,
    state.kind === "released" ? "ignored" : "accepted",
    `release mishandled ${state.kind}`,
  );
  assert.equal(
    rendererSession.advanceRendererLifecycle(state, { kind: "rendered", capability: "forged" }).kind,
    "ignored",
    `forged capability admitted ${state.kind}`,
  );
}

const lifecycle = rendererSession.createRendererLifecycle();
assert.throws(
  (): Promise<void> => lifecycle.waitUntilRendered("out-of-order", "fixture renderer"),
  /renderer render transition is not permitted from created/,
  "render began before the ready state",
);
const ready = lifecycle.waitUntilReady("fixture renderer");
assert.throws(
  (): Promise<void> => lifecycle.waitUntilReady("fixture renderer"),
  /renderer ready transition is not permitted from awaiting-ready/,
);
assert(!lifecycle.apply({ kind: "rendered", capability: "out-of-order" }));
assert(lifecycle.apply({ kind: "ready" }));
await ready;
const rendered = lifecycle.waitUntilRendered("expected", "fixture renderer");
assert(!lifecycle.apply({ kind: "rendered", capability: "forged" }));
assert(lifecycle.apply({ kind: "rendered", capability: "expected" }));
await rendered;

const releasedLifecycle = rendererSession.createRendererLifecycle();
const released = releasedLifecycle.waitUntilReady("fixture renderer");
releasedLifecycle.release(new DOMException("superseded", "AbortError"));
await assert.rejects(released, { name: "AbortError", message: "superseded" });

const documentLifecycle = rendererSession.createRendererLifecycle();
const documentReady = documentLifecycle.waitUntilReady("fixture renderer");
assert(documentLifecycle.apply({ kind: "ready" }));
await documentReady;
let postedRender: Record<string, unknown> | undefined;
await rendererSession.renderRendererDocument({
  lifecycle: documentLifecycle,
  rendererGeneration: 9,
  target: "https://example.test/document",
  html: "<p>document</p>",
  owner: "fixture renderer",
  effects: {
    createCapability: (): string => "render-9",
    createGateway: (): RendererGatewayLeaseFixture => {
      const gateway = new MessageChannel();
      return rendererSession.createRendererGatewayLease(gateway.port1, gateway.port2, (port: MessagePort): void =>
        port.close(),
      );
    },
    installGateway: (port: MessagePort): void => port.close(),
    postRender: (message: Record<string, unknown>, port: MessagePort): void => {
      postedRender = message;
      port.close();
      queueMicrotask((): void => {
        documentLifecycle.apply({ kind: "rendered", capability: "render-9" });
      });
    },
  },
});
assert.deepEqual(postedRender, {
  type: "rings.webview.render",
  rendererGeneration: 9,
  renderCapability: "render-9",
  target: "https://example.test/document",
  html: "<p>document</p>",
});
assert.deepEqual(
  protocol.parseRendererRenderCommand(postedRender),
  postedRender,
  "the shared render constructor and parser did not round-trip",
);

const failedDocumentLifecycle = rendererSession.createRendererLifecycle();
const failedDocumentReady = failedDocumentLifecycle.waitUntilReady("fixture renderer");
assert(failedDocumentLifecycle.apply({ kind: "ready" }));
await failedDocumentReady;
await assert.rejects(
  rendererSession.renderRendererDocument({
    lifecycle: failedDocumentLifecycle,
    rendererGeneration: 10,
    target: "https://example.test/failure",
    html: "",
    owner: "fixture renderer",
    effects: {
      createCapability: (): string => "render-10",
      createGateway: (): never => {
        throw new Error("gateway allocation failed");
      },
      installGateway: (): void => {},
      postRender: (): void => {},
    },
  }),
  { message: "gateway allocation failed" },
);
assert.throws(
  (): Promise<void> => failedDocumentLifecycle.waitUntilRendered("recovered", "fixture renderer"),
  /renderer render transition is not permitted from failed/,
  "a failed document transfer returned to a reusable ready state",
);

const installFailureLifecycle = rendererSession.createRendererLifecycle();
const installFailureReady = installFailureLifecycle.waitUntilReady("install failure renderer");
assert(installFailureLifecycle.apply({ kind: "ready" }));
await installFailureReady;
let installFailureClosedPorts = 0;
await assert.rejects(
  rendererSession.renderRendererDocument({
    lifecycle: installFailureLifecycle,
    rendererGeneration: 11,
    target: "https://example.test/install-failure",
    html: "",
    owner: "install failure renderer",
    effects: {
      createCapability: (): string => "render-11",
      createGateway: (): RendererGatewayLeaseFixture => {
        const gateway = new MessageChannel();
        return rendererSession.createRendererGatewayLease(gateway.port1, gateway.port2, (port: MessagePort): void => {
          installFailureClosedPorts += 1;
          port.close();
        });
      },
      installGateway: (): never => {
        throw new Error("gateway installation failed");
      },
      postRender: (): void => {},
    },
  }),
  { message: "gateway installation failed" },
);
assert.equal(installFailureClosedPorts, 2, "install failure did not close both locally owned ports");

const postFailureLifecycle = rendererSession.createRendererLifecycle();
const postFailureReady = postFailureLifecycle.waitUntilReady("post failure renderer");
assert(postFailureLifecycle.apply({ kind: "ready" }));
await postFailureReady;
let postFailureClosedPorts = 0;
let postFailureLease: RendererGatewayLeaseFixture | undefined;
await assert.rejects(
  rendererSession.renderRendererDocument({
    lifecycle: postFailureLifecycle,
    rendererGeneration: 12,
    target: "https://example.test/post-failure",
    html: "",
    owner: "post failure renderer",
    effects: {
      createCapability: (): string => "render-12",
      createGateway: (): RendererGatewayLeaseFixture => {
        const gateway = new MessageChannel();
        postFailureLease = rendererSession.createRendererGatewayLease(
          gateway.port1,
          gateway.port2,
          (port: MessagePort): void => {
            postFailureClosedPorts += 1;
            port.close();
          },
        );
        return postFailureLease;
      },
      installGateway: (): void => {},
      postRender: (): never => {
        throw new Error("renderer post failed");
      },
    },
  }),
  { message: "renderer post failed" },
);
postFailureLease?.release();
assert.equal(postFailureClosedPorts, 2, "post failure cleanup was not idempotent for both ports");

const effectOrder: string[] = [];
const effectQueue = extensionRuntime.createSerializedEffectQueue();
const failedEffect = effectQueue.enqueue(async (): Promise<void> => {
  effectOrder.push("failed");
  throw new Error("expected queue failure");
});
const laterEffect = effectQueue.enqueue(async (): Promise<void> => {
  effectOrder.push("later");
});
await assert.rejects(failedEffect, { message: "expected queue failure" });
await laterEffect;
assert.deepEqual(effectOrder, ["failed", "later"], "a failed serialized effect poisoned later independent work");

const post = {
  target: "https://example.test/submit",
  method: "POST",
  headers: [
    { name: "Content-Type", value: "application/json" },
    { name: "Accept", value: "application/json" },
  ],
  body: [1, 2, 3],
  credentials: "same-origin" as const,
  kind: "fetch" as const,
  topLevelNavigation: false as const,
  redirect: "follow" as const,
};
const redirected = protocol.redirectedWebviewRequest(post, 303, "https://example.test/result");
assert.equal(redirected.target, "https://example.test/result");
assert.equal(redirected.method, "GET");
assert.deepEqual(redirected.body, []);

const crossOrigin = protocol.redirectedWebviewRequest(
  { ...post, headers: [...post.headers, { name: "Authorization", value: "secret" }] },
  302,
  "https://other.test/result",
);
assert.equal(crossOrigin.method, "GET");
assert.deepEqual(
  crossOrigin.headers.map((header): string => header.name),
  ["Accept"],
  "a cross-origin POST redirect retained body or authority headers",
);
const preserved = protocol.redirectedWebviewRequest(post, 307, "https://example.test/retry");
assert.equal(preserved.method, "POST");
assert.deepEqual(preserved.body, post.body);

let state = navigation.beginNavigation(navigation.initialNavigationState(), "https://example.test/a", { kind: "push" });
state = navigation.beginRendering(state, state.generation, "https://example.test/final");
const committed = navigation.commitNavigation(state, state.generation);
assert.equal(committed.committedTarget, "https://example.test/final");
assert.equal(committed.phase.target, committed.committedTarget);
assert.equal(committed.history.entries[committed.history.index], committed.committedTarget);

let reloaded = navigation.beginNavigation(committed, committed.committedTarget, { kind: "reload" });
reloaded = navigation.beginRendering(reloaded, reloaded.generation, "https://example.test/reload-redirect");
reloaded = navigation.commitNavigation(reloaded, reloaded.generation);
assert.equal(reloaded.history.entries[reloaded.history.index], reloaded.committedTarget);
assert.deepEqual(reloaded.history.entries, ["https://example.test/reload-redirect"]);

let invalidHistory = navigation.beginNavigation(reloaded, "https://example.test/fallback", {
  kind: "history",
  index: 99,
});
invalidHistory = navigation.beginRendering(invalidHistory, invalidHistory.generation, "https://example.test/fallback");
invalidHistory = navigation.commitNavigation(invalidHistory, invalidHistory.generation);
assert.equal(invalidHistory.history.entries[invalidHistory.history.index], invalidHistory.committedTarget);

const pendingBack = navigation.beginNavigation(committed, "https://example.test/a", { kind: "history", index: 0 });
assert.deepEqual(pendingBack.history, committed.history, "history mutated before the render commit");
const failedBack = navigation.failNavigation(pendingBack, pendingBack.generation, "render failed");
assert.equal(failedBack.committedTarget, committed.committedTarget);
assert.deepEqual(failedBack.history, committed.history);
assert.strictEqual(
  navigation.commitNavigation(state, state.generation + 1),
  state,
  "a stale generation changed navigation state",
);
assert.strictEqual(
  navigation.beginRendering(committed, committed.generation + 1, "https://example.test/stale"),
  committed,
  "a non-fetching generation entered the render phase",
);
for (const staleGeneration of [0, pendingBack.generation - 1, pendingBack.generation + 1]) {
  assert.strictEqual(
    navigation.failNavigation(pendingBack, staleGeneration, "stale failure"),
    pendingBack,
    "a stale failure erased the active transition",
  );
}

let second = navigation.beginNavigation(committed, "https://example.test/b", { kind: "push" });
second = navigation.beginRendering(second, second.generation, "https://example.test/b");
second = navigation.commitNavigation(second, second.generation);
let back = navigation.beginNavigation(second, "https://example.test/a", { kind: "history", index: 0 });
back = navigation.beginRendering(back, back.generation, "https://example.test/a");
back = navigation.commitNavigation(back, back.generation);
let replacement = navigation.beginNavigation(back, "https://example.test/c", { kind: "push" });
replacement = navigation.beginRendering(replacement, replacement.generation, "https://example.test/c");
replacement = navigation.commitNavigation(replacement, replacement.generation);
assert.deepEqual(
  replacement.history.entries,
  ["https://example.test/a", "https://example.test/c"],
  "a push after Back retained an unreachable forward-history suffix",
);

let duplicate = navigation.beginNavigation(replacement, "https://example.test/c", { kind: "push" });
duplicate = navigation.beginRendering(duplicate, duplicate.generation, "https://example.test/c");
duplicate = navigation.commitNavigation(duplicate, duplicate.generation);
assert.deepEqual(duplicate.history, replacement.history, "a duplicate committed push created redundant history");

console.log("Extension WebView protocol contract passed");

/** Resolves the frontend root from source or generated-script execution. */
function frontendProjectRoot(currentScriptDir: string): string {
  const parentDir = dirname(currentScriptDir);
  return basename(parentDir) === ".generated" ? resolve(parentDir, "..") : resolve(currentScriptDir, "..");
}
