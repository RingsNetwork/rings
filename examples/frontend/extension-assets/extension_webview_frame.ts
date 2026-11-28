// biome-ignore-all lint/complexity/useLiteralKeys: Untrusted records require bracket access under noPropertyAccessFromIndexSignature.
/**
 * Opaque sandbox renderer for onion-fetched WebView documents.
 */

import { installDynamicFrameBoundary } from "./extension_webview_frame_boundary.js";
import { createNestedRenderer } from "./extension_webview_nested.js";
import { installWorkerBridge } from "./extension_webview_worker.js";
import { installWebviewXmlHttpRequest } from "./extension_webview_xhr.js";
import {
  decodeControlledWebviewTarget,
  errorMessage,
  type FrameGatewayRequest,
  type FrameGatewayResponse,
  isRecord,
  isRedirectStatus,
  normalizeHttpsTarget,
  parseFrameGatewayResponse,
  parseRendererPortMessage,
  parseRendererRenderCommand,
  type RendererFrameEvent,
  type RendererGatewayRequestMessage,
  type RendererGatewayResponseMessage,
  type WebviewHeader,
  webviewHeaderValue,
} from "./webview_protocol.js";

/** Unique owner of one outstanding renderer request response slot. */
type PendingSandboxRequest = {
  readonly resolve: (response: FrameGatewayResponse) => void;
  readonly reject: (error: Error) => void;
};

/** Bound private-port effects captured before authored scripts can patch DOM prototypes. */
type GatewayCapability = {
  readonly close: () => void;
  readonly send: (message: unknown) => void;
};

/** One URL and descriptor from the shared HTML srcset grammar. */
type SrcsetCandidate = {
  readonly url: string;
  readonly descriptor: string;
};

/** Srcset candidate plus an optional validated onion target. */
type HydratedSrcsetCandidate = SrcsetCandidate & {
  readonly target?: string;
};

/** One independently hydrated networking attribute on an element. */
type ResourceHydrationPlan = {
  readonly attribute: string;
  readonly target: string;
};

/** Pure transform functions loaded from the shared browser runtime asset. */
type BrowserTransforms = {
  readonly encodeCssText: (input: string, encodeTarget: (value: string) => string) => string;
  readonly parseSrcsetCandidates: (input: string) => readonly SrcsetCandidate[];
};

/** Global slot populated by the shared transform asset. */
type TransformGlobal = typeof globalThis & {
  readonly __ringsWebviewTransforms?: BrowserTransforms;
};

/** One-shot effect captured and removed by the trusted shared browser bootstrap. */
type ShellNavigationGlobal = typeof globalThis & {
  __ringsWebviewShellNavigation?: (target: string) => boolean;
};

/** Shared bounded request reader installed before this module executes. */
type RequestBodyRuntime = {
  readonly readGatewayRequestBody: (request: Request, signal?: AbortSignal) => Promise<ArrayBuffer | undefined>;
  readonly workerRuntimeSource: string;
};

/** Global slot installed from the Web Service Worker's canonical request reader. */
type RequestBodyGlobal = typeof globalThis & {
  readonly RingsWebviewWorkerRequest?: RequestBodyRuntime;
};

const TARGET_HEADER = "X-Rings-Webview-Target";
const MAX_CSS_HYDRATION_DEPTH = 8;
const rendererGeneration = parseRendererGeneration();
const pendingRequests = new Map<number, PendingSandboxRequest>();
const objectUrls = new Set<string>();
const preparedElements = new WeakSet<HTMLElement>();
const nativeFetch = globalThis.fetch.bind(globalThis);
const nativeParentPostMessage = globalThis.parent.postMessage.bind(globalThis.parent);
const transforms = sharedTransforms();
const requestBodyRuntime = sharedRequestBodyRuntime();
let nextRequestId = 1;
let currentTarget = "https://example.com/";
let renderGeneration = 0;
let resourceObserver: MutationObserver | undefined;
let dynamicResourceEffects: Promise<void> = Promise.resolve();
let gatewayCapability: GatewayCapability | undefined;
const nestedRenderer = createNestedRenderer({
  currentTarget: (): string => currentTarget,
  normalizeTarget,
  targetFromRewritten,
  request: requestGateway,
  assertCurrentRender,
  reportError,
});
installDynamicFrameBoundary(nestedRenderer);
installShellNavigationEffect();

