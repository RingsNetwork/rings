// biome-ignore-all lint/complexity/useLiteralKeys: Worker bridge messages are untrusted records.
/**
 * Fail-closed Worker facades whose scripts and runtime fetches cross the trusted WebView bridge.
 */

import type { FrameGatewayRequest, FrameGatewayResponse, WebviewCredentials } from "./webview_protocol.js";
import { errorMessage, isWebviewCredentials, parseFrameGatewayRequest } from "./webview_protocol.js";

/** Effects supplied by the opaque renderer to isolated worker facades. */
type WorkerBridge = {
  readonly currentTarget: () => string;
  readonly normalizeTarget: (value: string, base: string) => string;
  readonly request: (request: FrameGatewayRequest) => Promise<FrameGatewayResponse>;
  readonly reportError: (message: string) => void;
};

/** Renderer-owned effect lifetime for every Worker created by one remote document. */
type WorkerBridgeRuntime = {
  readonly release: () => void;
};

/** One page message retained until the asynchronous worker bootstrap commits. */
type QueuedWorkerMessage = {
  readonly value: unknown;
  readonly transfer?: Transferable[];
};

/** Shared worker identity and its single initialization transition. */
type SharedWorkerEntry = {
  worker?: Worker;
  ready: Promise<void>;
  readonly errorListeners: Set<(error: unknown) => void>;
};

/** Explicit lifecycle of one dedicated worker facade. */
type DedicatedWorkerState =
  | { readonly kind: "loading" }
  | { readonly kind: "initializing"; readonly worker: Worker }
  | { readonly kind: "ready"; readonly worker: Worker }
  | { readonly kind: "failed" }
  | { readonly kind: "terminated" };

/** Generation witness and ownership effects for one asynchronous Worker creation. */
type WorkerOwnership = {
  readonly adopt: (worker: Worker) => boolean;
  readonly disown: (worker: Worker) => void;
};

/** Document-scoped Worker identities and native effects. */
type WorkerRuntimeState = {
  readonly sharedWorkers: Map<string, SharedWorkerEntry>;
  readonly captureOwnership: () => WorkerOwnership;
};

/** Source produced by one isolated classic parser or module bundler. */
type MaterializedWorkerGraph = { readonly code: string };

/** Validated dependency request emitted by the isolated bundler. */
type BundlerSourceRequest = {
  readonly requestId: number;
  readonly target: string;
  readonly credentials: WebviewCredentials;
};

/** Validated terminal result emitted by the isolated bundler. */
type BundlerResult = { readonly code?: string; readonly error?: string };

/** One-shot packaged source installed before this module enters the opaque renderer. */
type ModuleBundlerSourceGlobal = typeof globalThis & {
  __ringsWebviewModuleBundlerSource?: unknown;
};

const NativeWorker = globalThis.Worker;
const NativeMessageChannel = globalThis.MessageChannel;
const nativeEventAdd = globalThis.EventTarget.prototype.addEventListener;
const nativeWorkerPostMessage = globalThis.Worker.prototype.postMessage;
const nativeWorkerTerminate = globalThis.Worker.prototype.terminate;
const nativeStructuredClone = globalThis.structuredClone.bind(globalThis);
const nativeRandomUuid = globalThis.crypto.randomUUID.bind(globalThis.crypto);
const nativePortPostMessage = globalThis.MessagePort.prototype.postMessage;
const nativePortStart = globalThis.MessagePort.prototype.start;
const nativePortClose = globalThis.MessagePort.prototype.close;
const nativeSetTimeout = globalThis.setTimeout.bind(globalThis);
const nativeClearTimeout = globalThis.clearTimeout.bind(globalThis);
const workerGatewayPorts = new WeakMap<Worker, MessagePort>();
const workerModuleUrls = new WeakMap<Worker, string>();
const workerInitializationCancels = new WeakMap<Worker, (error: Error) => void>();
const moduleBundlerSource = sharedModuleBundlerSource();
const acornUrl = new URL("./acorn.mjs", import.meta.url).href;
const moduleLexerUrl = new URL("./es-module-lexer.js", import.meta.url).href;
const rollupUrl = new URL("./rollup.browser.js", import.meta.url).href;
const MODULE_BUNDLE_TIMEOUT_MS = 30_000;
const requestBodyRuntimeSource = sharedRequestBodyRuntimeSource();

