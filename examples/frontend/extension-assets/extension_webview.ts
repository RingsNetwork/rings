// biome-ignore-all lint/complexity/useLiteralKeys: Untrusted records require bracket access under noPropertyAccessFromIndexSignature.
/**
 * Trusted MV3 window that routes an opaque renderer through the retained Rings node.
 */

import {
  beginNavigation,
  beginRendering,
  commitNavigation,
  failNavigation,
  initialNavigationState,
  isActiveNavigation,
  type NavigationIntent,
  type NavigationState,
} from "./webview_navigation_state.js";
import {
  controlledWebviewUrl,
  errorMessage,
  type FrameGatewayRequest,
  type FrameGatewayResponse,
  isHttpStatus,
  isRecord,
  isRedirectStatus,
  MAX_WEBVIEW_REDIRECTS,
  normalizeHttpsTarget,
  parseFrameGatewayRequest,
  parseRendererFrameMessage,
  parseRendererGatewayRequestMessage,
  parseWebviewByteView,
  parseWebviewHeaders,
  type RawWebviewGatewayResponse,
  type RendererBrowserNavigationMessage,
  type RendererFrameMessage,
  type RendererGatewayRequestMessage,
  type RendererRenderCommand,
  redirectedWebviewRequest,
  rendererGatewayFailure,
  rendererGatewaySuccess,
  resolveWebviewRedirect,
  type WebviewGatewayRequest,
} from "./webview_protocol.js";
import {
  applyRendererLifecycleMessage,
  createRendererGatewayLease,
  createRendererLifecycle,
  type RendererLifecycle,
  renderRendererDocument,
} from "./webview_renderer_session.js";

/** Minimal retained-node bridge consumed by the trusted WebView window. */
type ExtensionWebviewNodeBridge = {
  webviewRequest(request: WebviewGatewayRequest): Promise<RawWebviewGatewayResponse>;
};

/** Immutable authority owned by one live renderer realm. */
type RendererSession = {
  readonly generation: number;
  readonly sourceTarget: string;
  readonly frame: HTMLIFrameElement;
  readonly lifecycle: RendererLifecycle;
  gatewayPort?: MessagePort;
  title?: string;
};

/** Authority for a complete bounded redirect evaluation. */
type GatewayAuthority =
  | { readonly kind: "top-level-navigation" }
  | { readonly kind: "renderer"; readonly session: RendererSession; readonly sourceTarget: string };

const DEFAULT_TARGET = "https://example.com/";
const addressForm = requiredElement<HTMLFormElement>("#webview-address-form");
const addressInput = requiredElement<HTMLInputElement>("#webview-address");
const frameContainer = requiredElement<HTMLElement>(".webview-content");
const placeholderFrame = requiredElement<HTMLIFrameElement>("#webview-frame");
const statusText = requiredElement<HTMLElement>("#webview-status");
const backButton = requiredElement<HTMLButtonElement>("#webview-back");
const forwardButton = requiredElement<HTMLButtonElement>("#webview-forward");
const reloadButton = requiredElement<HTMLButtonElement>("#webview-reload");

let navigationState: NavigationState = initialNavigationState();
let committedRenderer: RendererSession | undefined;
let pendingRenderer: RendererSession | undefined;
let webviewTabId: number | undefined;

addressInput.value = DEFAULT_TARGET;
addressForm.addEventListener("submit", (event: SubmitEvent): void => {
  event.preventDefault();
  void navigate(addressInput.value, { kind: "push" });
});
backButton.addEventListener("click", (): void => navigateHistory(-1));
forwardButton.addEventListener("click", (): void => navigateHistory(1));
reloadButton.addEventListener("click", (): void => {
  if (navigationState.committedTarget) {
    void navigate(navigationState.committedTarget, { kind: "reload" });
  }
});

globalThis.addEventListener("message", (event: MessageEvent<unknown>): void => {
  const session = rendererSessionForSource(event.source);
  const message = parseRendererFrameMessage(event.data);
  if (!session || !message || message.rendererGeneration !== session.generation) return;
  handleFrameMessage(session, message);
});