globalThis.fetch = onionFetch;
installWebviewXmlHttpRequest(onionFetch, reportError);
installDirectNetworkGuards();
const workerBridgeRuntime = installWorkerBridge({
  currentTarget: (): string => currentTarget,
  normalizeTarget,
  request: requestGateway,
  reportError,
});

globalThis.addEventListener("message", routeRendererMessage);

document.addEventListener("click", captureNavigationClick, true);
postToParent({ type: "rings.webview.frame.ready" });

/** Routes parent lifecycle authority separately from recursive child lifecycle messages. */
function routeRendererMessage(event: MessageEvent<unknown>): void {
  if (event.source === globalThis.parent) {
    handleParentRenderMessage(event);
    return;
  }
  nestedRenderer.routeMessage(event);
}

/** Accepts the single trusted render command and its transferred gateway capability. */
function handleParentRenderMessage(event: MessageEvent<unknown>): void {
  const command = parseRendererRenderCommand(event.data);
  if (!command || command.rendererGeneration !== rendererGeneration || !event.ports[0]) return;
  installGatewayPort(event.ports[0]);
  const renderCapability = command.renderCapability;
  void renderDocument(command.target, command.html)
    .then((): void => postToParent({ type: "rings.webview.frame.rendered", renderCapability }))
    .catch((error: unknown): void => {
      const message = errorMessage(error);
      reportError(message);
      postToParent({ type: "rings.webview.frame.renderFailed", renderCapability, error: message });
    });
}

/** Closes non-HTTP browser transports that declarative request rules cannot model. */
function installDirectNetworkGuards(): void {
  const scope = globalThis as typeof globalThis & Record<string, unknown>;
  for (const name of [
    "EventSource",
    "RTCPeerConnection",
    "WebSocket",
    "WebSocketStream",
    "WebTransport",
    "webkitRTCPeerConnection",
  ]) {
    if (!(name in scope)) continue;
    const BlockedTransport = class {
      constructor() {
        throw new TypeError(`${name} is blocked by Rings WebView`);
      }
    };
    Object.defineProperty(scope, name, { configurable: true, value: BlockedTransport, writable: true });
  }
}

/** Captures application-owned link navigation before the browser can dispatch it. */
function captureNavigationClick(event: MouseEvent): void {
  if (event.defaultPrevented || event.button !== 0) return;
  const element = (event.target as Element | null)?.closest<HTMLElement>(
    "a[data-rings-navigation-target], area[data-rings-navigation-target]",
  );
  const target = element?.dataset["ringsNavigationTarget"];
  if (!target) return;
  event.preventDefault();
  requestNavigation(target);
}

/** Exposes one navigation effect only until the shared bootstrap captures it. */
function installShellNavigationEffect(): void {
  const scope = globalThis as ShellNavigationGlobal;
  Object.defineProperty(scope, "__ringsWebviewShellNavigation", {
    configurable: true,
    value: (target: string): boolean => {
      requestNavigation(target);
      return true;
    },
  });
}

/** Renders one rewritten document and resolves only after its initial effects settle. */
async function renderDocument(target: string, html: string): Promise<void> {
  const generation = ++renderGeneration;
  currentTarget = normalizeTarget(target, currentTarget);
  resourceObserver?.disconnect();
  nestedRenderer.release();
  workerBridgeRuntime.release();
  revokeObjectUrls();
  const parsed = new DOMParser().parseFromString(html, "text/html");
  parsed.querySelectorAll('meta[http-equiv="Content-Security-Policy" i], base').forEach((element: Element): void => {
    element.remove();
  });
  for (const element of Array.from(parsed.querySelectorAll<HTMLElement>("*"))) neutralizeElement(element, true);
  await hydrateStylesAndMedia(parsed, generation);
  assertCurrentRender(generation);
  const nextHead = document.createDocumentFragment();
  for (const child of Array.from(parsed.head.childNodes)) nextHead.append(document.importNode(child, true));
  const nextBody = document.createDocumentFragment();
  for (const child of Array.from(parsed.body.childNodes)) nextBody.append(document.importNode(child, true));
  document.head.replaceChildren(nextHead);
  document.body.replaceChildren(nextBody);
  if (parsed.title.trim()) postToParent({ type: "rings.webview.frame.title", title: parsed.title.trim() });
  await nestedRenderer.hydrateFrames(document, generation);
  assertCurrentRender(generation);
  await activateScripts(document, generation);
  await nestedRenderer.hydrateFrames(document, generation);
  if (generation !== renderGeneration) throw new Error("renderer generation was superseded");
  observeDynamicResources();
}