/** Installs document-scoped Worker constructors before the shared browser bootstrap runs. */
export function installWorkerBridge(bridge: WorkerBridge): WorkerBridgeRuntime {
  const ownedWorkers = new Set<Worker>();
  const sharedWorkers = new Map<string, SharedWorkerEntry>();
  let generation = 0;
  const runtime: WorkerRuntimeState = {
    sharedWorkers,
    captureOwnership: (): WorkerOwnership => {
      const ownerGeneration = generation;
      return {
        adopt: (worker: Worker): boolean => {
          if (ownerGeneration !== generation) return false;
          ownedWorkers.add(worker);
          return true;
        },
        disown: (worker: Worker): void => {
          ownedWorkers.delete(worker);
        },
      };
    },
  };
  Object.defineProperty(globalThis, "Worker", {
    configurable: true,
    value: bridgedWorkerConstructor(bridge, runtime),
    writable: true,
  });
  Object.defineProperty(globalThis, "SharedWorker", {
    configurable: true,
    value: bridgedSharedWorkerConstructor(bridge, runtime),
    writable: true,
  });
  return {
    release: (): void => {
      generation += 1;
      for (const worker of ownedWorkers) {
        cancelWorkerInitialization(worker, new DOMException("Worker document was superseded", "AbortError"));
        closeWorkerGateway(worker);
        nativeWorkerTerminate.call(worker);
      }
      ownedWorkers.clear();
      sharedWorkers.clear();
    },
  };
}

/** Parses the closed Worker script-kind algebra at the authored-code boundary. */
function workerType(value: unknown): WorkerType {
  if (value === undefined || value === "classic") return "classic";
  if (value === "module") return "module";
  throw new TypeError("Worker type must be classic or module");
}

/** Applies the browser's module-worker credential default and classic-worker law. */
function workerScriptCredentials(type: WorkerType, value: unknown): WebviewCredentials {
  if (type === "classic") return "same-origin";
  if (value === undefined || value === "same-origin") return "same-origin";
  if (value === "omit" || value === "include") return value;
  throw new TypeError("Worker credentials must be omit, same-origin, or include");
}

/** Builds the dedicated Worker constructor bound to one trusted bridge. */
function bridgedWorkerConstructor(bridge: WorkerBridge, runtime: WorkerRuntimeState) {
  return class RingsWorker extends EventTarget {
    onerror: ((this: AbstractWorker, ev: ErrorEvent) => unknown) | null = null;
    onmessage: ((this: Worker, ev: MessageEvent) => unknown) | null = null;
    onmessageerror: ((this: Worker, ev: MessageEvent) => unknown) | null = null;
    private state: DedicatedWorkerState = { kind: "loading" };
    private readonly queued: QueuedWorkerMessage[] = [];
    private readonly ownership: WorkerOwnership;

    constructor(scriptURL: string | URL, options?: WorkerOptions) {
      super();
      const target = bridge.normalizeTarget(String(scriptURL), bridge.currentTarget());
      const type = workerType(options?.type);
      const credentials = workerScriptCredentials(type, options?.credentials);
      const ownership = runtime.captureOwnership();
      this.ownership = ownership;
      void createWorker(
        target,
        type,
        credentials,
        options?.name,
        "dedicated",
        bridge,
        ownership,
        (worker: Worker): boolean => {
          if (this.state.kind === "terminated") return false;
          this.state = { kind: "initializing", worker };
          this.forwardEvents(worker);
          return true;
        },
      )
        .then((worker: Worker): void => {
          if (this.state.kind !== "initializing" || this.state.worker !== worker) return;
          this.state = { kind: "ready", worker };
          for (const message of this.queued.splice(0)) {
            postWorkerMessage(worker, message);
          }
        })
        .catch((error: unknown): void => {
          if (this.state.kind === "terminated") return;
          if (this.state.kind === "initializing") ownership.disown(this.state.worker);
          this.state = { kind: "failed" };
          this.queued.splice(0);
          this.emitError(error);
        });
    }

    postMessage(message: unknown, transferOrOptions?: Transferable[] | StructuredSerializeOptions): void {
      const transfer = Array.isArray(transferOrOptions) ? transferOrOptions : transferOrOptions?.transfer;
      if (this.state.kind === "ready") {
        postWorkerMessage(
          this.state.worker,
          transfer ? { value: message, transfer: [...transfer] } : { value: message },
        );
      } else if (this.state.kind === "loading" || this.state.kind === "initializing") {
        this.queued.push(snapshotWorkerMessage(message, transfer));
      }
    }

    terminate(): void {
      const worker = this.state.kind === "initializing" || this.state.kind === "ready" ? this.state.worker : undefined;
      this.state = { kind: "terminated" };
      this.queued.splice(0);
      if (worker) this.ownership.disown(worker);
      if (worker) cancelWorkerInitialization(worker, new DOMException("Worker was terminated", "AbortError"));
      if (worker) closeWorkerGateway(worker);
      if (worker) nativeWorkerTerminate.call(worker);
    }

    private forwardEvents(worker: Worker): void {
      worker.addEventListener("message", (event: MessageEvent): void => {
        const forwarded = new MessageEvent("message", {
          data: event.data,
          origin: event.origin,
          ports: [...event.ports],
        });
        this.dispatchEvent(forwarded);
        this.onmessage?.call(this, forwarded);
      });
      worker.addEventListener("messageerror", (event: MessageEvent): void => {
        const forwarded = new MessageEvent("messageerror", { data: event.data });
        this.dispatchEvent(forwarded);
        this.onmessageerror?.call(this, forwarded);
      });
      worker.addEventListener("error", (event: ErrorEvent): void => this.emitError(event));
    }

    private emitError(error: unknown): void {
      const event = workerErrorEvent(error);
      bridge.reportError(event.message);
      this.dispatchEvent(event);
      this.onerror?.call(this, event);
    }
  };
}