chrome.runtime.onMessage.addListener((message: unknown): false => {
  if (
    isRecord(message) &&
    message["type"] === "rings.webview.navigate" &&
    message["tabId"] === webviewTabId &&
    typeof message["url"] === "string"
  ) {
    if (typeof message["sessionId"] === "string") {
      routeNestedBrowserNavigation(message["sessionId"], message["url"]);
    } else {
      void navigate(message["url"], { kind: "push" });
    }
  }
  return false;
});

void activateNetworkIsolation().catch((error: unknown): void => {
  setStatus(`Network isolation failed: ${errorMessage(error)}`, true);
});

/** Returns one required trusted-host DOM element. */
function requiredElement<T extends Element>(selector: string): T {
  const element = document.querySelector<T>(selector);
  if (!element) {
    throw new Error(`missing WebView element ${selector}`);
  }
  return element;
}

/** Installs the tab-scoped direct-network deny rule before navigation. */
async function activateNetworkIsolation(): Promise<void> {
  const response = await sendRuntimeMessage({ type: "rings.webview.activate" });
  webviewTabId = activationTabId(response);
  setStatus("Onion WebView ready", false);
}

/** Executes effects around the pure navigation transitions. */
async function navigate(input: string, intent: NavigationIntent): Promise<void> {
  let target: string;
  try {
    target = normalizeAddressTarget(input);
  } catch (error: unknown) {
    setStatus(errorMessage(error), true);
    return;
  }
  discardPendingRenderer("renderer navigation superseded");
  navigationState = beginNavigation(navigationState, target, intent);
  const generation = navigationState.generation;
  setLoading(true, `Opening ${target}`);
  try {
    await activateNetworkIsolation();
    const result = await requestNavigation(target);
    if (!isActiveNavigation(navigationState, generation)) return;
    navigationState = beginRendering(navigationState, generation, result.url);
    const session = await prepareRenderer(generation, result.url);
    if (!isActiveNavigation(navigationState, generation)) return;
    await renderInFrame(session, result.url, new TextDecoder().decode(Uint8Array.from(result.body)));
    if (!isActiveNavigation(navigationState, generation)) return;
    const committedState = commitNavigation(navigationState, generation);
    commitRenderer(session);
    navigationState = committedState;
    syncCommittedUi();
    setStatus(`Loaded through Rings onion gateway: ${result.url}`, false);
  } catch (error: unknown) {
    if (isActiveNavigation(navigationState, generation)) {
      const message = errorMessage(error);
      discardPendingRenderer(message, generation);
      navigationState = failNavigation(navigationState, generation, message);
      syncCommittedUi();
      setStatus(message, true);
    }
  } finally {
    if (navigationState.generation === generation) {
      setLoading(false);
    }
  }
}

/** Fetches and follows one top-level navigation through the shared redirect machine. */
async function requestNavigation(target: string): Promise<FrameGatewayResponse> {
  const response = await gatewayRequestFollowingRedirects(
    {
      target,
      method: "GET",
      headers: [],
      body: [],
      credentials: "include",
      kind: "navigation",
      topLevelNavigation: true,
      redirect: "follow",
    },
    { kind: "top-level-navigation" },
  );
  if (response.status < 200 || response.status >= 300) {
    throw new Error(`WebView navigation returned HTTP ${response.status}`);
  }
  return response;
}

/** Applies one validated renderer message to its owned lifecycle transition. */
function handleFrameMessage(session: RendererSession, message: RendererFrameMessage): void {
  if (applyRendererLifecycleMessage(session.lifecycle, message)) return;
  if (message.type === "rings.webview.frame.navigate") {
    void navigate(message.target, { kind: "push" });
    return;
  }
  if (message.type === "rings.webview.frame.title") {
    if (message.title.trim()) {
      session.title = message.title.trim();
      if (session === committedRenderer) applyRendererTitle(session);
    }
    return;
  }
}