/** Converts networking attributes into inert, typed hydration plans. */
function neutralizeElement(element: HTMLElement, markInlineScript = false): void {
  const frame = element instanceof HTMLIFrameElement ? element : undefined;
  if (frame) nestedRenderer.preserveFramePlan(frame);
  if (!preparedElements.has(element)) {
    clearAuthoredBridgeMetadata(element);
    preparedElements.add(element);
  }
  const tag = element.tagName.toLowerCase();
  if (frame) return;
  if (tag === "a" || tag === "area") {
    preserveNavigationTarget(element, "href");
    return;
  }
  if (tag === "script") {
    preserveResourceTarget(element, "src");
    if (markInlineScript || readResourceHydrationPlans(element).length > 0) element.dataset["ringsInertScript"] = "";
    return;
  }
  if (tag === "link") {
    preserveResourceTarget(element, "href");
    return;
  }
  for (const attribute of ["src", "poster", "data"]) preserveResourceTarget(element, attribute);
  preserveSrcsetTargets(element, "srcset");
  preserveSrcsetTargets(element, "imagesrcset");
}

/** Retains a validated navigation target while replacing its browser-network attribute. */
function preserveNavigationTarget(element: HTMLElement, attribute: string): void {
  const value = element.getAttribute(attribute);
  const target = value ? targetFromRewritten(value) : undefined;
  if (target) {
    element.dataset["ringsNavigationTarget"] = target;
    element.setAttribute(attribute, "#");
  } else if (value && value !== "#" && !value.startsWith("#")) {
    element.setAttribute(attribute, "#");
  }
}

/** Retains a validated resource target while removing its browser-network attribute. */
function preserveResourceTarget(element: HTMLElement, attribute: string): void {
  const value = element.getAttribute(attribute);
  const target = value ? targetFromRewritten(value) : undefined;
  if (target) {
    const plans = readResourceHydrationPlans(element).filter(
      (plan: ResourceHydrationPlan): boolean => plan.attribute !== attribute,
    );
    plans.push({ attribute, target });
    element.dataset["ringsResourcePlans"] = JSON.stringify(plans);
    element.removeAttribute(attribute);
  }
}

/** Removes page-authored attributes from the renderer's private metadata namespace. */
function clearAuthoredBridgeMetadata(element: HTMLElement): void {
  const keepBootstrapMarker = isTrustedBootstrapElement(element);
  const keepNestedPlan = element instanceof HTMLIFrameElement && nestedRenderer.hasFramePlan(element);
  for (const attribute of Array.from(element.attributes)) {
    if (
      attribute.name.startsWith("data-rings-") &&
      !(attribute.name === "data-rings-webview-bootstrap" && keepBootstrapMarker) &&
      !(attribute.name === "data-rings-nested-plan" && keepNestedPlan)
    ) {
      element.removeAttribute(attribute.name);
    }
  }
}

/** Recognizes the Rust-injected shared bootstrap before retaining its recovery marker. */
function isTrustedBootstrapElement(element: Element): element is HTMLScriptElement {
  return (
    element instanceof HTMLScriptElement &&
    element.hasAttribute("data-rings-webview-bootstrap") &&
    (element.textContent ?? "").includes("globalThis.__ringsWebviewBootstrapConfig=") &&
    (element.textContent ?? "").includes("__ringsWebviewGateway")
  );
}

/** Parses the renderer-owned list of independent resource effects. */
function readResourceHydrationPlans(element: HTMLElement): ResourceHydrationPlan[] {
  const raw = element.dataset["ringsResourcePlans"];
  if (!raw) return [];
  try {
    const value: unknown = JSON.parse(raw);
    if (!Array.isArray(value)) return [];
    return value.flatMap((candidate: unknown): ResourceHydrationPlan[] => {
      if (
        !isRecord(candidate) ||
        typeof candidate["attribute"] !== "string" ||
        typeof candidate["target"] !== "string"
      ) {
        return [];
      }
      return [{ attribute: candidate["attribute"], target: candidate["target"] }];
    });
  } catch (_error: unknown) {
    return [];
  }
}