/**
 * Applies Worker.postMessage's structured-clone and ownership-transfer effect at call time.
 *
 * Law: mutating `value` after this function returns cannot change `result.value`.
 * Post: every transferable accepted by the clone operation is detached from the caller.
 */
function snapshotWorkerMessage(value: unknown, transfer?: Transferable[]): QueuedWorkerMessage {
  if (!transfer) return { value: nativeStructuredClone(value) };
  const ownedTransfer = [...transfer];
  const snapshot = nativeStructuredClone({ value, transfer: ownedTransfer }, { transfer: ownedTransfer }) as {
    readonly value: unknown;
    readonly transfer: Transferable[];
  };
  return { value: snapshot.value, transfer: snapshot.transfer };
}

/** Builds the document-scoped SharedWorker constructor and identity registry. */
function bridgedSharedWorkerConstructor(bridge: WorkerBridge, runtime: WorkerRuntimeState) {
  return class RingsSharedWorker extends EventTarget {
    onerror: ((this: AbstractWorker, ev: ErrorEvent) => unknown) | null = null;
    readonly port: MessagePort;

    constructor(scriptURL: string | URL, options?: string | WorkerOptions) {
      super();
      const normalized = typeof options === "string" ? { name: options } : options;
      const target = bridge.normalizeTarget(String(scriptURL), bridge.currentTarget());
      const type = workerType(normalized?.type);
      const credentials = workerScriptCredentials(type, normalized?.credentials);
      const key = `${target}\u0000${normalized?.name ?? ""}\u0000${type}\u0000${credentials}`;
      const channel = new NativeMessageChannel();
      this.port = channel.port1;
      let entry = runtime.sharedWorkers.get(key);
      if (!entry) {
        const ownership = runtime.captureOwnership();
        const stableEntry: SharedWorkerEntry = { ready: Promise.resolve(), errorListeners: new Set() };
        stableEntry.ready = createWorker(target, type, credentials, normalized?.name, "shared", bridge, ownership)
          .then((worker: Worker): void => {
            stableEntry.worker = worker;
            worker.addEventListener("error", (event: ErrorEvent): void => {
              for (const listener of stableEntry.errorListeners) listener(event);
            });
          })
          .catch((error: unknown): never => {
            if (runtime.sharedWorkers.get(key) === stableEntry) runtime.sharedWorkers.delete(key);
            throw error;
          });
        entry = stableEntry;
        runtime.sharedWorkers.set(key, stableEntry);
      }
      entry.errorListeners.add((error: unknown): void => this.emitError(error, bridge));
      void entry.ready
        .then((): void => {
          if (entry?.worker) {
            nativeWorkerPostMessage.call(entry.worker, { type: "rings.worker.connect" }, { transfer: [channel.port2] });
          }
          this.port.start();
        })
        .catch((error: unknown): void => {
          this.port.close();
          this.emitError(error, bridge);
        });
    }

    private emitError(error: unknown, currentBridge: WorkerBridge): void {
      const event = workerErrorEvent(error);
      currentBridge.reportError(event.message);
      this.dispatchEvent(event);
      this.onerror?.call(this, event);
    }
  };
}

