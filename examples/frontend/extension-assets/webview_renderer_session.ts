/**
 * Shared renderer lifecycle algebra and its bounded browser-effect interpreter.
 */

import type { RendererFrameMessage, RendererRenderCommand } from "./webview_protocol.js";

const NativeError = globalThis.Error;
const NativePromise = globalThis.Promise;
const NativeString = globalThis.String;
const nativeClearTimeout = globalThis.clearTimeout;
const nativePromiseCatch = globalThis.Promise.prototype.catch;
const nativeSetTimeout = globalThis.setTimeout;

/** Complete finite state of one single-document renderer realm. */
export type RendererLifecycleState =
  | { readonly kind: "created" }
  | { readonly kind: "awaiting-ready" }
  | { readonly kind: "ready" }
  | { readonly kind: "awaiting-render"; readonly capability: string }
  | { readonly kind: "rendered" }
  | { readonly kind: "failed" }
  | { readonly kind: "released" };

/** Inputs accepted by the pure renderer lifecycle relation. */
export type RendererLifecycleAction =
  | { readonly kind: "begin-ready" }
  | { readonly kind: "ready" }
  | { readonly kind: "begin-render"; readonly capability: string }
  | { readonly kind: "rendered"; readonly capability: string }
  | { readonly kind: "render-failed"; readonly capability: string }
  | { readonly kind: "pending-failed" }
  | { readonly kind: "release" };

/** Result of applying one value-level lifecycle action. */
export type RendererLifecycleStep =
  | { readonly kind: "accepted"; readonly state: RendererLifecycleState }
  | { readonly kind: "ignored"; readonly state: RendererLifecycleState }
  | { readonly kind: "invalid"; readonly state: RendererLifecycleState; readonly message: string };

/** Acknowledgements accepted by the lifecycle effect interpreter. */
export type RendererLifecycleEvent =
  | { readonly kind: "ready" }
  | { readonly kind: "rendered"; readonly capability: string }
  | { readonly kind: "render-failed"; readonly capability: string; readonly error: Error };

/** Effect boundary for one renderer realm's promises and timers. */
export type RendererLifecycle = {
  readonly waitUntilReady: (owner: string) => Promise<void>;
  readonly waitUntilRendered: (capability: string, owner: string) => Promise<void>;
  readonly apply: (event: RendererLifecycleEvent) => boolean;
  readonly release: (error: Error) => void;
};

/** Ports owned locally until renderer transfer succeeds. */
export type RendererGatewayLease = {
  readonly hostPort: MessagePort;
  readonly rendererPort: MessagePort;
  /** Idempotently closes both locally owned ports without throwing. */
  readonly release: () => void;
};

/** Browser effects required to transfer and install one renderer document. */
export type RendererDocumentEffects = {
  readonly createCapability: () => string;
  readonly createGateway: () => RendererGatewayLease;
  readonly installGateway: (port: MessagePort) => void;
  readonly postRender: (message: RendererRenderCommand, port: MessagePort) => void;
};

/** Complete value required to render one document through one owned session. */
export type RendererDocumentRequest = {
  readonly lifecycle: RendererLifecycle;
  readonly rendererGeneration: number;
  readonly target: string;
  readonly html: string;
  readonly owner: string;
  readonly effects: RendererDocumentEffects;
};

/** Deferred effect owned by the unique pending lifecycle state. */
type PendingRendererTransition =
  | {
      readonly kind: "ready";
      readonly resolve: () => void;
      readonly reject: (error: Error) => void;
      readonly timeout: number;
    }
  | {
      readonly kind: "rendered";
      readonly capability: string;
      readonly resolve: () => void;
      readonly reject: (error: Error) => void;
      readonly timeout: number;
    };

/** Shared timeout for the packaged renderer to establish its generation. */
export const RENDERER_READY_TIMEOUT_MS = 10_000;
/** Shared timeout for a renderer to commit one document. */
export const RENDER_COMMIT_TIMEOUT_MS = 30_000;