/** Converts every srcset candidate into a shared-contract hydration plan. */
function preserveSrcsetTargets(element: HTMLElement, attribute: "srcset" | "imagesrcset"): void {
  const value = element.getAttribute(attribute);
  if (!value) return;
  const candidates = transforms
    .parseSrcsetCandidates(value)
    .map((candidate: SrcsetCandidate): HydratedSrcsetCandidate => {
      const target = targetFromRewritten(candidate.url);
      return target ? { ...candidate, target } : candidate;
    });
  element.dataset["ringsSrcsetCandidates"] = JSON.stringify(candidates);
  element.dataset["ringsSrcsetAttribute"] = attribute;
  element.removeAttribute(attribute);
}

/** Executes the initial or dynamic resource hydration plan for one DOM root. */
async function hydrateStylesAndMedia(root: ParentNode, generation: number): Promise<void> {
  await hydrateInlineStyles(root, generation);
  const resources = Array.from(root.querySelectorAll<HTMLElement>("[data-rings-resource-plans]"));
  const srcsets = Array.from(root.querySelectorAll<HTMLElement>("[data-rings-srcset-candidates]"));
  await Promise.all([
    ...resources.map((element: HTMLElement): Promise<void> => hydrateResource(element, generation)),
    ...srcsets.map((element: HTMLElement): Promise<void> => hydrateSrcset(element, generation)),
  ]);
}

/** Resolves inline style URLs through the shared CSS tokenizer. */
async function hydrateInlineStyles(root: ParentNode, generation: number): Promise<void> {
  const styles = Array.from(
    root.querySelectorAll<HTMLElement>("style:not([data-rings-css-hydrated]), [style]:not([data-rings-css-hydrated])"),
  );
  await Promise.all(
    styles.map(async (element: HTMLElement): Promise<void> => {
      element.dataset["ringsCssHydrated"] = "";
      if (element instanceof HTMLStyleElement) {
        element.textContent = await hydrateCssText(element.textContent ?? "", currentTarget, generation);
      } else {
        const value = element.getAttribute("style");
        if (value != null) element.setAttribute("style", await hydrateCssText(value, currentTarget, generation));
      }
    }),
  );
}

/** Materializes one ordinary resource as a sandbox-local object URL. */
async function hydrateResource(element: HTMLElement, generation: number): Promise<void> {
  if (element.tagName.toLowerCase() === "script") return;
  const plans = readResourceHydrationPlans(element);
  delete element.dataset["ringsResourcePlans"];
  if (plans.length === 0) return;
  if (element instanceof HTMLLinkElement) {
    if (!element.relList.contains("stylesheet")) return;
    const target = plans.find((plan: ResourceHydrationPlan): boolean => plan.attribute === "href")?.target;
    if (!target) return;
    try {
      const response = await fetchGatewayResource(target);
      assertCurrentRender(generation);
      const style = document.createElement("style");
      const css = new TextDecoder().decode(Uint8Array.from(response.body));
      style.textContent = await hydrateCssText(css, response.url, generation);
      assertCurrentRender(generation);
      element.replaceWith(style);
    } catch (error: unknown) {
      reportError(`resource ${target}: ${errorMessage(error)}`);
    }
    return;
  }
  await Promise.all(
    plans.map(async (plan: ResourceHydrationPlan): Promise<void> => {
      try {
        const response = await fetchGatewayResource(plan.target);
        assertCurrentRender(generation);
        const contentType = webviewHeaderValue(response.headers, "content-type") ?? "application/octet-stream";
        element.setAttribute(plan.attribute, retainObjectUrl(response.body, contentType));
      } catch (error: unknown) {
        reportError(`resource ${plan.target}: ${errorMessage(error)}`);
      }
    }),
  );
}