/** Onion-fetches one worker script and starts its fail-closed bootstrap. */
async function createWorker(
  target: string,
  type: WorkerType,
  credentials: WebviewCredentials,
  name: string | undefined,
  mode: "dedicated" | "shared",
  bridge: WorkerBridge,
  ownership: WorkerOwnership,
  onCreated?: (worker: Worker) => boolean,
): Promise<Worker> {
  const scriptResponse = await bridge.request({
    target,
    method: "GET",
    headers: [],
    body: [],
    credentials,
    kind: "subresource",
    topLevelNavigation: false,
    redirect: "follow",
  });
  if (scriptResponse.status < 200 || scriptResponse.status >= 300) {
    throw new Error(`worker ${target} returned HTTP ${scriptResponse.status}`);
  }
  const source = new TextDecoder().decode(Uint8Array.from(scriptResponse.body));
  const graph = await materializeWorkerGraph(type, scriptResponse.url, source, credentials, bridge);
  const classicSource = type === "classic" ? graph.code : source;
  const moduleUrl =
    type === "module" ? URL.createObjectURL(new Blob([graph.code], { type: "text/javascript" })) : undefined;
  const wrapper = workerBootstrapSource(classicSource, scriptResponse.url, mode, moduleUrl);
  const wrapperUrl = URL.createObjectURL(new Blob([wrapper], { type: "text/javascript" }));
  // The bridge wrapper is classic in both modes; a module entry is evaluated with dynamic import
  // after the private gateway port is installed, so it cannot run before network isolation.
  let worker: Worker;
  try {
    worker = new NativeWorker(wrapperUrl, { ...(name ? { name } : {}) });
  } catch (error: unknown) {
    URL.revokeObjectURL(wrapperUrl);
    if (moduleUrl) URL.revokeObjectURL(moduleUrl);
    throw error;
  }
  if (moduleUrl) workerModuleUrls.set(worker, moduleUrl);
  let gateway: MessageChannel;
  try {
    gateway = new NativeMessageChannel();
    workerGatewayPorts.set(worker, gateway.port1);
    installGatewayPort(gateway.port1, bridge);
  } catch (error: unknown) {
    URL.revokeObjectURL(wrapperUrl);
    closeWorkerGateway(worker);
    nativeWorkerTerminate.call(worker);
    throw error;
  }
  if (!ownership.adopt(worker)) {
    URL.revokeObjectURL(wrapperUrl);
    closeWorkerGateway(worker);
    nativeWorkerTerminate.call(worker);
    ownership.disown(worker);
    throw new DOMException("Worker was superseded before initialization", "AbortError");
  }
  try {
    await initializeWorker(worker, gateway.port2, wrapperUrl, onCreated);
    return worker;
  } catch (error: unknown) {
    ownership.disown(worker);
    throw error;
  }
}

/**
 * Waits for one native worker to acknowledge bridge installation.
 *
 * Invariant W1: the capability is sampled from a native primitive captured
 * before authored code and is transferred only in the intercepted init event.
 * Postcondition: queued page messages cannot commit before the matching ACK.
 */
