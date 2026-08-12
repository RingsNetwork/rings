// biome-ignore-all lint/complexity/useLiteralKeys: Protocol codecs validate untrusted records before field access.
/**
 * Shared algebra and pure codecs for the trusted Extension WebView boundary.
 */

export const CONTROLLED_WEBVIEW_ORIGIN = "https://rings-webview.invalid";
export const WEBVIEW_GATEWAY_PREFIX = "/webview/";
export const MAX_WEBVIEW_REDIRECTS = 10;

const NativeError = globalThis.Error;
const NativeNumber = globalThis.Number;
const NativeString = globalThis.String;
const NativeUint8Array = globalThis.Uint8Array;
const NativeURL = globalThis.URL;
const nativeArrayIsArray = globalThis.Array.isArray;
const nativeDecodeURIComponent = globalThis.decodeURIComponent;
const nativeEncodeURIComponent = globalThis.encodeURIComponent;
const nativeNumberIsInteger = globalThis.Number.isInteger;
const nativeNumberIsSafeInteger = globalThis.Number.isSafeInteger;
const nativeObjectEntries = globalThis.Object.entries;

/** Finite request kinds accepted by the Rust gateway. */
export type WebviewRequestKind = "navigation" | "subresource" | "fetch" | "xhr";
/** Browser credential modes preserved across the bridge. */
export type WebviewCredentials = "omit" | "same-origin" | "include";

/** One normalized HTTP header. */
export type WebviewHeader = {
  readonly name: string;
  readonly value: string;
};

/** Complete request passed from the trusted extension host to Rust. */
export type WebviewGatewayRequest = {
  readonly requested: string;
  readonly sourceTarget?: string;
  readonly method: string;
  readonly headers: readonly WebviewHeader[];
  readonly body: readonly number[];
  readonly credentials: WebviewCredentials;
  readonly kind: WebviewRequestKind;
  readonly topLevelNavigation: boolean;
};

/** Untrusted response envelope returned by the wasm bridge. */
export type RawWebviewGatewayResponse = {
  readonly ok?: unknown;
  readonly status?: unknown;
  readonly headers?: unknown;
  readonly body?: unknown;
  readonly error?: unknown;
  readonly errorCode?: unknown;
  readonly errorSummary?: unknown;
};

/** Validated renderer request before redirect evaluation. */
export type FrameGatewayRequest = {
  readonly target: string;
  readonly method: string;
  readonly headers: readonly WebviewHeader[];
  readonly body: readonly number[];
  readonly credentials: WebviewCredentials;
  readonly kind: WebviewRequestKind;
  readonly topLevelNavigation: boolean;
  readonly redirect: RequestRedirect;
};

/** Final renderer response after redirect evaluation. */
export type FrameGatewayResponse = {
  readonly status: number;
  readonly headers: readonly WebviewHeader[];
  readonly body: readonly number[];
  readonly url: string;
  readonly redirected: boolean;
};

/** Finite lifecycle events emitted by an opaque renderer. */
export type RendererFrameEvent =
  | { readonly type: "rings.webview.frame.ready" }
  | { readonly type: "rings.webview.frame.rendered"; readonly renderCapability: string }
  | {
      readonly type: "rings.webview.frame.renderFailed";
      readonly renderCapability: string;
      readonly error: string;
    }
  | { readonly type: "rings.webview.frame.navigate"; readonly target: string }
  | { readonly type: "rings.webview.frame.title"; readonly title: string };

/** Renderer event paired with the immutable generation that emitted it. */
export type RendererFrameMessage = RendererFrameEvent & { readonly rendererGeneration: number };

/** Single document command accepted by an opaque renderer. */
export type RendererRenderCommand = {
  readonly type: "rings.webview.render";
  readonly rendererGeneration: number;
  readonly renderCapability: string;
  readonly target: string;
  readonly html: string;
};

/** Request envelope carried only by a private renderer MessagePort. */
export type RendererGatewayRequestMessage = {
  readonly type: "rings.webview.gateway.request";
  readonly requestId: number;
  readonly request: unknown;
  readonly sourceTarget?: string;
};

/** Response envelope carried only by a private renderer MessagePort. */
export type RendererGatewayResponseMessage =
  | { readonly type: "rings.webview.gateway.response"; readonly requestId: number; readonly response: unknown }
  | { readonly type: "rings.webview.gateway.response"; readonly requestId: number; readonly error: string };