/** Materializes every responsive-image candidate without discarding descriptors. */
async function hydrateSrcset(element: HTMLElement, generation: number): Promise<void> {
  const raw = element.dataset["ringsSrcsetCandidates"];
  const attribute = element.dataset["ringsSrcsetAttribute"] ?? "srcset";
  if (!raw) return;
  delete element.dataset["ringsSrcsetCandidates"];
  delete element.dataset["ringsSrcsetAttribute"];
  const candidates = parseHydratedSrcsetCandidates(raw);
  const hydrated = await Promise.all(
    candidates.map(async (candidate: HydratedSrcsetCandidate): Promise<SrcsetCandidate | undefined> => {
      if (!candidate.target) return isLocalResource(candidate.url) ? candidate : undefined;
      try {
        const response = await fetchGatewayResource(candidate.target);
        assertCurrentRender(generation);
        const type = webviewHeaderValue(response.headers, "content-type") ?? "application/octet-stream";
        return { url: retainObjectUrl(response.body, type), descriptor: candidate.descriptor };
      } catch (error: unknown) {
        reportError(`srcset ${candidate.target}: ${errorMessage(error)}`);
        return undefined;
      }
    }),
  );
  if (generation !== renderGeneration) return;
  const value = hydrated
    .filter((candidate: SrcsetCandidate | undefined): candidate is SrcsetCandidate => candidate !== undefined)
    .map((candidate: SrcsetCandidate): string =>
      candidate.descriptor ? `${candidate.url} ${candidate.descriptor}` : candidate.url,
    )
    .join(", ");
  if (value) element.setAttribute(attribute, value);
}

/** Executes inert scripts in document order after onion-fetching external sources. */
async function activateScripts(root: ParentNode, generation: number): Promise<void> {
  while (generation === renderGeneration) {
    for (const script of Array.from(root.querySelectorAll<HTMLScriptElement>("script[src]"))) neutralizeElement(script);
    const inertScripts = Array.from(root.querySelectorAll<HTMLScriptElement>("script[data-rings-inert-script]"));
    if (inertScripts.length === 0) return;
    for (const inert of inertScripts) {
      if (generation !== renderGeneration) return;
      delete inert.dataset["ringsInertScript"];
      const script = document.createElement("script");
      const trustedBootstrap = isTrustedBootstrapElement(inert);
      for (const attribute of Array.from(inert.attributes)) {
        if (
          !attribute.name.startsWith("data-rings-") ||
          (attribute.name === "data-rings-webview-bootstrap" && trustedBootstrap)
        ) {
          script.setAttribute(attribute.name, attribute.value);
        }
      }
      const target = readResourceHydrationPlans(inert).find(
        (plan: ResourceHydrationPlan): boolean => plan.attribute === "src",
      )?.target;
      if (target) {
        try {
          const response = await fetchGatewayResource(target);
          script.textContent = new TextDecoder().decode(Uint8Array.from(response.body));
        } catch (error: unknown) {
          reportError(`script ${target}: ${errorMessage(error)}`);
          inert.remove();
          continue;
        }
      } else {
        script.textContent = inert.textContent;
      }
      await replaceScript(inert, script);
    }
  }
}

/** Serializes dynamic DOM resource mutations behind one effect chain. */
function observeDynamicResources(): void {
  resourceObserver?.disconnect();
  resourceObserver = new MutationObserver((records: readonly MutationRecord[]): void => {
    const candidates = new Set<HTMLElement>();
    for (const record of records) {
      if (record.type === "attributes" && record.target instanceof HTMLElement) candidates.add(record.target);
      for (const node of Array.from(record.addedNodes)) {
        if (!(node instanceof HTMLElement)) continue;
        candidates.add(node);
        for (const descendant of Array.from(node.querySelectorAll<HTMLElement>("*"))) candidates.add(descendant);
      }
    }
    for (const element of candidates) neutralizeElement(element);
    const generation = renderGeneration;
    dynamicResourceEffects = dynamicResourceEffects
      .then(async (): Promise<void> => {
        if (generation === renderGeneration) await hydrateStylesAndMedia(document, generation);
        if (generation === renderGeneration) await nestedRenderer.hydrateFrames(document, generation);
        if (generation === renderGeneration) await activateScripts(document, generation);
      })
      .catch((error: unknown): void => reportError(`dynamic resource: ${errorMessage(error)}`));
  });
  resourceObserver.observe(document.documentElement, {
    attributes: true,
    attributeFilter: [
      "action",
      "data",
      "data-rings-nested-plan",
      "formaction",
      "href",
      "imagesrcset",
      "poster",
      "src",
      "srcdoc",
      "srcset",
      "style",
    ],
    childList: true,
    subtree: true,
  });
}