function initializeWorker(
  worker: Worker,
  gatewayPort: MessagePort,
  wrapperUrl: string,
  onPrepared: ((worker: Worker) => boolean) | undefined,
): Promise<void> {
  const capability = nativeRandomUuid();
  return new Promise((resolve, reject): void => {
    let timeout = 0;
    const finish = (): void => {
      globalThis.clearTimeout(timeout);
      worker.removeEventListener("message", onMessage, true);
      worker.removeEventListener("error", onError);
      workerInitializationCancels.delete(worker);
      URL.revokeObjectURL(wrapperUrl);
    };
    const onMessage = (event: MessageEvent<unknown>): void => {
      if (!isWorkerReadyMessage(event.data, capability)) return;
      event.stopImmediatePropagation();
      finish();
      resolve();
    };
    const onError = (event: ErrorEvent): void => {
      cancelWorkerInitialization(worker, new Error(event.message || "worker bridge initialization failed"));
      closeWorkerGateway(worker);
      nativeWorkerTerminate.call(worker);
    };
    timeout = globalThis.setTimeout((): void => {
      cancelWorkerInitialization(worker, new Error("worker bridge initialization timed out"));
      closeWorkerGateway(worker);
      nativeWorkerTerminate.call(worker);
    }, 10_000);
    workerInitializationCancels.set(worker, (error: Error): void => {
      finish();
      reject(error);
    });
    worker.addEventListener("message", onMessage, { capture: true });
    worker.addEventListener("error", onError, { once: true });
    if (onPrepared && !onPrepared(worker)) {
      cancelWorkerInitialization(worker, new DOMException("Worker was terminated before initialization", "AbortError"));
      closeWorkerGateway(worker);
      nativeWorkerTerminate.call(worker);
      return;
    }
    nativeWorkerPostMessage.call(worker, { type: "rings.worker.initialize", capability }, { transfer: [gatewayPort] });
  });
}

/** Rejects one outstanding initialization transition exactly once. */
function cancelWorkerInitialization(worker: Worker, error: Error): void {
  workerInitializationCancels.get(worker)?.(error);
}

/** Releases the private gateway port owned by a terminated worker. */
function closeWorkerGateway(worker: Worker): void {
  const port = workerGatewayPorts.get(worker);
  if (port) nativePortClose.call(port);
  workerGatewayPorts.delete(worker);
  const moduleUrl = workerModuleUrls.get(worker);
  if (moduleUrl) URL.revokeObjectURL(moduleUrl);
  workerModuleUrls.delete(worker);
}

/**
 * Runs classic parsing or module bundling only after a Worker is requested and in a pristine realm.
 *
 * Post: authored source is returned only after its complete dependency graph crosses the bridge.
 */
function materializeWorkerGraph(
  type: WorkerType,
  entryTarget: string,
  entrySource: string,
  credentials: WebviewCredentials,
  bridge: WorkerBridge,
): Promise<MaterializedWorkerGraph> {
  return new Promise((resolve, reject): void => {
    const bundlerUrl = URL.createObjectURL(new Blob([moduleBundlerSource], { type: "text/javascript" }));
    let bundler: Worker;
    try {
      bundler = new NativeWorker(bundlerUrl, { name: "rings-module-worker-bundler" });
    } catch (error: unknown) {
      URL.revokeObjectURL(bundlerUrl);
      reject(error instanceof Error ? error : new Error(errorMessage(error)));
      return;
    }
    const channel = new NativeMessageChannel();
    let settled = false;
    const timeout = nativeSetTimeout((): void => {
      settle(undefined, new Error("module Worker bundling timed out"));
    }, MODULE_BUNDLE_TIMEOUT_MS);
    const settle = (code?: string, error?: Error): void => {
      if (settled) return;
      settled = true;
      nativeClearTimeout(timeout);
      nativePortClose.call(channel.port1);
      nativeWorkerTerminate.call(bundler);
      URL.revokeObjectURL(bundlerUrl);
      if (error) reject(error);
      else if (code === undefined) reject(new Error("isolated Worker materializer returned no code"));
      else resolve({ code });
    };
    nativeEventAdd.call(channel.port1, "message", ((event: MessageEvent<unknown>): void => {
      const sourceRequest = parseBundlerSourceRequest(event.data);
      if (sourceRequest) {
        void respondToBundlerSource(channel.port1, sourceRequest, entryTarget, bridge);
        return;
      }
      const result = parseBundlerResult(event.data);
      if (!result) return;
      settle(result.code, result.error ? new Error(result.error) : undefined);
    }) as EventListener);
    nativeEventAdd.call(bundler, "error", ((event: ErrorEvent): void => {
      event.preventDefault();
      settle(undefined, new Error(event.message || "module Worker bundler failed"));
    }) as EventListener);
    nativePortStart.call(channel.port1);
    const request =
      type === "classic"
        ? {
            type: "rings.worker.bundle",
            mode: "classic",
            entryTarget,
            entrySource,
            credentials: "same-origin",
            acornUrl,
          }
        : {
            type: "rings.worker.bundle",
            mode: "module",
            entryTarget,
            entrySource,
            credentials,
            moduleLexerUrl,
            rollupUrl,
          };
    nativeWorkerPostMessage.call(bundler, request, { transfer: [channel.port2] });
  });
}

