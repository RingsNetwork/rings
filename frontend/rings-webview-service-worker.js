"use strict";

importScripts("/assets/webview-worker-response.js?gateway-response-protocol=1");

const {
  gatewayFailure,
  gatewayFailureDocument,
} = self.RingsWebviewWorkerResponse;
const gatewayPrefix = "/webview/";
const runtimeGatewayPrefix = `${gatewayPrefix.replace(/\/$/, "")}-runtime/`;
const runtimeTargetHeader = "x-rings-webview-target";
const requestTimeoutMs = 30_000;
const gatewayFetchDeadlineMs = requestTimeoutMs + 1_000;
const webviewOverlayScriptPath = "/assets/webview-overlay.js";
const webviewHistoryGuardMarker = "data-rings-webview-history-guard";
const webviewHistoryGuardScriptTag = `<script ${webviewHistoryGuardMarker}>(() => {
  "use strict";
  if (globalThis.__ringsWebviewHistoryGuard) return;
  Object.defineProperty(globalThis, "__ringsWebviewHistoryGuard", { value: true });
  const gatewayPrefix = "/webview/";
  function currentTargetUrl() {
    if (!location.pathname.startsWith(gatewayPrefix)) return undefined;
    const encoded = location.pathname.slice(gatewayPrefix.length);
    if (!encoded) return undefined;
    try {
      const target = new URL(decodeURIComponent(encoded));
      if (location.search) {
        const extra = location.search.slice(1);
        target.search = target.search ? target.search + "&" + extra : "?" + extra;
      }
      if (!target.hash && location.hash) target.hash = location.hash;
      return target;
    } catch (_error) {
      return undefined;
    }
  }
  function controlledGatewayUrl(target) {
    return gatewayPrefix + encodeURIComponent(target.href);
  }
  function rewrittenHistoryUrl(value) {
    const currentTarget = currentTargetUrl();
    if (!currentTarget) return value;
    const target = new URL(value, currentTarget.href);
    if (target.origin !== currentTarget.origin) {
      throw new DOMException("Rings WebView blocks history navigation outside the current target origin.", "SecurityError");
    }
    return controlledGatewayUrl(target);
  }
  function guardHistoryMethod(method) {
    const original = History.prototype[method];
    if (typeof original !== "function") return;
    Object.defineProperty(History.prototype, method, {
      value(state, title, url) {
        if (arguments.length >= 3 && url != null) {
          return Reflect.apply(original, this, [state, title, rewrittenHistoryUrl(url)]);
        }
        return Reflect.apply(original, this, arguments);
      },
      configurable: false,
      writable: false,
    });
  }
  guardHistoryMethod("pushState");
  guardHistoryMethod("replaceState");
})();</script>`;
const webviewOverlayScriptTag = `<script src="${webviewOverlayScriptPath}"></script>`;
const gatewayContentSecurityPolicy = "default-src 'self' data: blob:; base-uri 'self'; connect-src 'self'; font-src 'self' data:; form-action 'self'; frame-src 'self' data: blob:; img-src 'self' data: blob:; media-src 'self' data: blob:; object-src 'self'; script-src 'self' data: 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' blob:; style-src 'self' data: 'unsafe-inline'; worker-src 'none'";
const minimumGatewayHostCapabilityLength = 32;
let gatewayHostClientId = null;
let gatewayHostCapability = null;
const trustedShellClientIds = new Set();
const trustedHostClientIds = new Set();
const trustedDebugClientIds = new Set();
const clientSourceTargets = new Map();
const debugClientScopes = new Map();
const debugHistory = [];
let nextRequestId = 1;

self.addEventListener("install", (event) => {
  event.waitUntil(self.skipWaiting());
});