/** Reuses the shared CSS tokenizer to discover both url() and quoted @import targets. */
async function hydrateCssText(
  css: string,
  base: string,
  generation: number,
  depth = 0,
  ancestors: ReadonlySet<string> = new Set(),
): Promise<string> {
  const values = new Set<string>();
  transforms.encodeCssText(css, (value: string): string => {
    values.add(value);
    return value;
  });
  const replacements = new Map<string, string>();
  await Promise.all(
    Array.from(values, async (value: string): Promise<void> => {
      const target = targetFromRewritten(value, base);
      if (!target) return;
      if (depth >= MAX_CSS_HYDRATION_DEPTH || ancestors.has(target)) {
        reportError(`stylesheet resource cycle or depth limit at ${target}`);
        replacements.set(value, "data:text/css,");
        return;
      }
      try {
        const response = await fetchGatewayResource(target);
        assertCurrentRender(generation);
        const type = webviewHeaderValue(response.headers, "content-type") ?? "application/octet-stream";
        if (type.toLowerCase().split(";", 1)[0]?.trim() === "text/css") {
          const nestedCss = new TextDecoder().decode(Uint8Array.from(response.body));
          const nestedAncestors = new Set(ancestors);
          nestedAncestors.add(target);
          const hydratedCss = await hydrateCssText(nestedCss, response.url, generation, depth + 1, nestedAncestors);
          assertCurrentRender(generation);
          replacements.set(value, retainObjectUrl(new TextEncoder().encode(hydratedCss), type));
        } else {
          replacements.set(value, retainObjectUrl(response.body, type));
        }
      } catch (error: unknown) {
        reportError(`stylesheet resource ${target}: ${errorMessage(error)}`);
      }
    }),
  );
  let hydrated = css;
  for (const [source, replacement] of replacements) hydrated = hydrated.split(source).join(replacement);
  return hydrated;
}

/** Replaces an inert script and waits for observable module completion. */
function replaceScript(inert: HTMLScriptElement, script: HTMLScriptElement): Promise<void> {
  return new Promise((resolve): void => {
    if (script.type === "module") {
      const timeout = globalThis.setTimeout(resolve, 5_000);
      const finish = (): void => {
        globalThis.clearTimeout(timeout);
        resolve();
      };
      script.addEventListener("load", finish, { once: true });
      script.addEventListener("error", finish, { once: true });
    }
    inert.replaceWith(script);
    if (script.type !== "module") resolve();
  });
}

/** Routes page-authored fetch or XHR through the trusted parent gateway. */
async function onionFetch(
  input: RequestInfo | URL,
  init?: RequestInit,
  kind: "fetch" | "xhr" = "fetch",
): Promise<Response> {
  const raw = input instanceof Request ? input.url : String(input);
  if (raw.startsWith("blob:") || raw.startsWith("data:")) return nativeFetch(input, init);
  const initialHeaders = new Headers(init?.headers ?? (input instanceof Request ? input.headers : undefined));
  const explicitTarget = initialHeaders.get(TARGET_HEADER);
  initialHeaders.delete(TARGET_HEADER);
  initialHeaders.delete("X-Rings-Webview-Kind");
  const target = explicitTarget ? normalizeTarget(explicitTarget, currentTarget) : normalizeTarget(raw, currentTarget);
  const request = input instanceof Request ? new Request(input, init) : new Request(target, init);
  const bodyBuffer = await requestBodyRuntime.readGatewayRequestBody(request, request.signal);
  const body = bodyBuffer ? Array.from(new Uint8Array(bodyBuffer)) : [];
  const response = await requestGateway(
    {
      target,
      method: request.method,
      headers: Array.from(
        initialHeaders.entries(),
        ([name, value]: [string, string]): WebviewHeader => ({ name, value }),
      ),
      body,
      credentials: request.credentials,
      kind,
      topLevelNavigation: false,
      redirect: request.redirect,
    },
    request.signal,
  );
  if (request.redirect === "manual" && isRedirectStatus(response.status)) return opaqueRedirectResponse();
  if (response.status < 200) {
    throw new TypeError(`Rings onion WebView cannot expose HTTP ${response.status} as a Fetch response`);
  }
  const responseBody = responseHasNullBody(response.status) ? null : Uint8Array.from(response.body);
  const result = new Response(responseBody, {
    status: response.status,
    headers: response.headers.map((header: WebviewHeader): [string, string] => [header.name, header.value]),
  });
  try {
    Object.defineProperties(result, {
      url: { value: response.url },
      redirected: { value: response.redirected },
    });
  } catch (_error: unknown) {
    // Some browser Response implementations expose these metadata fields as non-configurable.
  }
  return result;
}