/** Captures and erases the packaged bundler source before authored scripts execute. */
function sharedModuleBundlerSource(): string {
  const scope = globalThis as ModuleBundlerSourceGlobal;
  const source = scope.__ringsWebviewModuleBundlerSource;
  try {
    delete scope.__ringsWebviewModuleBundlerSource;
  } catch (_error) {
    // The package installs a configurable slot; fail below if a host violates that contract.
  }
  if (typeof source !== "string" || !source.includes("rings.worker.bundle")) {
    throw new Error("isolated module Worker bundler source is unavailable");
  }
  return source;
}

/** Supplies one validated dependency to the isolated Rollup effect. */
async function respondToBundlerSource(
  port: MessagePort,
  request: BundlerSourceRequest,
  entryTarget: string,
  bridge: WorkerBridge,
): Promise<void> {
  try {
    const target = bridge.normalizeTarget(request.target, entryTarget);
    const response = await bridge.request({
      target,
      method: "GET",
      headers: [],
      body: [],
      credentials: request.credentials,
      kind: "subresource",
      topLevelNavigation: false,
      redirect: "follow",
    });
    if (response.status < 200 || response.status >= 300) {
      throw new Error(`worker module ${target} returned HTTP ${response.status}`);
    }
    nativePortPostMessage.call(port, {
      type: "rings.worker.bundle.source.response",
      requestId: request.requestId,
      source: {
        url: response.url,
        source: new TextDecoder().decode(Uint8Array.from(response.body)),
      },
    });
  } catch (error: unknown) {
    nativePortPostMessage.call(port, {
      type: "rings.worker.bundle.source.response",
      requestId: request.requestId,
      error: errorMessage(error),
    });
  }
}

/** Parses one dependency request before it reaches the renderer authority. */
function parseBundlerSourceRequest(value: unknown): BundlerSourceRequest | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const record = value as Record<string, unknown>;
  if (
    record["type"] !== "rings.worker.bundle.source.request" ||
    !Number.isSafeInteger(record["requestId"]) ||
    typeof record["requestId"] !== "number" ||
    typeof record["target"] !== "string" ||
    !isWebviewCredentials(record["credentials"])
  ) {
    return undefined;
  }
  return {
    requestId: record["requestId"],
    target: record["target"],
    credentials: record["credentials"],
  };
}

/** Parses only the terminal bundler result message. */
function parseBundlerResult(value: unknown): BundlerResult | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const record = value as Record<string, unknown>;
  if (record["type"] !== "rings.worker.bundle.result") return undefined;
  if (typeof record["code"] === "string") return { code: record["code"] };
  return { error: typeof record["error"] === "string" ? record["error"] : "module Worker bundling failed" };
}

/** Routes private worker gateway requests through the renderer request effect. */
function installGatewayPort(port: MessagePort, bridge: WorkerBridge): void {
  nativeEventAdd.call(port, "message", ((event: MessageEvent<unknown>): void => {
    const gatewayRequest = parseWorkerGatewayRequest(event.data, bridge.currentTarget());
    if (!gatewayRequest) return;
    const requestId = gatewayRequest.requestId;
    void bridge
      .request(gatewayRequest.request)
      .then((response: FrameGatewayResponse): void => {
        nativePortPostMessage.call(port, { type: "rings.worker.gateway.response", requestId, response });
      })
      .catch((error: unknown): void => {
        nativePortPostMessage.call(port, {
          type: "rings.worker.gateway.response",
          requestId,
          error: errorMessage(error),
        });
      });
  }) as EventListener);
  nativePortStart.call(port);
}

