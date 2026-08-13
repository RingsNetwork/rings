// biome-ignore-all lint/complexity/useLiteralKeys: Untrusted records require bracket access under noPropertyAccessFromIndexSignature.
/**
 * Recursive sandbox renderer for iframe and srcdoc documents.
 *
 * The module owns the child-window lifecycle and exposes only a finite effect
 * algebra to the containing renderer. Remote page scripts never receive a
 * gateway port or a mutable source-principal handle.
 */

import type { FrameSourceAttribute } from "./extension_webview_frame_boundary.js";
import {
  errorMessage,
  type FrameGatewayRequest,
  type FrameGatewayResponse,
  isRecord,
  parseFrameGatewayRequest,
  parseRendererFrameMessage,
  parseRendererGatewayRequestMessage,
  type RendererBrowserNavigationMessage,
  type RendererFrameMessage,
  type RendererRenderCommand,
  rendererGatewayFailure,
  rendererGatewaySuccess,
} from "./webview_protocol.js";
import {
  applyRendererLifecycleMessage,
  createRendererGatewayLease,
  createRendererLifecycle,
  type RendererLifecycle,
  renderRendererDocument,
} from "./webview_renderer_session.js";

/** Effects supplied by the containing opaque renderer. */
type NestedRendererEffects = {
  readonly currentTarget: () => string;
  readonly normalizeTarget: (value: string, base: string) => string;
  readonly targetFromRewritten: (value: string) => string | undefined;
  readonly request: (
    request: FrameGatewayRequest,
    signal?: AbortSignal,
    sourceTarget?: string,
  ) => Promise<FrameGatewayResponse>;
  readonly assertCurrentRender: (generation: number) => void;
  readonly reportError: (message: string) => void;
};

/** Operations the containing renderer may perform on nested documents. */
type NestedRenderer = {
  readonly preserveFramePlan: (frame: HTMLIFrameElement) => void;
  readonly captureFrameSource: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute, value: string) => void;
  readonly discardFrameSource: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute) => void;
  readonly hasFramePlan: (frame: HTMLIFrameElement) => boolean;
  readonly hydrateFrames: (root: ParentNode, generation: number) => Promise<void>;
  readonly routeMessage: (event: MessageEvent<unknown>) => boolean;
  readonly routeBrowserNavigation: (sessionId: string, target: string) => void;
  readonly release: () => void;
};

/** Recursive renderer session for one remote iframe or srcdoc document. */
type NestedRendererSession = {
  readonly id: string;
  readonly generation: number;
  readonly frame: HTMLIFrameElement;
  readonly lifecycle: RendererLifecycle;
  sourceTarget: string;
  gatewayPort?: MessagePort;
  navigationAttempt?: number;
};

/** Pending nested document source retained outside the page-visible DOM. */
type NestedFramePlan =
  | { readonly kind: "target"; readonly target: string }
  | { readonly kind: "srcdoc"; readonly html: string };

/** Renderer-owned envelope that distinguishes private plans from authored metadata. */
type NestedFramePlanEnvelope = {
  readonly capability: string;
  readonly source: FrameSourceAttribute;
  readonly plan: NestedFramePlan;
};

const NativeMessageChannel = globalThis.MessageChannel;
const nativeWindowPostMessage = globalThis.postMessage;
const nativePortAddEventListener = globalThis.EventTarget.prototype.addEventListener;
const nativePortPostMessage = globalThis.MessagePort.prototype.postMessage;
const nativePortStart = globalThis.MessagePort.prototype.start;
const nativePortClose = globalThis.MessagePort.prototype.close;
const nativeSetAttribute = globalThis.Element.prototype.setAttribute;
const nativeRemoveAttribute = globalThis.Element.prototype.removeAttribute;
const nativeGetAttribute = globalThis.Element.prototype.getAttribute;
const nativeRandomUuid = globalThis.crypto.randomUUID.bind(globalThis.crypto);
const NESTED_PLAN_ATTRIBUTE = "data-rings-nested-plan";
const packagedRendererUrl = new URL("./webview_frame.html", globalThis.location.href);