/** Creates a hidden renderer and waits for its generation-ready witness. */
async function prepareRenderer(generation: number, sourceTarget: string): Promise<RendererSession> {
  discardPendingRenderer("renderer navigation superseded");
  const nextFrame = document.createElement("iframe");
  nextFrame.className = "webview-frame webview-frame-staging";
  nextFrame.title = "Onion WebView pending content";
  nextFrame.setAttribute("sandbox", "allow-scripts allow-forms allow-modals");
  nextFrame.src = `./webview_frame.html?navigation=${generation}`;
  const session: RendererSession = {
    generation,
    sourceTarget,
    frame: nextFrame,
    lifecycle: createRendererLifecycle(),
  };
  pendingRenderer = session;
  frameContainer.append(nextFrame);
  await session.lifecycle.waitUntilReady("WebView renderer");
  return session;
}

/** Sends one document and waits for its matching committed-render witness. */
async function renderInFrame(session: RendererSession, target: string, html: string): Promise<void> {
  await renderRendererDocument({
    lifecycle: session.lifecycle,
    rendererGeneration: session.generation,
    target,
    html,
    owner: "WebView renderer",
    effects: {
      createCapability: (): string => crypto.randomUUID(),
      createGateway: () => {
        const gateway = new MessageChannel();
        return createRendererGatewayLease(gateway.port1, gateway.port2, (port: MessagePort): void => port.close());
      },
      installGateway: (port: MessagePort): void => {
        session.gatewayPort?.close();
        session.gatewayPort = port;
        installRendererGateway(session, port);
      },
      postRender: (message: RendererRenderCommand, port: MessagePort): void => {
        postToRenderer(session, message, [port]);
      },
    },
  });
}

/** Returns the live session that owns one renderer WindowProxy. */
function rendererSessionForSource(source: MessageEventSource | null): RendererSession | undefined {
  return [pendingRenderer, committedRenderer].find(
    (session: RendererSession | undefined): session is RendererSession => session?.frame.contentWindow === source,
  );
}

/** Returns whether a renderer still owns authority to begin another effect. */
function isLiveRenderer(session: RendererSession): boolean {
  return session === pendingRenderer || session === committedRenderer;
}

/** Sends a browser-observed nested navigation only through live private renderer capabilities. */
function routeNestedBrowserNavigation(sessionId: string, target: string): void {
  const message = {
    type: "rings.webview.browser.navigate",
    sessionId,
    target,
  } satisfies RendererBrowserNavigationMessage;
  for (const session of [pendingRenderer, committedRenderer]) {
    if (!session?.gatewayPort) continue;
    session.gatewayPort.postMessage(message);
  }
}

/** Installs a private gateway channel that page scripts cannot observe or forge. */
function installRendererGateway(session: RendererSession, port: MessagePort): void {
  port.addEventListener("message", (event: MessageEvent<unknown>): void => {
    const message = parseRendererGatewayRequestMessage(event.data);
    if (!message || !isLiveRenderer(session)) return;
    void handleRendererGatewayRequest(session, port, message);
  });
  port.start();
}

/** Executes one request with the immutable authority of its renderer generation. */
async function handleRendererGatewayRequest(
  session: RendererSession,
  port: MessagePort,
  message: RendererGatewayRequestMessage,
): Promise<void> {
  const requestId = message.requestId;
  const delegatedSource =
    typeof message.sourceTarget === "string"
      ? normalizeHttpsTarget(message.sourceTarget, session.sourceTarget)
      : session.sourceTarget;
  const authority: GatewayAuthority = { kind: "renderer", session, sourceTarget: delegatedSource };
  try {
    const request = parseFrameGatewayRequest(message.request, delegatedSource);
    const response = await gatewayRequestFollowingRedirects(request, authority);
    if (isLiveRenderer(session)) {
      port.postMessage(rendererGatewaySuccess(requestId, response));
    }
  } catch (error: unknown) {
    if (isLiveRenderer(session)) {
      port.postMessage(rendererGatewayFailure(requestId, errorMessage(error)));
    }
  }
}

/**
 * Evaluates the bounded Fetch redirect relation for every gateway request kind.
 *
 * Invariant G1: every hop carries the same immutable `GatewayAuthority` value.
 * Invariant G2: a renderer effect may dispatch a hop iff its session is live.
 * Therefore a concurrently prepared or committed realm cannot lend its source
 * principal to an older continuation.
 */