/** Preserves optional structured-clone transfers while flushing queued messages. */
function postWorkerMessage(worker: Worker, message: QueuedWorkerMessage): void {
  if (message.transfer) {
    nativeWorkerPostMessage.call(worker, message.value, { transfer: message.transfer });
  } else {
    nativeWorkerPostMessage.call(worker, message.value);
  }
}

/** Generates the isolated worker-side fetch bridge and script evaluator. */
function workerBootstrapSource(
  source: string,
  target: string,
  mode: "dedicated" | "shared",
  moduleUrl: string | undefined,
): string {
  const execute = moduleUrl ? `await import(${JSON.stringify(moduleUrl)});` : `(0, eval)(${JSON.stringify(source)});`;
  return `(() => {
${workerRuntimeSource(target, mode)}
${workerControlSource(execute)}
})();`;
}

/** Builds the closed worker network runtime without evaluating authored code. */
function workerRuntimeSource(target: string, mode: "dedicated" | "shared"): string {
  return `
${requestBodyRuntimeSource}
const ringsTargetBase = ${JSON.stringify(target)};
const ringsMode = ${JSON.stringify(mode)};
const ringsPending = new Map();
let ringsGatewayPort;
let ringsGatewaySend;
let ringsRequestId = 1;
let ringsOnConnect = null;
const ringsNativePostMessage = self.postMessage.bind(self);
const ringsNativeFetch = self.fetch.bind(self);
Object.defineProperty(self, "onconnect", {
  configurable: false,
  enumerable: true,
  get: () => ringsOnConnect,
  set: (listener) => { ringsOnConnect = typeof listener === "function" ? listener : null; }
});
function ringsBlocked(name) {
  return class { constructor() { throw new TypeError(name + " is blocked by Rings Worker bridge"); } };
}
function ringsErrorMessage(error) { return error instanceof Error ? error.message : String(error); }
function ringsHeaders(value) { return Array.from(value.entries(), ([name, headerValue]) => ({ name, value: headerValue })); }
function ringsFetch(input, init) {
  const raw = input instanceof Request ? input.url : String(input);
  if (raw.startsWith("blob:") || raw.startsWith("data:")) return ringsNativeFetch(input, init);
  const target = new URL(raw, ringsTargetBase).href;
  const request = input instanceof Request ? new Request(input, init) : new Request(target, init);
  if (request.signal.aborted) return Promise.reject(request.signal.reason || new DOMException("The operation was aborted", "AbortError"));
  return readGatewayRequestBody(request, request.signal).then((buffer) => new Promise((resolve, reject) => {
    const requestId = ringsRequestId++;
    const abort = () => {
      ringsPending.delete(requestId);
      reject(request.signal.reason || new DOMException("The operation was aborted", "AbortError"));
    };
    const cleanup = () => request.signal.removeEventListener("abort", abort);
    ringsPending.set(requestId, { resolve, reject, cleanup });
    request.signal.addEventListener("abort", abort, { once: true });
    ringsGatewaySend({
      type: "rings.worker.gateway.request",
      requestId,
      request: {
        target,
        method: request.method,
        headers: ringsHeaders(request.headers),
        body: buffer ? Array.from(new Uint8Array(buffer)) : [],
        credentials: request.credentials,
        kind: "fetch",
        topLevelNavigation: false,
        redirect: request.redirect
      }
    });
  })).then((response) => {
    if (request.redirect === "manual" && [301, 302, 303, 307, 308].includes(response.status)) {
      const opaque = Response.error();
      try { Object.defineProperty(opaque, "type", { value: "opaqueredirect" }); }
      catch (_error) { /* Status, headers, and body remain opaque. */ }
      return opaque;
    }
    if (response.status < 200) throw new TypeError("Rings onion Worker cannot expose HTTP " + response.status + " as a Fetch response");
    const body = [204, 205, 304].includes(response.status) ? null : Uint8Array.from(response.body);
    const result = new Response(body, { status: response.status, headers: response.headers.map((header) => [header.name, header.value]) });
    try {
      Object.defineProperties(result, { url: { value: response.url }, redirected: { value: response.redirected } });
    } catch (_error) {
      // Some Worker Response implementations expose these metadata fields as non-configurable.
    }
    return result;
  });
}

self.fetch = ringsFetch;
self.XMLHttpRequest = ringsBlocked("XMLHttpRequest");
self.WebSocket = ringsBlocked("WebSocket");
self.WebSocketStream = ringsBlocked("WebSocketStream");
self.EventSource = ringsBlocked("EventSource");
self.WebTransport = ringsBlocked("WebTransport");
self.RTCPeerConnection = ringsBlocked("RTCPeerConnection");
self.webkitRTCPeerConnection = ringsBlocked("webkitRTCPeerConnection");
self.importScripts = function() { throw new TypeError("importScripts is unavailable; use a module Worker or fetch through Rings"); };`;
}