/**
 * Applies the pure renderer lifecycle relation.
 *
 * Law: only `awaiting-render(c)` accepts a rendered or failed witness carrying
 * exactly `c`. Preservation: every accepted action produces one member of the
 * closed `RendererLifecycleState` union.
 */
export function advanceRendererLifecycle(
  state: RendererLifecycleState,
  action: RendererLifecycleAction,
): RendererLifecycleStep {
  switch (action.kind) {
    case "begin-ready":
      return state.kind === "created"
        ? accepted({ kind: "awaiting-ready" })
        : invalid(state, `renderer ready transition is not permitted from ${state.kind}`);
    case "ready":
      return state.kind === "awaiting-ready" ? accepted({ kind: "ready" }) : ignored(state);
    case "begin-render":
      return state.kind === "ready"
        ? accepted({ kind: "awaiting-render", capability: action.capability })
        : invalid(state, `renderer render transition is not permitted from ${state.kind}`);
    case "rendered":
      return state.kind === "awaiting-render" && state.capability === action.capability
        ? accepted({ kind: "rendered" })
        : ignored(state);
    case "render-failed":
      return state.kind === "awaiting-render" && state.capability === action.capability
        ? accepted({ kind: "failed" })
        : ignored(state);
    case "pending-failed":
      return state.kind === "awaiting-ready" || state.kind === "awaiting-render"
        ? accepted({ kind: "failed" })
        : ignored(state);
    case "release":
      return state.kind === "released" ? ignored(state) : accepted({ kind: "released" });
  }
}

/** Creates one independent effect interpreter for a renderer lifecycle. */
export function createRendererLifecycle(): RendererLifecycle {
  let state: RendererLifecycleState = { kind: "created" };
  let pending: PendingRendererTransition | undefined;

  /**
   * Invariant: `pending` exists iff `state` is awaiting the same phase and,
   * for render, the same capability. Every operation below preserves it.
   */
  const begin = (
    action: Extract<RendererLifecycleAction, { readonly kind: "begin-ready" | "begin-render" }>,
    owner: string,
  ): Promise<void> => {
    const step = advanceRendererLifecycle(state, action);
    if (step.kind !== "accepted") {
      throw new NativeError(step.kind === "invalid" ? step.message : "renderer transition failed");
    }
    state = step.state;
    const timeoutMs = action.kind === "begin-ready" ? RENDERER_READY_TIMEOUT_MS : RENDER_COMMIT_TIMEOUT_MS;
    const timeoutMessage =
      action.kind === "begin-ready" ? `${owner} did not become ready` : `${owner} did not commit the document`;
    return new NativePromise((resolve, reject): void => {
      const timeout = nativeSetTimeout((): void => failPending(timeout, new NativeError(timeoutMessage)), timeoutMs);
      const transition: PendingRendererTransition =
        action.kind === "begin-ready"
          ? { kind: "ready", resolve, reject, timeout }
          : { kind: "rendered", capability: action.capability, resolve, reject, timeout };
      pending = transition;
    });
  };

  const failPending = (timeout: number, error: Error): void => {
    const owned = pending;
    if (!owned || owned.timeout !== timeout) return;
    pending = undefined;
    const step = advanceRendererLifecycle(state, { kind: "pending-failed" });
    if (step.kind === "accepted") state = step.state;
    owned.reject(error);
  };

  const apply = (event: RendererLifecycleEvent): boolean => {
    const step = advanceRendererLifecycle(state, lifecycleAction(event));
    if (step.kind !== "accepted") return false;
    const owned = pending;
    if (!owned) return false;
    state = step.state;
    pending = undefined;
    nativeClearTimeout(owned.timeout);
    if (event.kind === "render-failed") owned.reject(event.error);
    else owned.resolve();
    return true;
  };

  return {
    waitUntilReady: (owner: string): Promise<void> => begin({ kind: "begin-ready" }, owner),
    waitUntilRendered: (capability: string, owner: string): Promise<void> =>
      begin({ kind: "begin-render", capability }, owner),
    apply,
    release: (error: Error): void => {
      const step = advanceRendererLifecycle(state, { kind: "release" });
      if (step.kind === "accepted") state = step.state;
      const owned = pending;
      pending = undefined;
      if (!owned) return;
      nativeClearTimeout(owned.timeout);
      owned.reject(error);
    },
  };
}