/** Projects a manual redirect into Fetch's bodyless, headerless filtered response. */
function opaqueRedirectResponse(): Response {
  const response = Response.error();
  try {
    Object.defineProperty(response, "type", { value: "opaqueredirect" });
  } catch (_error: unknown) {
    // The status, headers, and body remain opaque even if this browser fixes the type accessor.
  }
  return response;
}

/** Returns whether Fetch requires a null response body for one status. */
function responseHasNullBody(status: number): boolean {
  return status === 204 || status === 205 || status === 304;
}

/** Prevents a superseded render effect from allocating or committing resources. */
function assertCurrentRender(generation: number): void {
  if (generation !== renderGeneration) throw new Error("renderer generation was superseded");
}

/** Fetches one resource with standard redirect following and a 2xx postcondition. */
async function fetchGatewayResource(target: string): Promise<FrameGatewayResponse> {
  const response = await requestGateway({
    target,
    method: "GET",
    headers: [],
    body: [],
    credentials: "same-origin",
    kind: "subresource",
    topLevelNavigation: false,
    redirect: "follow",
  });
  if (response.status < 200 || response.status >= 300) {
    throw new Error(`resource ${target} returned HTTP ${response.status}`);
  }
  return response;
}

/** Allocates one request identity and transfers ownership to the trusted parent. */
function requestGateway(
  request: FrameGatewayRequest,
  signal?: AbortSignal,
  sourceTarget = currentTarget,
): Promise<FrameGatewayResponse> {
  const requestId = nextRequestId;
  nextRequestId += 1;
  return new Promise((resolve, reject): void => {
    if (signal?.aborted) {
      reject(signal.reason ?? new DOMException("The operation was aborted", "AbortError"));
      return;
    }
    const abort = (): void => {
      pendingRequests.delete(requestId);
      reject(signal?.reason ?? new DOMException("The operation was aborted", "AbortError"));
    };
    const cleanup = (): void => signal?.removeEventListener("abort", abort);
    pendingRequests.set(requestId, {
      resolve: (response: FrameGatewayResponse): void => {
        cleanup();
        resolve(response);
      },
      reject: (error: Error): void => {
        cleanup();
        reject(error);
      },
    });
    signal?.addEventListener("abort", abort, { once: true });
    const capability = gatewayCapability;
    if (!capability) {
      pendingRequests.delete(requestId);
      cleanup();
      reject(new Error("renderer gateway capability is unavailable"));
      return;
    }
    const message = {
      type: "rings.webview.gateway.request",
      requestId,
      request,
      sourceTarget,
    } satisfies RendererGatewayRequestMessage;
    capability.send(message);
  });
}

/** Replaces the private gateway capability before remote scripts execute. */
function installGatewayPort(port: MessagePort): void {
  gatewayCapability?.close();
  const send = port.postMessage.bind(port);
  gatewayCapability = { close: port.close.bind(port), send: (message: unknown): void => send(message) };
  port.addEventListener("message", (event: MessageEvent<unknown>): void => {
    const message = parseRendererPortMessage(event.data);
    if (!message) return;
    if (message.type === "rings.webview.gateway.response") settleGatewayResponse(message);
    else nestedRenderer.routeBrowserNavigation(message.sessionId, message.target);
  });
  port.start();
}

/** Settles exactly one outstanding request response. */
function settleGatewayResponse(message: RendererGatewayResponseMessage): void {
  const pending = pendingRequests.get(message.requestId);
  if (!pending) return;
  pendingRequests.delete(message.requestId);
  if ("error" in message) {
    pending.reject(new Error(message.error));
    return;
  }
  try {
    pending.resolve(parseFrameGatewayResponse(message.response));
  } catch (error: unknown) {
    pending.reject(error instanceof Error ? error : new Error(String(error)));
  }
}