/** Browser-observed navigation routed to an exact nested renderer session. */
export type RendererBrowserNavigationMessage = {
  readonly type: "rings.webview.browser.navigate";
  readonly sessionId: string;
  readonly target: string;
};

/** Finite messages accepted from a renderer's private parent port. */
export type RendererPortMessage = RendererGatewayResponseMessage | RendererBrowserNavigationMessage;

/** Returns whether an unknown value is a non-null record. */
export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** Parses header records or tuple pairs at an untrusted protocol boundary. */
export function parseWebviewHeaders(value: unknown): readonly WebviewHeader[] {
  if (!nativeArrayIsArray(value)) {
    return [];
  }
  return value.flatMap((entry: unknown): WebviewHeader[] => {
    if (nativeArrayIsArray(entry) && typeof entry[0] === "string" && typeof entry[1] === "string") {
      return [{ name: entry[0], value: entry[1] }];
    }
    if (isRecord(entry) && typeof entry["name"] === "string" && typeof entry["value"] === "string") {
      return [{ name: entry["name"], value: entry["value"] }];
    }
    return [];
  });
}

/** Parses an untrusted JSON byte array. */
export function parseWebviewBytes(value: unknown, label = "gateway body"): readonly number[] {
  if (!nativeArrayIsArray(value)) {
    return [];
  }
  return value.map((byte: unknown): number => {
    if (typeof byte !== "number" || !nativeNumberIsInteger(byte) || byte < 0 || byte > 255) {
      throw new NativeError(`${label} contains a non-byte value`);
    }
    return byte;
  });
}

/** Normalizes Chrome's JSON representation of arrays and typed arrays. */
export function parseWebviewByteView(value: unknown): Uint8Array {
  if (value instanceof NativeUint8Array) {
    return value;
  }
  if (nativeArrayIsArray(value)) {
    return new NativeUint8Array(parseWebviewBytes(value));
  }
  if (!isRecord(value)) {
    return new NativeUint8Array();
  }
  const bytes = nativeObjectEntries(value)
    .filter(([key, byte]: [string, unknown]): boolean => /^\d+$/.test(key) && typeof byte === "number")
    .sort(([left]: [string, unknown], [right]: [string, unknown]): number => NativeNumber(left) - NativeNumber(right))
    .map(([, byte]: [string, unknown]): number => NativeNumber(byte));
  return new NativeUint8Array(parseWebviewBytes(bytes));
}

/** Parses an untrusted renderer request into the complete finite request algebra. */
export function parseFrameGatewayRequest(value: unknown, sourceTarget: string): FrameGatewayRequest {
  if (!isRecord(value)) throw new NativeError("renderer request is not an object");
  const target = normalizeHttpsTarget(typeof value["target"] === "string" ? value["target"] : "", sourceTarget);
  const method = typeof value["method"] === "string" ? value["method"].toUpperCase() : "GET";
  const kind = isWebviewRequestKind(value["kind"]) ? value["kind"] : "subresource";
  const credentials = isWebviewCredentials(value["credentials"]) ? value["credentials"] : "same-origin";
  const redirect = isRequestRedirect(value["redirect"]) ? value["redirect"] : "follow";
  return {
    target,
    method,
    headers: parseWebviewHeaders(value["headers"]),
    body: parseWebviewBytes(value["body"], "renderer request body"),
    credentials,
    kind,
    topLevelNavigation: false,
    redirect,
  };
}

/** Parses a complete lifecycle message emitted by an opaque renderer. */
export function parseRendererFrameMessage(value: unknown): RendererFrameMessage | undefined {
  if (!isRecord(value) || !isRendererGeneration(value["rendererGeneration"])) return undefined;
  const rendererGeneration = value["rendererGeneration"];
  switch (value["type"]) {
    case "rings.webview.frame.ready":
      return { type: value["type"], rendererGeneration };
    case "rings.webview.frame.rendered":
      return typeof value["renderCapability"] === "string"
        ? { type: value["type"], rendererGeneration, renderCapability: value["renderCapability"] }
        : undefined;
    case "rings.webview.frame.renderFailed":
      return typeof value["renderCapability"] === "string" && typeof value["error"] === "string"
        ? {
            type: value["type"],
            rendererGeneration,
            renderCapability: value["renderCapability"],
            error: value["error"],
          }
        : undefined;
    case "rings.webview.frame.navigate":
      return typeof value["target"] === "string"
        ? { type: value["type"], rendererGeneration, target: value["target"] }
        : undefined;
    case "rings.webview.frame.title":
      return typeof value["title"] === "string"
        ? { type: value["type"], rendererGeneration, title: value["title"] }
        : undefined;
    default:
      return undefined;
  }
}