self.addEventListener("activate", (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener("message", (event) => {
  const clientId = event.source?.id;
  const reply = event.ports?.[0];
  if (event.data?.type === "rings-webview-debug-entry" && event.data.entry) {
    const publish = acceptDebugEntry(clientId, event.data.capability).then((ok) => {
      if (!ok) {
        queueDebug("worker", "Rejected untrusted debug entry", "warning");
        reply?.postMessage({ ok: false, error: "untrusted debug entry" });
        return;
      }
      const entry = event.data.entry;
      queueDebug(
        entry.scope || "host",
        entry.message || "unknown event",
        entry.level || "info",
        entry.resource,
        entry.at,
        entry.onion,
      );
      reply?.postMessage({ ok: true });
    });
    event.waitUntil?.(publish);
    return;
  }
  if (event.data?.type === "rings-webview-host-register" && typeof clientId === "string" && clientId) {
    const registration = registerGatewayHostClient(clientId, event.data.capability).then((ok) => {
      if (ok) {
        queueDebug("worker", "Updated local Rings node gateway host");
        reply?.postMessage({ ok: true });
      } else {
        queueDebug("worker", "Rejected untrusted Rings node gateway host registration", "warning");
        reply?.postMessage({ ok: false, error: "untrusted gateway host registration" });
      }
    });
    event.waitUntil?.(registration);
    return;
  }
  if (event.data?.type === "rings-webview-debug-register" && typeof clientId === "string" && clientId) {
    const registration = registerDebugClient(clientId, event.data.capability).then((ok) => {
      if (ok) {
        queueDebug("worker", "Registered WebView debug client");
        reply?.postMessage({ ok: true });
      } else {
        queueDebug("worker", "Rejected untrusted debug client registration", "warning");
        reply?.postMessage({ ok: false, error: "untrusted debug client registration" });
      }
    });
    event.waitUntil?.(registration);
    return;
  }
  reply?.postMessage({ ok: false, error: "unsupported gateway registration" });
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin || !isGatewayControlledPathname(url.pathname)) {
    rememberShellNavigationClient(event, url);
    return;
  }
  event.respondWith(handleGatewayFetchWithTimeout(event));
});

function handleGatewayFetchWithTimeout(event) {
  const requestId = nextRequestId;
  nextRequestId += 1;
  const startedAt = performance.now();
  const controller = new AbortController();
  let timeout;
  let settled = false;
  const timeoutResponse = new Promise((resolve) => {
    timeout = globalThis.setTimeout(() => {
      if (settled) return;
      controller.abort();
      const request = debugRequestForFailure(event.request);
      emitResourceDebug(
        requestId,
        request,
        startedAt,
        "failed",
        `#${requestId} gateway request exceeded the Service Worker deadline`,
        "error",
        504,
      );
      resolve(gatewayDeadlineFailure());
    }, gatewayFetchDeadlineMs);
  });
  return Promise.race([
    handleGatewayFetch(event, requestId, startedAt, controller.signal),
    timeoutResponse,
  ]).finally(() => {
    settled = true;
    globalThis.clearTimeout(timeout);
  });
}

async function handleGatewayFetch(event, requestId, startedAt, signal = undefined) {
  let request;
  try {
    request = await serializeRequest(event, signal);
    if (signal?.aborted) {
      return gatewayDeadlineFailure();
    }
    rememberNavigationClientTarget(event, request);
  } catch (error) {
    if (error === gatewayRequestCancelled) {
      return gatewayDeadlineFailure();
    }
    request = debugRequestForFailure(event.request);
    emitResourceDebug(
      requestId,
      request,
      startedAt,
      "failed",
      `#${requestId} rejected malformed gateway request: ${errorMessage(error)}`,
      "error",
      400,
    );
    return gatewayFailure(
      400,
      errorMessage(error),
      "Malformed Rings WebView gateway request.",
      "invalid_gateway_request",
    );
  }
  emitResourceDebug(
    requestId,
    request,
    startedAt,
    "intercepted",
    `#${requestId} intercepted ${request.kind} ${request.method} ${requestedTarget(request.requested)} (mode=${event.request.mode}, destination=${event.request.destination || "none"})`,
  );
  const host = await gatewayHostClient();
  if (signal?.aborted) {
    return gatewayDeadlineFailure();
  }
  if (!host) {
    emitResourceDebug(
      requestId,
      request,
      startedAt,
      "failed",
      `#${requestId} rejected: no local gateway host`,
      "error",
      503,
    );
    return gatewayFailure(
      503,
      "Start a local Rings node before opening WebView.",
      "Local Rings node gateway is unavailable.",
      "local_gateway_unavailable",
    );
  }
  emitResourceDebug(
    requestId,
    request,
    startedAt,
    "dispatched",
    `#${requestId} Request sent to the local Rings gateway`,
  );
  const response = await requestGatewayResponse(host, request, requestId, signal);
  if (signal?.aborted) {
    return gatewayDeadlineFailure();
  }
  if (!response?.ok) {
    const status = response?.status || 502;
    emitResourceDebug(
      requestId,
      request,
      startedAt,
      "failed",
      `#${requestId} gateway failure ${status}: ${response?.error || "unknown error"}`,
      "error",
      status,
    );
    return gatewayFailure(
      response?.status || 502,
      response?.error || "gateway request failed",
      response?.errorSummary,
      response?.errorCode,
    );
  }
  try {
    const headers = new Headers();
    for (const header of response.headers || []) {
      headers.append(header.name, header.value);
    }
    emitResourceDebug(
      requestId,
      request,
      startedAt,
      "completed",
      `#${requestId} returned ${response.status}`,
      "info",
      response.status,
    );
    const body = responseMustNotHaveBody(response.status)
      ? null
      : controlledNavigationBody(request, response.status, headers, response.body || null);
    return new Response(body, {
      status: response.status,
      headers,
    });
  } catch (error) {
    emitResourceDebug(
      requestId,
      request,
      startedAt,
      "failed",
      `#${requestId} invalid gateway response: ${String(error)}`,
      "error",
      502,
    );
    return gatewayFailure(
      502,
      `invalid gateway response: ${String(error)}`,
      "The local gateway returned an invalid response.",
      "invalid_gateway_response",
    );
  }
}

function gatewayDeadlineFailure() {
  return gatewayFailure(
    504,
    "local Rings node gateway timed out before the Service Worker completed request handling",
    "Local Rings node gateway timed out.",
    "local_gateway_timeout",
  );
}

const gatewayRequestCancelled = Object.freeze({});

function responseMustNotHaveBody(status) {
  return status === 204 || status === 205 || status === 304;
}

function controlledNavigationBody(request, status, headers, body) {
  if (request.kind !== "navigation" || !body || status < 200 || status >= 300) {
    return body;
  }
  const bytes = bodyBytes(body);
  if (!bytes) {
    return body;
  }
  const contentType = (headers.get("content-type") || "").toLowerCase();
  if (contentType && !contentType.includes("text/html") && !contentType.includes("application/xhtml+xml")) {
    return body;
  }
  prepareControlledNavigationHeaders(headers);
  const text = decodeUtf8(bytes);
  if (!text || !looksLikeHtml(text)) {
    return body;
  }
  const injected = injectControlledNavigationScripts(text, request.topLevelNavigation !== false);
  if (injected === text) {
    return body;
  }
  return new TextEncoder().encode(injected);
}

function prepareControlledNavigationHeaders(headers) {
  headers.delete("content-length");
  headers.delete("content-encoding");
  headers.delete("content-security-policy-report-only");
  headers.delete("x-frame-options");
  headers.set("content-security-policy", gatewayContentSecurityPolicy);
}

function bodyBytes(body) {
  if (body instanceof Uint8Array) {
    return body;
  }
  if (body instanceof ArrayBuffer) {
    return new Uint8Array(body);
  }
  if (ArrayBuffer.isView(body)) {
    return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
  }
  if (typeof body === "string") {
    return new TextEncoder().encode(body);
  }
  return undefined;
}

function decodeUtf8(bytes) {
  try {
    return new TextDecoder("utf-8", { fatal: false }).decode(bytes);
  } catch (_error) {
    return "";
  }
}

function looksLikeHtml(text) {
  return /^\uFEFF?\s*(?:<!--[\s\S]*?-->\s*)*(?:<!doctype\s+html\b|<html\b|<head\b|<body\b)/i.test(text);
}

function injectWebviewOverlay(html) {
  const guarded = injectWebviewHistoryGuard(html);
  if (/<\/head\s*>/i.test(guarded)) {
    return guarded.replace(/<\/head\s*>/i, `${webviewOverlayScriptTag}</head>`);
  }
  if (/<body\b[^>]*>/i.test(guarded)) {
    return guarded.replace(/<body\b[^>]*>/i, (bodyTag) => `${bodyTag}${webviewOverlayScriptTag}`);
  }
  return `${guarded}\n${webviewOverlayScriptTag}`;
}

function injectControlledNavigationScripts(html, includeOverlay) {
  if (includeOverlay) {
    return injectWebviewOverlay(html);
  }
  return injectWebviewHistoryGuard(html);
}

function injectWebviewHistoryGuard(html) {
  const leading = /^\uFEFF?\s*(?:(?:<!--[\s\S]*?-->)\s*)*(?:<!doctype\s+html\b[^>]*>\s*)?/i.exec(html);
  const index = leading ? leading[0].length : 0;
  return `${html.slice(0, index)}${webviewHistoryGuardScriptTag}${html.slice(index)}`;
}

function emitResourceDebug(requestId, request, startedAt, phase, message, level = "info", status = undefined) {
  const resource = {
    requestId,
    target: requestedTarget(request.requested),
    sourceTarget: request.sourceTarget,
    method: request.method,
    kind: request.kind,
    phase,
    durationMs: Math.max(0, Math.round(performance.now() - startedAt)),
  };
  if (status !== undefined) {
    resource.status = status;
  }
  queueDebug("worker", message, level, resource);
}

function queueDebug(scope, message, level = "info", resource = undefined, at = undefined, onion = undefined) {
  void emitDebug(scope, message, level, resource, at, onion).catch((error) => {
    console.warn("Rings WebView debug delivery failed", error);
  });
}

async function emitDebug(scope, message, level = "info", resource = undefined, at = undefined, onion = undefined) {
  const entry = {
    type: "rings-webview-debug",
    at: at || new Date().toISOString(),
    scope,
    message,
    level,
  };
  if (resource) {
    entry.resource = resource;
  }
  if (onion) {
    entry.onion = onion;
  }
  debugHistory.push(entry);
  if (debugHistory.length > 200) {
    debugHistory.splice(0, debugHistory.length - 200);
  }
  const clients = await debugClientsForEntry(entry);
  await Promise.all(
    clients.map((client) => client.postMessage(entry)),
  );
}

async function debugClientsForEntry(entry) {
  const clientsById = new Map();
  for (const [clientId, scope] of debugClientScopes) {
    const client = await self.clients.get(clientId);
    if (!client || !debugScopeStillValid(clientId, client, scope)) {
      debugClientScopes.delete(clientId);
      continue;
    }
    if (debugScopeAllowsEntry(scope, entry)) {
      clientsById.set(client.id, client);
    }
  }
  return [...clientsById.values()];
}

function requestedTarget(url) {
  return targetFromGatewayUrl(url) || url;
}

async function gatewayHostClient() {
  return registeredGatewayHostClient();
}

async function registeredGatewayHostClient() {
  if (!gatewayHostClientId) {
    return undefined;
  }
  const client = await self.clients.get(gatewayHostClientId);
  if (!client || !isTrustedGatewayHostClient(gatewayHostClientId, client)) {
    gatewayHostClientId = null;
    gatewayHostCapability = null;
    return undefined;
  }
  return client;
}

async function registerGatewayHostClient(clientId, capability) {
  if (!isValidGatewayHostCapability(capability)) {
    return false;
  }
  const client = await self.clients.get(clientId);
  if (!client || !isTrustedGatewayHostRegistrationClient(clientId, client)) {
    return false;
  }
  if (gatewayHostCapability && gatewayHostCapability !== capability) {
    return false;
  }
  trustedShellClientIds.add(clientId);
  clientSourceTargets.delete(clientId);
  trustedHostClientIds.add(clientId);
  gatewayHostClientId = clientId;
  gatewayHostCapability = capability;
  return true;
}

async function registerDebugClient(clientId, capability) {
  const client = await self.clients.get(clientId);
  if (!client) {
    return false;
  }
  const scope = debugClientRegistrationScope(clientId, client, capability);
  if (scope !== false) {
    trustedDebugClientIds.add(clientId);
    debugClientScopes.set(clientId, scope);
    replayDebugHistory(client, scope);
    return true;
  }
  return false;
}

async function acceptDebugEntry(clientId, capability) {
  if (!hasGatewayHostCapability(clientId, capability)) {
    return false;
  }
  const host = await registeredGatewayHostClient();
  return host?.id === clientId;
}

function hasGatewayHostCapability(clientId, capability) {
  return typeof clientId === "string" && Boolean(clientId) && Boolean(gatewayHostCapability) && capability === gatewayHostCapability;
}

function isValidGatewayHostCapability(capability) {
  return typeof capability === "string" && capability.length >= minimumGatewayHostCapabilityLength;
}

function isTrustedGatewayHostClient(clientId, client) {
  return trustedHostClientIds.has(clientId)
    && isTrustedShellClient(clientId)
    && clientFramePermitsGatewayHostRegistration(client)
    && isTrustedGatewayHostUrl(client.url);
}

function isTrustedShellClient(clientId) {
  return trustedShellClientIds.has(clientId) && !clientSourceTargets.has(clientId);
}

function isTrustedGatewayHostRegistrationClient(clientId, client) {
  return !clientSourceTargets.has(clientId)
    && clientFramePermitsGatewayHostRegistration(client)
    && isTrustedGatewayHostUrl(client.url);
}

function clientFramePermitsGatewayHostRegistration(client) {
  return client.frameType === "top-level";
}

function clientFramePermitsDebugRegistration(client) {
  return client.frameType === "top-level" || client.frameType === "auxiliary";
}

function debugClientRegistrationScope(clientId, client, capability) {
  if (
    hasGatewayHostCapability(clientId, capability)
    && isTrustedShellClient(clientId)
    && clientFramePermitsDebugRegistration(client)
    && isTrustedDebugClientUrl(client.url)
  ) {
    return shellDebugScope();
  }
  const target = targetFromGatewayUrl(client.url);
  if (
    trustedDebugClientIds.has(clientId)
    && target
    && clientFramePermitsDebugRegistration(client)
  ) {
    return targetDebugScope(target);
  }
  return false;
}

function debugScopeStillValid(clientId, client, scope) {
  if (!trustedDebugClientIds.has(clientId) || !clientFramePermitsDebugRegistration(client)) {
    return false;
  }
  if (scope?.kind === "shell") {
    return isTrustedShellClient(clientId) && isTrustedDebugClientUrl(client.url);
  }
  if (scope?.kind === "target") {
    const target = targetFromGatewayUrl(client.url);
    return Boolean(target && sameTargetUrl(target, scope.target));
  }
  return false;
}

function debugScopeAllowsEntry(scope, entry) {
  if (scope?.kind === "shell") {
    return true;
  }
  if (scope?.kind === "target") {
    return debugEntryMatchesTargetScope(entry, scope.target);
  }
  return false;
}

function shellDebugScope() {
  return { kind: "shell" };
}

function targetDebugScope(target) {
  return { kind: "target", target };
}

function debugEntryMatchesTargetScope(entry, target) {
  const payload = entry?.resource || entry?.onion;
  if (!payload) {
    return false;
  }
  if (payload.sourceTarget) {
    return sameTargetUrl(payload.sourceTarget, target);
  }
  if (payload.target && payload.kind === "navigation") {
    return sameTargetUrl(payload.target, target);
  }
  return false;
}

function sameTargetUrl(left, right) {
  try {
    return new URL(left).href === new URL(right).href;
  } catch (_error) {
    return false;
  }
}

function replayDebugHistory(client, scope) {
  for (const entry of debugHistory) {
    if (debugScopeAllowsEntry(scope, entry)) {
      client.postMessage(entry);
    }
  }
}

function isTrustedGatewayHostUrl(url) {
  return isTrustedShellUrl(url, "#node");
}

function isTrustedDebugClientUrl(url) {
  return isTrustedShellUrl(url, "#webview");
}

function isTrustedShellUrl(url, hashPrefix) {
  try {
    const parsed = new URL(url);
    return parsed.origin === self.location.origin
      && !isGatewayControlledPathname(parsed.pathname)
      && parsed.hash.startsWith(hashPrefix);
  } catch (_error) {
    return false;
  }
}

function resetGatewayHostForTest() {
  gatewayHostClientId = null;
  gatewayHostCapability = null;
  trustedShellClientIds.clear();
  trustedHostClientIds.clear();
  trustedDebugClientIds.clear();
  clientSourceTargets.clear();
  debugClientScopes.clear();
  debugHistory.splice(0, debugHistory.length);
}

async function serializeRequest(event, signal = undefined) {
  const request = event.request;
  const sourceTarget = await sourceTargetForClient(event.clientId);
  throwIfGatewayRequestCancelled(signal);
  const kind = requestKind(request);
  const runtimeTarget = runtimeGatewayTarget(request);
  const body = request.method === "GET" || request.method === "HEAD"
    ? undefined
    : await request.clone().arrayBuffer();
  throwIfGatewayRequestCancelled(signal);
  return {
    requested: runtimeTarget ? controlledGatewayUrl(runtimeTarget) : request.url,
    sourceTarget,
    method: request.method,
    credentials: request.credentials,
    topLevelNavigation: isTopLevelNavigationRequest(request),
    headers: [...request.headers]
      .filter(([name]) => !isRuntimeControlHeader(name))
      .map(([name, value]) => ({ name, value })),
    body,
    kind,
  };
}

function throwIfGatewayRequestCancelled(signal) {
  if (signal?.aborted) {
    throw gatewayRequestCancelled;
  }
}

function debugRequestForFailure(request) {
  let requested = request.url;
  try {
    const runtimeTarget = runtimeGatewayTarget(request);
    if (runtimeTarget) {
      requested = controlledGatewayUrl(runtimeTarget);
    }
  } catch (_error) {}
  return {
    requested,
    sourceTarget: undefined,
    method: request.method || "GET",
    credentials: request.credentials,
    headers: [],
    body: undefined,
    kind: "invalid",
  };
}

async function sourceTargetForClient(clientId) {
  if (!clientId) {
    return undefined;
  }
  const client = await self.clients.get(clientId);
  if (!client) {
    clientSourceTargets.delete(clientId);
    debugClientScopes.delete(clientId);
    trustedDebugClientIds.delete(clientId);
    return undefined;
  }
  const currentTarget = targetFromGatewayUrl(client.url);
  if (!currentTarget) {
    clientSourceTargets.delete(clientId);
    if (trustedDebugClientIds.has(clientId) && isTrustedDebugClientUrl(client.url)) {
      debugClientScopes.set(clientId, shellDebugScope());
    } else {
      debugClientScopes.delete(clientId);
      trustedDebugClientIds.delete(clientId);
    }
    return undefined;
  }
  const previousTarget = clientSourceTargets.get(clientId);
  if (previousTarget !== currentTarget) {
    clientSourceTargets.set(clientId, currentTarget);
    trustedShellClientIds.delete(clientId);
    trustedHostClientIds.delete(clientId);
    if (trustedDebugClientIds.has(clientId)) {
      debugClientScopes.set(clientId, targetDebugScope(currentTarget));
    } else {
      debugClientScopes.delete(clientId);
    }
  }
  return currentTarget;
}

function rememberNavigationClientTarget(event, request) {
  if (request.kind !== "navigation") {
    return false;
  }
  const sourceTarget = targetFromGatewayUrl(request.requested);
  if (!sourceTarget) {
    return false;
  }
  let remembered = false;
  if (rememberClientSourceTarget(event.resultingClientId, sourceTarget, event.clientId)) {
    remembered = true;
  }
  if (!remembered && request.topLevelNavigation) {
    remembered = rememberClientSourceTarget(event.clientId, sourceTarget);
  }
  return remembered;
}

function rememberShellNavigationClient(event, url) {
  if (url.origin !== self.location.origin || isGatewayControlledPathname(url.pathname) || !isTopLevelNavigationRequest(event.request)) {
    return false;
  }
  let remembered = false;
  if (rememberTrustedShellClient(event.resultingClientId, event.clientId)) {
    remembered = true;
  }
  if (!remembered) {
    remembered = rememberTrustedShellClient(event.clientId);
  }
  return remembered;
}

function rememberClientSourceTargetForTest(clientId, sourceTarget) {
  if (typeof clientId !== "string" || !clientId || typeof sourceTarget !== "string" || !sourceTarget) {
    return false;
  }
  return rememberClientSourceTarget(clientId, sourceTarget);
}

function rememberClientSourceTarget(clientId, sourceTarget, debugTrustSourceClientId = clientId) {
  if (typeof clientId !== "string" || !clientId || typeof sourceTarget !== "string" || !sourceTarget) {
    return false;
  }
  clientSourceTargets.set(clientId, sourceTarget);
  trustedShellClientIds.delete(clientId);
  trustedHostClientIds.delete(clientId);
  if (trustedDebugClientIds.has(clientId) || trustedDebugClientIds.has(debugTrustSourceClientId)) {
    trustedDebugClientIds.add(clientId);
    debugClientScopes.set(clientId, targetDebugScope(sourceTarget));
  } else {
    debugClientScopes.delete(clientId);
  }
  return true;
}

function rememberTrustedShellClientForTest(clientId) {
  if (typeof clientId !== "string" || !clientId) {
    return false;
  }
  return rememberTrustedShellClient(clientId);
}

function rememberTrustedShellClient(clientId, debugTrustSourceClientId = clientId) {
  if (typeof clientId !== "string" || !clientId) {
    return false;
  }
  trustedShellClientIds.add(clientId);
  clientSourceTargets.delete(clientId);
  if (trustedDebugClientIds.has(clientId) || trustedDebugClientIds.has(debugTrustSourceClientId)) {
    trustedDebugClientIds.add(clientId);
    debugClientScopes.set(clientId, shellDebugScope());
  }
  return true;
}

function isGatewayControlledPathname(pathname) {
  return pathname.startsWith(gatewayPrefix) || pathname.startsWith(runtimeGatewayPrefix);
}

function runtimeGatewayTarget(request) {
  const url = new URL(request.url);
  if (url.origin !== self.location.origin || !url.pathname.startsWith(runtimeGatewayPrefix)) {
    return undefined;
  }
  const rawTarget = request.headers.get(runtimeTargetHeader);
  if (!rawTarget) {
    throw new Error("missing X-Rings-Webview-Target");
  }
  let target;
  try {
    target = new URL(rawTarget);
  } catch (_error) {
    throw new Error("invalid X-Rings-Webview-Target");
  }
  if (target.protocol !== "http:" && target.protocol !== "https:") {
    throw new Error(`unsupported X-Rings-Webview-Target scheme: ${target.protocol.replace(/:$/, "")}`);
  }
  return target.href;
}

function controlledGatewayUrl(target) {
  const url = new URL(target);
  return new URL(`${gatewayPrefix}${encodeURIComponent(url.href)}`, self.location.origin).href;
}

function isRuntimeControlHeader(name) {
  const lower = name.toLowerCase();
  return lower === "x-rings-webview-kind" || lower === runtimeTargetHeader;
}

function targetFromGatewayUrl(value) {
  let url;
  try {
    url = new URL(value);
  } catch (_error) {
    return undefined;
  }
  if (url.origin !== self.location.origin || !url.pathname.startsWith(gatewayPrefix)) {
    return undefined;
  }
  const encoded = url.pathname.slice(gatewayPrefix.length);
  if (!encoded) {
    return undefined;
  }
  try {
    return decodeURIComponent(encoded);
  } catch (_error) {
    return undefined;
  }
}

function isTopLevelNavigationRequest(request) {
  return request.mode === "navigate" && request.destination === "document";
}

function requestKind(request) {
  if (isNavigationRequest(request)) {
    return "navigation";
  }
  const taggedKind = runtimeKindTag(request);
  if (isRuntimeReadableRequest(request)) {
    return taggedKind || "fetch";
  }
  if (taggedKind) {
    throw new Error(`X-Rings-Webview-Kind is only valid for runtime requests, got ${taggedKind}`);
  }
  return "subresource";
}

function isNavigationRequest(request) {
  return (
    request.mode === "navigate"
    || request.destination === "document"
    || request.destination === "iframe"
  );
}

function isRuntimeReadableRequest(request) {
  return !request.destination;
}

function runtimeKindTag(request) {
  const rawKind = request.headers.get("x-rings-webview-kind");
  if (rawKind == null) {
    return undefined;
  }
  const values = rawKind.split(",").map((value) => value.trim()).filter(Boolean);
  if (values.length !== 1 || !isRuntimeKind(values[0])) {
    throw new Error(`invalid X-Rings-Webview-Kind: ${rawKind}`);
  }
  return values[0];
}

function isRuntimeKind(kind) {
  return kind === "fetch" || kind === "xhr";
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}

function requestGatewayResponse(host, request, requestId, signal = undefined) {
  return new Promise((resolve) => {
    const channel = new MessageChannel();
    let settled = false;
    let timeout;
    const finish = (response, cancelHost) => {
      if (settled) return;
      settled = true;
      globalThis.clearTimeout(timeout);
      signal?.removeEventListener("abort", cancel);
      channel.port1.close();
      if (cancelHost) {
        host.postMessage({
          type: "rings-webview-gateway-cancel",
          requestId,
        });
      }
      resolve(response);
    };
    const cancel = () => {
      finish({
        ok: false,
        status: 504,
        errorCode: "local_gateway_timeout",
        errorSummary: "Local Rings node gateway timed out.",
        error: "local Rings node gateway timed out",
      }, true);
    };
    if (signal?.aborted) {
      cancel();
      return;
    }
    signal?.addEventListener("abort", cancel, { once: true });
    timeout = globalThis.setTimeout(() => {
      cancel();
    }, requestTimeoutMs);
    channel.port1.onmessage = (event) => {
      finish(event.data, false);
    };
    host.postMessage(
      { type: "rings-webview-gateway-request", requestId, request },
      [channel.port2],
    );
  });
}