/** Resolves a rewritten gateway path or target-relative URL. */
function targetFromRewritten(value: string, base = currentTarget): string | undefined {
  const gatewayTarget = decodeControlledWebviewTarget(value);
  if (gatewayTarget) return gatewayTarget;
  if (isLocalResource(value)) return undefined;
  try {
    return normalizeTarget(value, base);
  } catch (_error: unknown) {
    return undefined;
  }
}

/** Restricts a renderer URL to the HTTPS target contract. */
function normalizeTarget(value: string, base: string): string {
  return normalizeHttpsTarget(value, base);
}

/** Requests one application-owned top-level navigation from the parent. */
function requestNavigation(target: string): void {
  postToParent({ type: "rings.webview.frame.navigate", target: normalizeTarget(target, currentTarget) });
}

/** Adds the immutable renderer-generation witness to one parent message. */
function postToParent(message: RendererFrameEvent): void {
  nativeParentPostMessage({ ...message, rendererGeneration }, "*");
}

/** Parses the renderer-generation witness from the packaged frame URL. */
function parseRendererGeneration(): number {
  const injected = (globalThis as typeof globalThis & { readonly __ringsRendererGeneration?: unknown })
    .__ringsRendererGeneration;
  const raw = new URL(globalThis.location.href).searchParams.get("navigation") ?? "0";
  const generation = typeof injected === "number" ? injected : Number(raw);
  if (!Number.isSafeInteger(generation) || generation < 0) {
    throw new Error("invalid Rings WebView renderer generation");
  }
  return generation;
}

/** Owns one object URL until the current document is replaced. */
function retainObjectUrl(body: Iterable<number>, contentType: string): string {
  const url = URL.createObjectURL(new Blob([Uint8Array.from(body)], { type: contentType }));
  objectUrls.add(url);
  return url;
}

/** Releases every object URL owned by the outgoing document. */
function revokeObjectUrls(): void {
  for (const url of objectUrls) URL.revokeObjectURL(url);
  objectUrls.clear();
}

/** Parses a serialized srcset hydration plan. */
function parseHydratedSrcsetCandidates(value: string): readonly HydratedSrcsetCandidate[] {
  const parsed: unknown = JSON.parse(value);
  if (!Array.isArray(parsed)) throw new Error("invalid srcset hydration plan");
  return parsed.flatMap((candidate: unknown): HydratedSrcsetCandidate[] => {
    if (!isRecord(candidate) || typeof candidate["url"] !== "string" || typeof candidate["descriptor"] !== "string") {
      return [];
    }
    return [
      {
        url: candidate["url"],
        descriptor: candidate["descriptor"],
        ...(typeof candidate["target"] === "string" ? { target: candidate["target"] } : {}),
      },
    ];
  });
}

/** Returns whether a URL can be used without a browser network connection. */
function isLocalResource(value: string): boolean {
  const lower = value.trim().toLowerCase();
  return (
    !lower ||
    lower.startsWith("#") ||
    lower.startsWith("data:") ||
    lower.startsWith("blob:") ||
    lower.startsWith("javascript:") ||
    lower.startsWith("about:")
  );
}

/** Returns the preloaded shared transforms after validating their callable surface. */
function sharedTransforms(): BrowserTransforms {
  const value = (globalThis as TransformGlobal).__ringsWebviewTransforms;
  if (!value || typeof value.encodeCssText !== "function" || typeof value.parseSrcsetCandidates !== "function") {
    throw new Error("shared Rings WebView transforms are unavailable");
  }
  return value;
}

/** Requires the canonical Web/Extension request-body implementation. */
function sharedRequestBodyRuntime(): RequestBodyRuntime {
  const value = (globalThis as RequestBodyGlobal).RingsWebviewWorkerRequest;
  if (!value || typeof value.readGatewayRequestBody !== "function" || typeof value.workerRuntimeSource !== "string") {
    throw new Error("shared WebView request-body runtime is unavailable");
  }
  return value;
}

/** Emits one renderer diagnostic without crossing the trusted DOM boundary. */
function reportError(message: string): void {
  console.error(`[Rings WebView] ${message}`);
}