/** Parses the single command accepted from a renderer's trusted parent. */
export function parseRendererRenderCommand(value: unknown): RendererRenderCommand | undefined {
  if (
    !isRecord(value) ||
    value["type"] !== "rings.webview.render" ||
    !isRendererGeneration(value["rendererGeneration"]) ||
    typeof value["renderCapability"] !== "string" ||
    typeof value["target"] !== "string" ||
    typeof value["html"] !== "string"
  ) {
    return undefined;
  }
  return {
    type: value["type"],
    rendererGeneration: value["rendererGeneration"],
    renderCapability: value["renderCapability"],
    target: value["target"],
    html: value["html"],
  };
}

/** Parses a request arriving through a renderer's private gateway capability. */
export function parseRendererGatewayRequestMessage(value: unknown): RendererGatewayRequestMessage | undefined {
  if (
    !isRecord(value) ||
    value["type"] !== "rings.webview.gateway.request" ||
    !isRequestId(value["requestId"]) ||
    (value["sourceTarget"] !== undefined && typeof value["sourceTarget"] !== "string")
  ) {
    return undefined;
  }
  return {
    type: value["type"],
    requestId: value["requestId"],
    request: value["request"],
    ...(typeof value["sourceTarget"] === "string" ? { sourceTarget: value["sourceTarget"] } : {}),
  };
}

/** Parses one response or browser-navigation command from a private parent port. */
export function parseRendererPortMessage(value: unknown): RendererPortMessage | undefined {
  if (!isRecord(value)) return undefined;
  if (value["type"] === "rings.webview.gateway.response" && isRequestId(value["requestId"])) {
    if (typeof value["error"] === "string") {
      return { type: value["type"], requestId: value["requestId"], error: value["error"] };
    }
    if ("response" in value) {
      return { type: value["type"], requestId: value["requestId"], response: value["response"] };
    }
    return undefined;
  }
  if (
    value["type"] === "rings.webview.browser.navigate" &&
    typeof value["sessionId"] === "string" &&
    typeof value["target"] === "string"
  ) {
    return { type: value["type"], sessionId: value["sessionId"], target: value["target"] };
  }
  return undefined;
}

/** Constructs a successful private gateway response from validated domain values. */
export function rendererGatewaySuccess(
  requestId: number,
  response: FrameGatewayResponse,
): RendererGatewayResponseMessage {
  return { type: "rings.webview.gateway.response", requestId, response };
}

/** Constructs a failed private gateway response at the application boundary. */
export function rendererGatewayFailure(requestId: number, error: string): RendererGatewayResponseMessage {
  return { type: "rings.webview.gateway.response", requestId, error };
}

/** Parses one normalized gateway response at an opaque renderer boundary. */
export function parseFrameGatewayResponse(value: unknown): FrameGatewayResponse {
  if (
    !isRecord(value) ||
    typeof value["status"] !== "number" ||
    !isHttpStatus(value["status"]) ||
    typeof value["url"] !== "string" ||
    typeof value["redirected"] !== "boolean"
  ) {
    throw new NativeError("invalid renderer gateway response");
  }
  return {
    status: value["status"],
    headers: parseWebviewHeaders(value["headers"]),
    body: parseWebviewBytes(value["body"]),
    url: normalizeAbsoluteHttpsTarget(value["url"]),
    redirected: value["redirected"],
  };
}

/** Returns whether a number belongs to the finite HTTP status domain. */
export function isHttpStatus(status: number): boolean {
  return nativeNumberIsInteger(status) && status >= 100 && status <= 599;
}

/** Normalizes an absolute URL under the HTTPS-only onion-exit contract. */
export function normalizeAbsoluteHttpsTarget(value: string): string {
  return normalizeParsedHttpsTarget(new NativeURL(value));
}

/** Restricts a target to the HTTPS onion-exit contract. */
export function normalizeHttpsTarget(value: string, base: string): string {
  return normalizeParsedHttpsTarget(new NativeURL(value, base));
}

/** Applies the credential and scheme law to one already parsed target. */
function normalizeParsedHttpsTarget(target: URL): string {
  if (target.protocol !== "https:") {
    throw new NativeError("Rings onion WebView accepts HTTPS targets only");
  }
  target.username = "";
  target.password = "";
  return target.href;
}