async function gatewayRequestFollowingRedirects(
  initial: FrameGatewayRequest,
  authority: GatewayAuthority,
): Promise<FrameGatewayResponse> {
  let request = initial;
  let redirected = false;
  for (let count = 0; count <= MAX_WEBVIEW_REDIRECTS; count += 1) {
    assertGatewayAuthority(authority);
    const response = await gatewayRequest(request, authority);
    const target = isRedirectStatus(response.status)
      ? resolveWebviewRedirect(response.headers, request.target)
      : undefined;
    if (!target) return { ...response, url: request.target, redirected };
    if (request.redirect === "manual") return { ...response, url: request.target, redirected };
    if (request.redirect === "error") throw new TypeError("Rings onion WebView redirect mode is error");
    if (count === MAX_WEBVIEW_REDIRECTS) {
      throw new TypeError(`Rings onion WebView exceeded ${MAX_WEBVIEW_REDIRECTS} redirects`);
    }
    request = redirectedWebviewRequest(request, response.status, target);
    redirected = true;
  }
  throw new TypeError(`Rings onion WebView exceeded ${MAX_WEBVIEW_REDIRECTS} redirects`);
}

/** Sends exactly one request through the retained Rust gateway. */
async function gatewayRequest(
  request: FrameGatewayRequest,
  authority: GatewayAuthority,
): Promise<Omit<FrameGatewayResponse, "url" | "redirected">> {
  const sourceTarget = authority.kind === "renderer" ? authority.sourceTarget : undefined;
  const payload: WebviewGatewayRequest = {
    requested: controlledWebviewUrl(request.target),
    ...(sourceTarget ? { sourceTarget } : {}),
    method: request.method,
    headers: request.headers,
    body: request.body,
    credentials: request.credentials,
    kind: request.kind,
    topLevelNavigation: request.topLevelNavigation,
  };
  const response = await nodeBridge().webviewRequest(payload);
  if (response.ok !== true) {
    const code = typeof response.errorCode === "string" ? ` (${response.errorCode})` : "";
    const summary = typeof response.errorSummary === "string" ? response.errorSummary : "WebView gateway failed";
    const detail = typeof response.error === "string" ? response.error : "unknown gateway error";
    throw new Error(`${summary}${code}: ${detail}`);
  }
  const status = Number(response.status);
  if (!isHttpStatus(status)) {
    throw new Error("WebView gateway returned an invalid status");
  }
  return {
    status,
    headers: parseWebviewHeaders(response.headers),
    body: Array.from(parseWebviewByteView(response.body)),
  };
}

/** Rejects a renderer authority after its realm has left the live session set. */
function assertGatewayAuthority(authority: GatewayAuthority): void {
  if (authority.kind === "renderer" && !isLiveRenderer(authority.session)) {
    throw new DOMException("Renderer request was superseded", "AbortError");
  }
}

/**
 * Commits a rendered staging realm and releases the previous realm atomically.
 *
 * Invariant R1: exactly one session is committed and visible after this effect.
 * Precondition: `session` owns `pendingRenderer` and its render capability was
 * acknowledged. A failed pending realm never reaches this function.
 */
function commitRenderer(session: RendererSession): void {
  if (pendingRenderer !== session) throw new Error("renderer commit does not own the pending generation");
  const previous = committedRenderer;
  pendingRenderer = undefined;
  committedRenderer = session;
  session.frame.id = "webview-frame";
  session.frame.className = "webview-frame";
  session.frame.title = "Onion WebView content";
  placeholderFrame.remove();
  if (previous) releaseRenderer(previous);
  applyRendererTitle(session);
}

/** Removes one failed staging realm while preserving the committed renderer. */
function discardPendingRenderer(_message: string, generation?: number): void {
  const session = pendingRenderer;
  if (!session || (generation !== undefined && session.generation !== generation)) return;
  pendingRenderer = undefined;
  releaseRenderer(session);
}

/** Releases the port, document, and object graph owned by one renderer session. */
function releaseRenderer(session: RendererSession): void {
  session.lifecycle.release(new DOMException("Renderer was superseded", "AbortError"));
  session.gatewayPort?.close();
  delete session.gatewayPort;
  session.frame.remove();
}