/**
 * Creates one isolated recursive-renderer state machine.
 *
 * Invariant N1: a child request can reach the parent only through the port owned
 * by its live session. Invariant N2: its source target is captured when the
 * request begins and is not read from a later navigation state. Invariant N3:
 * a nested navigation commits a fresh realm, so stateful effects from the old
 * document cannot survive the document transition.
 */
export function createNestedRenderer(effects: NestedRendererEffects): NestedRenderer {
  const sessions = new Map<number, NestedRendererSession>();
  const managedFrames = new WeakSet<HTMLIFrameElement>();
  const framePlanCapability = nativeRandomUuid();
  let nextGeneration = 1;
  let nextNavigationAttempt = 1;

  /** Converts iframe navigation or srcdoc into a private recursive-renderer plan. */
  const preserveFramePlan = (frame: HTMLIFrameElement): void => {
    if (managedFrames.has(frame)) return;
    if (readFramePlan(frame, framePlanCapability)) return;
    nativeRemoveAttribute.call(frame, NESTED_PLAN_ATTRIBUTE);
    const srcdoc = nativeGetAttribute.call(frame, "srcdoc");
    if (srcdoc != null) {
      captureFrameSource(frame, "srcdoc", srcdoc);
      return;
    }
    const value = nativeGetAttribute.call(frame, "src");
    if (value !== null) {
      if (!isPackagedRendererUrl(value)) captureFrameSource(frame, "src", value);
      return;
    }
    captureFrameSource(frame, "srcdoc", "");
  };

  /** Captures an authored frame source without entering the browser navigation algorithm. */
  const captureFrameSource = (frame: HTMLIFrameElement, attribute: FrameSourceAttribute, value: string): void => {
    nativeRemoveAttribute.call(frame, "src");
    nativeRemoveAttribute.call(frame, "srcdoc");
    nativeRemoveAttribute.call(frame, NESTED_PLAN_ATTRIBUTE);
    nativeSetAttribute.call(frame, "sandbox", "allow-scripts allow-forms allow-modals");
    const plan = framePlanFromSource(attribute, value, effects.targetFromRewritten);
    if (plan) {
      const envelope: NestedFramePlanEnvelope = { capability: framePlanCapability, source: attribute, plan };
      nativeSetAttribute.call(frame, NESTED_PLAN_ATTRIBUTE, JSON.stringify(envelope));
    }
  };

  /** Converts removal of the effective source into a managed empty document. */
  const discardFrameSource = (frame: HTMLIFrameElement, attribute: FrameSourceAttribute): void => {
    const pending = readFramePlanEnvelope(frame, framePlanCapability);
    if (pending && pending.source !== attribute) return;
    if (!pending && nativeGetAttribute.call(frame, attribute) === null) return;
    captureFrameSource(frame, "srcdoc", "");
  };

  /** Recognizes only a plan issued by this recursive-renderer instance. */
  const hasFramePlan = (frame: HTMLIFrameElement): boolean => Boolean(readFramePlan(frame, framePlanCapability));

  /** Materializes every planned iframe as another isolated packaged renderer. */
  const hydrateFrames = async (root: ParentNode, generation: number): Promise<void> => {
    const frames = Array.from(root.querySelectorAll<HTMLIFrameElement>(`iframe[${NESTED_PLAN_ATTRIBUTE}]`));
    await Promise.all(
      frames.map(async (frame: HTMLIFrameElement): Promise<void> => {
        const plan = readFramePlan(frame, framePlanCapability);
        nativeRemoveAttribute.call(frame, NESTED_PLAN_ATTRIBUTE);
        if (!plan) return;
        try {
          await startSession(frame, plan, generation);
        } catch (error: unknown) {
          releaseForFrame(frame);
          const source = plan.kind === "target" ? plan.target : "srcdoc";
          effects.reportError(`nested renderer ${source}: ${errorMessage(error)}`);
        }
      }),
    );
  };

  /** Routes a child lifecycle message to the exact owning session. */
  const routeMessage = (event: MessageEvent<unknown>): boolean => {
    const session = Array.from(sessions.values()).find(
      (candidate: NestedRendererSession): boolean => candidate.frame.contentWindow === event.source,
    );
    const message = parseRendererFrameMessage(event.data);
    if (!session || !message || message.rendererGeneration !== session.generation) return false;
    handleMessage(session, message);
    return true;
  };

  /** Routes one browser-observed navigation through the private renderer tree. */
  const routeBrowserNavigation = (sessionId: string, target: string): void => {
    const owner = Array.from(sessions.values()).find(
      (session: NestedRendererSession): boolean => session.id === sessionId,
    );
    if (owner) {
      void navigateSession(owner, target).catch((error: unknown): void => {
        effects.reportError(`nested browser navigation: ${errorMessage(error)}`);
      });
      return;
    }
    for (const session of sessions.values()) {
      if (!session.gatewayPort) continue;
      const message = {
        type: "rings.webview.browser.navigate",
        sessionId,
        target,
      } satisfies RendererBrowserNavigationMessage;
      nativePortPostMessage.call(session.gatewayPort, message);
    }
  };

  /** Releases every child session before replacing its owning document. */
  const release = (): void => {
    for (const session of sessions.values()) releaseSession(session);
    sessions.clear();
  };

  /** Starts one recursive renderer and commits only a fully fetched document to it. */
  async function startSession(
    frame: HTMLIFrameElement,
    plan: NestedFramePlan,
    parentRenderGeneration?: number,
  ): Promise<NestedRendererSession> {
    if (parentRenderGeneration !== undefined) effects.assertCurrentRender(parentRenderGeneration);
    releaseForFrame(frame);
    const generation = nextGeneration;
    nextGeneration += 1;
    const sourceTarget = plan.kind === "target" ? plan.target : effects.currentTarget();
    const session: NestedRendererSession = {
      id: nativeRandomUuid(),
      generation,
      frame,
      sourceTarget,
      lifecycle: createRendererLifecycle(),
    };
    sessions.set(generation, session);
    managedFrames.add(frame);
    nativeSetAttribute.call(frame, "sandbox", "allow-scripts allow-forms allow-modals");
    const ready = session.lifecycle.waitUntilReady("nested renderer");
    nativeRemoveAttribute.call(frame, "src");
    nativeRemoveAttribute.call(frame, "srcdoc");
    setFrameRendererSource(frame, session);
    await ready;
    const nextDocument = await resolveDocument(plan);
    if (parentRenderGeneration !== undefined) effects.assertCurrentRender(parentRenderGeneration);
    session.sourceTarget = nextDocument.target;
    await sendDocument(session, nextDocument.target, nextDocument.html);
    return session;
  }

  /** Applies one child lifecycle message to its exact nested session. */
  function handleMessage(session: NestedRendererSession, message: RendererFrameMessage): void {
    if (applyRendererLifecycleMessage(session.lifecycle, message)) return;
    if (message.type === "rings.webview.frame.navigate") {
      void navigateSession(session, message.target).catch((error: unknown): void => {
        effects.reportError(`nested navigation: ${errorMessage(error)}`);
      });
    }
  }

  /** Resolves a nested srcdoc or onion-fetched navigation into renderable bytes. */
  async function resolveDocument(plan: NestedFramePlan): Promise<{ readonly target: string; readonly html: string }> {
    if (plan.kind === "srcdoc") return { target: effects.currentTarget(), html: plan.html };
    const response = await effects.request({
      target: plan.target,
      method: "GET",
      headers: [],
      body: [],
      credentials: "include",
      kind: "navigation",
      topLevelNavigation: false,
      redirect: "follow",
    });
    if (response.status < 200 || response.status >= 300) {
      throw new Error(`nested navigation ${plan.target} returned HTTP ${response.status}`);
    }
    return { target: response.url, html: new TextDecoder().decode(Uint8Array.from(response.body)) };
  }

  /** Transfers a child-specific gateway port before its remote scripts execute. */
  async function sendDocument(session: NestedRendererSession, target: string, html: string): Promise<void> {
    await renderRendererDocument({
      lifecycle: session.lifecycle,
      rendererGeneration: session.generation,
      target,
      html,
      owner: "nested renderer",
      effects: {
        createCapability: nativeRandomUuid,
        createGateway: () => {
          const gateway = new NativeMessageChannel();
          return createRendererGatewayLease(gateway.port1, gateway.port2, (port: MessagePort): void => {
            nativePortClose.call(port);
          });
        },
        installGateway: (port: MessagePort): void => {
          if (session.gatewayPort) nativePortClose.call(session.gatewayPort);
          session.gatewayPort = port;
          installGatewayRelay(session, port);
        },
        postRender: (message: RendererRenderCommand, port: MessagePort): void => {
          const child = session.frame.contentWindow;
          if (!child) throw new Error("nested renderer window is unavailable");
          nativeWindowPostMessage.call(child, message, { targetOrigin: "*", transfer: [port] });
        },
      },
    });
  }

  /** Relays child requests while preserving the request-time source target. */
  function installGatewayRelay(session: NestedRendererSession, port: MessagePort): void {
    nativePortAddEventListener.call(port, "message", ((event: MessageEvent<unknown>): void => {
      const message = parseRendererGatewayRequestMessage(event.data);
      if (!message) return;
      const sourceTarget =
        message.sourceTarget !== undefined
          ? effects.normalizeTarget(message.sourceTarget, session.sourceTarget)
          : session.sourceTarget;
      void relayGatewayRequest(port, message.requestId, message.request, sourceTarget);
    }) as EventListener);
    nativePortStart.call(port);
  }

  /** Projects one private child request onto the parent's private capability. */
  async function relayGatewayRequest(
    port: MessagePort,
    requestId: number,
    request: unknown,
    sourceTarget: string,
  ): Promise<void> {
    try {
      const response = await effects.request(parseFrameGatewayRequest(request, sourceTarget), undefined, sourceTarget);
      nativePortPostMessage.call(port, rendererGatewaySuccess(requestId, response));
    } catch (error: unknown) {
      nativePortPostMessage.call(port, rendererGatewayFailure(requestId, errorMessage(error)));
    }
  }

  /** Re-renders a child realm after application-owned nested navigation. */
  async function navigateSession(session: NestedRendererSession, target: string): Promise<void> {
    const attempt = nextNavigationAttempt;
    nextNavigationAttempt += 1;
    session.navigationAttempt = attempt;
    const oldFrame = session.frame;
    const nextTarget = effects.normalizeTarget(target, session.sourceTarget);
    const staging = document.createElement("iframe");
    managedFrames.add(staging);
    const id = oldFrame.id;
    const name = oldFrame.name;
    const style = oldFrame.getAttribute("style");
    for (const attribute of Array.from(oldFrame.attributes)) {
      if (!["id", "name", "src", "srcdoc", "style"].includes(attribute.name)) {
        staging.setAttribute(attribute.name, attribute.value);
      }
    }
    Object.assign(staging.style, {
      inset: "0",
      pointerEvents: "none",
      position: "absolute",
      visibility: "hidden",
    });
    oldFrame.after(staging);
    try {
      const next = await startSession(staging, {
        kind: "target",
        target: nextTarget,
      }).catch((error: unknown): never => {
        throw new Error(`fresh realm failed: ${errorMessage(error)}`, { cause: error });
      });
      if (
        sessions.get(session.generation) !== session ||
        session.navigationAttempt !== attempt ||
        !oldFrame.isConnected ||
        !next.frame.isConnected
      ) {
        throw new DOMException("Nested navigation was superseded", "AbortError");
      }
      if (id) next.frame.id = id;
      if (name) next.frame.name = name;
      if (style === null) next.frame.removeAttribute("style");
      else next.frame.setAttribute("style", style);
      // The staging frame is already the old frame's next sibling. Moving it
      // through replaceWith() would detach its browsing context and reload the
      // packaged renderer, discarding the document that just ACKed.
      oldFrame.remove();
      releaseSession(session);
    } catch (error: unknown) {
      releaseForFrame(staging);
      staging.remove();
      throw error;
    }
  }

  /** Releases the prior session for one iframe before navigating it again. */
  function releaseForFrame(frame: HTMLIFrameElement): void {
    const session = Array.from(sessions.values()).find(
      (candidate: NestedRendererSession): boolean => candidate.frame === frame,
    );
    if (session) releaseSession(session);
  }

  /** Rejects waiters and closes capabilities owned by one nested renderer. */
  function releaseSession(session: NestedRendererSession): void {
    sessions.delete(session.generation);
    managedFrames.delete(session.frame);
    session.lifecycle.release(new DOMException("Nested renderer was superseded", "AbortError"));
    if (session.gatewayPort) nativePortClose.call(session.gatewayPort);
    delete session.gatewayPort;
    delete session.navigationAttempt;
  }

  return {
    preserveFramePlan,
    captureFrameSource,
    discardFrameSource,
    hasFramePlan,
    hydrateFrames,
    routeMessage,
    routeBrowserNavigation,
    release,
  };
}