/** Converts a target into the synthetic route consumed by the Rust gateway policy. */
export function controlledWebviewUrl(target: string): string {
  return `${CONTROLLED_WEBVIEW_ORIGIN}${WEBVIEW_GATEWAY_PREFIX}${nativeEncodeURIComponent(target)}`;
}

/** Decodes exactly one synthetic controlled-origin gateway URL. */
export function decodeControlledWebviewTarget(value: string): string | undefined {
  let candidate: URL;
  try {
    candidate = new NativeURL(value, CONTROLLED_WEBVIEW_ORIGIN);
  } catch (_error: unknown) {
    return undefined;
  }
  if (
    candidate.origin !== CONTROLLED_WEBVIEW_ORIGIN ||
    !candidate.pathname.startsWith(WEBVIEW_GATEWAY_PREFIX) ||
    candidate.search ||
    candidate.hash
  ) {
    return undefined;
  }
  const encoded = candidate.pathname.slice(WEBVIEW_GATEWAY_PREFIX.length);
  if (!encoded) {
    return undefined;
  }
  try {
    return normalizeHttpsTarget(nativeDecodeURIComponent(encoded), CONTROLLED_WEBVIEW_ORIGIN);
  } catch (_error: unknown) {
    return undefined;
  }
}

/** Returns a case-insensitive header value. */
export function webviewHeaderValue(headers: readonly WebviewHeader[], name: string): string | undefined {
  const normalized = name.toLowerCase();
  return headers.find((header: WebviewHeader): boolean => header.name.toLowerCase() === normalized)?.value;
}

/** Returns whether an HTTP status participates in redirect processing. */
export function isRedirectStatus(status: number): boolean {
  return status === 301 || status === 302 || status === 303 || status === 307 || status === 308;
}

/** Resolves a gateway-normalized redirect against the upstream response URL. */
export function resolveWebviewRedirect(headers: readonly WebviewHeader[], source: string): string | undefined {
  const location = webviewHeaderValue(headers, "location");
  if (!location) {
    return undefined;
  }
  const controlled = decodeControlledWebviewTarget(location);
  return controlled ?? normalizeHttpsTarget(new NativeURL(location, source).href, source);
}

/** Applies the Fetch redirect method law for one status transition. */
export function redirectedWebviewRequest(
  request: FrameGatewayRequest,
  status: number,
  target: string,
): FrameGatewayRequest {
  const crossesOrigin = new NativeURL(request.target).origin !== new NativeURL(target).origin;
  const safeHeaders = request.headers.filter((header: WebviewHeader): boolean => {
    const name = header.name.toLowerCase();
    return !crossesOrigin || (name !== "authorization" && name !== "proxy-authorization");
  });
  const becomesGet =
    (status === 303 && request.method !== "HEAD") || ((status === 301 || status === 302) && request.method === "POST");
  if (!becomesGet) {
    return { ...request, target, headers: safeHeaders };
  }
  return {
    ...request,
    target,
    method: "GET",
    headers: safeHeaders.filter((header: WebviewHeader): boolean => {
      const name = header.name.toLowerCase();
      return name !== "content-length" && name !== "content-type";
    }),
    body: [],
  };
}

/** Returns whether a value is one gateway request kind. */
function isWebviewRequestKind(value: unknown): value is WebviewRequestKind {
  return value === "navigation" || value === "subresource" || value === "fetch" || value === "xhr";
}

/** Returns whether a value is one credential mode. */
export function isWebviewCredentials(value: unknown): value is WebviewCredentials {
  return value === "omit" || value === "same-origin" || value === "include";
}

/** Returns whether a value is one supported Fetch redirect mode. */
function isRequestRedirect(value: unknown): value is RequestRedirect {
  return value === "error" || value === "follow" || value === "manual";
}

/** Returns whether an unknown number is a packaged renderer generation. */
function isRendererGeneration(value: unknown): value is number {
  return typeof value === "number" && nativeNumberIsSafeInteger(value) && value >= 0;
}

/** Returns whether an unknown number can own a private request slot. */
function isRequestId(value: unknown): value is number {
  return typeof value === "number" && nativeNumberIsSafeInteger(value) && value > 0;
}

/** Converts unknown failures into stable diagnostics. */
export function errorMessage(error: unknown): string {
  return error instanceof NativeError ? error.message : NativeString(error);
}