/** Applies only a committed renderer title to trusted browser chrome. */
function applyRendererTitle(session: RendererSession): void {
  document.title = session.title ? `${session.title} - Rings Onion WebView` : "Rings Onion WebView";
}

/** Returns the installed retained-node bridge after structural validation. */
function nodeBridge(): ExtensionWebviewNodeBridge {
  const value = (globalThis as typeof globalThis & { RingsExtensionNodeBridge?: unknown }).RingsExtensionNodeBridge;
  if (!isRecord(value) || typeof value["webviewRequest"] !== "function") {
    throw new Error("Rings extension node bridge is unavailable");
  }
  const requestEffect = value["webviewRequest"] as (
    this: Record<string, unknown>,
    request: WebviewGatewayRequest,
  ) => Promise<unknown>;
  return {
    webviewRequest: async (request: WebviewGatewayRequest): Promise<RawWebviewGatewayResponse> => {
      const response = await requestEffect.call(value, request);
      if (!isRecord(response)) throw new Error("Rings extension node bridge returned a non-object response");
      return response;
    },
  };
}

/** Parses an address-bar value and applies the HTTPS-only contract. */
function normalizeAddressTarget(input: string): string {
  let value = input.trim();
  if (!value) throw new Error("enter an HTTPS address");
  if (!/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(value)) value = `https://${value}`;
  return normalizeHttpsTarget(value, navigationState.committedTarget ?? DEFAULT_TARGET);
}

/** Starts a back or forward intent without mutating history before success. */
function navigateHistory(offset: -1 | 1): void {
  const index = navigationState.history.index + offset;
  const target = navigationState.history.entries[index];
  if (target) void navigate(target, { kind: "history", index });
}

/** Projects the committed pure model into toolbar controls. */
function syncCommittedUi(): void {
  if (navigationState.committedTarget) addressInput.value = navigationState.committedTarget;
  backButton.disabled = navigationState.history.index <= 0;
  forwardButton.disabled =
    navigationState.history.index < 0 || navigationState.history.index >= navigationState.history.entries.length - 1;
}

/** Sends one message and its private capabilities to an exact renderer realm. */
function postToRenderer(session: RendererSession, message: unknown, transfer: Transferable[] = []): void {
  const target = session.frame.contentWindow;
  if (!isLiveRenderer(session) || !target) throw new Error("WebView renderer is unavailable");
  target.postMessage(message, "*", transfer);
}

/** Projects the current effect state into loading controls. */
function setLoading(loading: boolean, message?: string): void {
  document.body.dataset["loading"] = String(loading);
  addressInput.disabled = loading;
  reloadButton.disabled = loading || !navigationState.committedTarget;
  if (message) setStatus(message, false);
}

/** Updates one text-only status message. */
function setStatus(message: string, error: boolean): void {
  statusText.textContent = message;
  statusText.dataset["error"] = String(error);
}

/** Sends one extension runtime message and rejects explicit failure envelopes. */
function sendRuntimeMessage(message: { readonly type: string }): Promise<unknown> {
  return new Promise((resolve, reject): void => {
    chrome.runtime.sendMessage(message, (response: unknown): void => {
      const runtimeError = chrome.runtime.lastError;
      if (runtimeError) {
        reject(new Error(runtimeError.message));
        return;
      }
      if (isRecord(response) && response["ok"] === false) {
        reject(new Error(typeof response["error"] === "string" ? response["error"] : "extension request failed"));
        return;
      }
      resolve(response);
    });
  });
}

/** Extracts the positive tab identity that owns the network-deny capability. */
function activationTabId(value: unknown): number {
  if (!isRecord(value) || value["ok"] !== true || !isRecord(value["result"])) {
    throw new Error("WebView activation returned an invalid response");
  }
  const tabId = value["result"]["tabId"];
  if (!Number.isSafeInteger(tabId) || typeof tabId !== "number" || tabId <= 0) {
    throw new Error("WebView activation returned an invalid tab identifier");
  }
  return tabId;
}