/** Applies only lifecycle messages and leaves navigation/title events to adapters. */
export function applyRendererLifecycleMessage(lifecycle: RendererLifecycle, message: RendererFrameMessage): boolean {
  switch (message.type) {
    case "rings.webview.frame.ready":
      return lifecycle.apply({ kind: "ready" });
    case "rings.webview.frame.rendered":
      return lifecycle.apply({ kind: "rendered", capability: message.renderCapability });
    case "rings.webview.frame.renderFailed":
      return lifecycle.apply({
        kind: "render-failed",
        capability: message.renderCapability,
        error: new NativeError(message.error),
      });
    default:
      return false;
  }
}

/** Creates an idempotent local lease for a newly allocated MessageChannel. */
export function createRendererGatewayLease(
  hostPort: MessagePort,
  rendererPort: MessagePort,
  closePort: (port: MessagePort) => void,
): RendererGatewayLease {
  let released = false;
  const releasePort = (port: MessagePort): void => {
    try {
      closePort(port);
    } catch (_error: unknown) {
      // Cleanup is best-effort and must not replace the originating failure.
    }
  };
  return {
    hostPort,
    rendererPort,
    release: (): void => {
      if (released) return;
      released = true;
      releasePort(hostPort);
      releasePort(rendererPort);
    },
  };
}

/**
 * Transfers one document and waits for its exact render capability witness.
 *
 * Pre: `lifecycle` is ready. Post: successful posting transfers the renderer
 * port and the adapter owns the host port; every earlier failure releases both.
 */
export async function renderRendererDocument(request: RendererDocumentRequest): Promise<void> {
  const { lifecycle, rendererGeneration, target, html, owner, effects } = request;
  const renderCapability = effects.createCapability();
  const rendered = lifecycle.waitUntilRendered(renderCapability, owner);
  const command = {
    type: "rings.webview.render",
    rendererGeneration,
    renderCapability,
    target,
    html,
  } satisfies RendererRenderCommand;
  let gateway: RendererGatewayLease | undefined;
  try {
    gateway = effects.createGateway();
    effects.installGateway(gateway.hostPort);
    effects.postRender(command, gateway.rendererPort);
    gateway = undefined;
  } catch (error: unknown) {
    gateway?.release();
    const failure = error instanceof NativeError ? error : new NativeError(NativeString(error));
    lifecycle.apply({ kind: "render-failed", capability: renderCapability, error: failure });
    await nativePromiseCatch.call(rendered, (): void => {});
    throw failure;
  }
  await rendered;
}

/** Constructs an accepted step without exposing mutable state. */
function accepted(state: RendererLifecycleState): RendererLifecycleStep {
  return { kind: "accepted", state };
}

/** Constructs an ignored step for an unowned acknowledgement. */
function ignored(state: RendererLifecycleState): RendererLifecycleStep {
  return { kind: "ignored", state };
}

/** Constructs a rejected command that would violate lifecycle order. */
function invalid(state: RendererLifecycleState, message: string): RendererLifecycleStep {
  return { kind: "invalid", state, message };
}

/** Projects an acknowledgement into the pure transition algebra. */
function lifecycleAction(event: RendererLifecycleEvent): RendererLifecycleAction {
  switch (event.kind) {
    case "ready":
      return event;
    case "rendered":
      return event;
    case "render-failed":
      return { kind: event.kind, capability: event.capability };
  }
}