/** Purely maps one authored frame source into the recursive renderer's finite plan algebra. */
function framePlanFromSource(
  attribute: FrameSourceAttribute,
  value: string,
  targetFromRewritten: (value: string) => string | undefined,
): NestedFramePlan | undefined {
  if (attribute === "srcdoc") return { kind: "srcdoc", html: value };
  const target = targetFromRewritten(value);
  return target ? { kind: "target", target } : undefined;
}

/** Loads a recursive packaged renderer with a globally unique routing witness. */
function setFrameRendererSource(frame: HTMLIFrameElement, session: NestedRendererSession): void {
  const url = new URL(packagedRendererUrl);
  url.searchParams.set("navigation", String(session.generation));
  url.searchParams.set("nestedSession", session.id);
  nativeSetAttribute.call(frame, "src", url.href);
}

/** Parses renderer-owned metadata after authored data-rings fields were cleared. */
function readFramePlan(frame: HTMLIFrameElement, capability: string): NestedFramePlan | undefined {
  return readFramePlanEnvelope(frame, capability)?.plan;
}

/** Parses the complete renderer-owned source witness for one planned frame. */
function readFramePlanEnvelope(frame: HTMLIFrameElement, capability: string): NestedFramePlanEnvelope | undefined {
  const raw = nativeGetAttribute.call(frame, NESTED_PLAN_ATTRIBUTE);
  if (!raw) return undefined;
  try {
    const envelope: unknown = JSON.parse(raw);
    if (
      !isRecord(envelope) ||
      envelope["capability"] !== capability ||
      (envelope["source"] !== "src" && envelope["source"] !== "srcdoc") ||
      !isRecord(envelope["plan"])
    ) {
      return undefined;
    }
    const plan = envelope["plan"];
    if (plan["kind"] === "target" && typeof plan["target"] === "string") {
      return { capability, source: envelope["source"], plan: { kind: "target", target: plan["target"] } };
    }
    return plan["kind"] === "srcdoc" && typeof plan["html"] === "string"
      ? { capability, source: envelope["source"], plan: { kind: "srcdoc", html: plan["html"] } }
      : undefined;
  } catch (_error: unknown) {
    return undefined;
  }
}

/** Recognizes only this extension's packaged recursive renderer URL. */
function isPackagedRendererUrl(value: string): boolean {
  try {
    const candidate = new URL(value, globalThis.location.href);
    return (
      candidate.protocol === packagedRendererUrl.protocol &&
      candidate.host === packagedRendererUrl.host &&
      candidate.pathname === packagedRendererUrl.pathname
    );
  } catch (_error: unknown) {
    return false;
  }
}