/** Captures the canonical reader source before authored code can mutate globals. */
function sharedRequestBodyRuntimeSource(): string {
  const scope = globalThis as typeof globalThis & {
    readonly RingsWebviewWorkerRequest?: { readonly workerRuntimeSource?: unknown };
  };
  const source = scope.RingsWebviewWorkerRequest?.workerRuntimeSource;
  if (typeof source !== "string" || !source.includes("readGatewayRequestBody")) {
    throw new Error("shared Worker request-body runtime is unavailable");
  }
  return source;
}

/** Builds the private initialization and SharedWorker connect transition. */
function workerControlSource(execute: string): string {
  return `
self.addEventListener("message", async function ringsControl(event) {
  if (event.data?.type === "rings.worker.initialize") {
    event.stopImmediatePropagation();
    const ringsReadyCapability = event.data.capability;
    if (typeof ringsReadyCapability !== "string") throw new TypeError("Worker initialization capability is invalid");
    ringsGatewayPort = event.ports[0];
    ringsGatewaySend = ringsGatewayPort.postMessage.bind(ringsGatewayPort);
    const ringsGatewayListen = ringsGatewayPort.addEventListener.bind(ringsGatewayPort);
    const ringsGatewayStart = ringsGatewayPort.start.bind(ringsGatewayPort);
    ringsGatewayListen("message", (gatewayEvent) => {
      if (gatewayEvent.data?.type !== "rings.worker.gateway.response") return;
      const pending = ringsPending.get(gatewayEvent.data.requestId);
      if (!pending) return;
      ringsPending.delete(gatewayEvent.data.requestId);
      pending.cleanup();
      if (typeof gatewayEvent.data.error === "string") pending.reject(new TypeError(gatewayEvent.data.error));
      else pending.resolve(gatewayEvent.data.response);
    });
    ringsGatewayStart();
    try {
      ${execute}
      ringsNativePostMessage({ type: "rings.worker.ready", capability: ringsReadyCapability });
    } catch (error) {
      throw new Error("Rings worker script failed: " + ringsErrorMessage(error));
    }
    return;
  }
  if (ringsMode === "shared" && event.data?.type === "rings.worker.connect") {
    event.stopImmediatePropagation();
    const connect = new MessageEvent("connect", { ports: event.ports });
    if (ringsOnConnect) ringsOnConnect.call(self, connect);
    self.dispatchEvent(connect);
  }
}, true);
`;
}

/** Narrows an untrusted worker message to the private gateway protocol. */
function parseWorkerGatewayRequest(
  value: unknown,
  sourceTarget: string,
): { readonly requestId: number; readonly request: FrameGatewayRequest } | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const record = value as Record<string, unknown>;
  if (
    record["type"] !== "rings.worker.gateway.request" ||
    !Number.isSafeInteger(record["requestId"]) ||
    typeof record["requestId"] !== "number"
  ) {
    return undefined;
  }
  return {
    requestId: record["requestId"],
    request: parseFrameGatewayRequest(record["request"], sourceTarget),
  };
}

/** Accepts only the lifecycle acknowledgement bound to this initialization. */
function isWorkerReadyMessage(value: unknown, capability: string): boolean {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  return record["type"] === "rings.worker.ready" && record["capability"] === capability;
}

/** Projects unknown worker failures into DOM error events. */
function workerErrorEvent(error: unknown): ErrorEvent {
  if (error instanceof ErrorEvent) return new ErrorEvent("error", { message: error.message, error: error.error });
  return new ErrorEvent("error", { message: errorMessage(error), error });
}
