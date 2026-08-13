"use strict";

importScripts(
  "/assets/webview-worker-response.js?gateway-response-protocol=1",
  "/assets/webview-worker-navigation.js?gateway-navigation-protocol=1",
  "/assets/webview-worker-request.js?gateway-request-protocol=1",
);

const { gatewayFailure, gatewayFailureDocument } = self.RingsWebviewWorkerResponse;
const { injectControlledNavigationScripts, isTrustedShellEntrypoint, sameTargetOrigin, sameTargetUrl } =
  self.RingsWebviewWorkerNavigation;
const {
  gatewayRequestBodyLimitBytes,
  gatewayRequestMayHaveBody,
  isGatewayRequestBodyTooLarge,
  readGatewayRequestBody,
  validateGatewayRequestBodyMetadata,
} = self.RingsWebviewWorkerRequest;
const gatewayPrefix = "/webview/";
const runtimeGatewayPrefix = `${gatewayPrefix.replace(/\/$/, "")}-runtime/`;
const runtimeTargetHeader = "x-rings-webview-target";
const requestTimeoutMs = 30_000;
const gatewayFetchDeadlineMs = requestTimeoutMs + 1_000;
const webviewOverlayScriptPath = "/assets/webview-overlay.js";
const gatewayContentSecurityPolicy =
  "sandbox allow-scripts allow-forms allow-popups allow-downloads; default-src 'self' data: blob:; base-uri 'self'; connect-src 'self'; font-src 'self' data:; form-action 'self'; frame-src 'self' data: blob:; img-src 'self' data: blob:; media-src 'self' data: blob:; object-src 'self'; script-src 'self' data: 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' blob:; style-src 'self' data: 'unsafe-inline'; webrtc 'block'; worker-src 'none'";
const minimumGatewayHostCapabilityLength = 32;
const clientStatePruneInterval = 64;
const shellRegistrationLifetimeMs = 5_000;
const gatewayHostLifetimeMs = Number.isFinite(globalThis.__ringsWebviewGatewayHostLifetimeMs)
  ? Math.max(0, globalThis.__ringsWebviewGatewayHostLifetimeMs)
  : 45_000;
// A retained FetchEvent may keep an unread, not-yet-size-checked request stream alive. Bound the
// body budget to six active readers without an unread waiter queue; bodyless GET/HEAD requests
// bypass this gate and remain covered by the host's ordinary request limiter.
const maxRetainedGatewayBodyBytes = 48 * 1024 * 1024;
const maxActiveGatewayBodies = Math.floor(
  maxRetainedGatewayBodyBytes / gatewayRequestBodyLimitBytes,
);
let gatewayHostClientId = null;
let gatewayHostCapability = null;
const trustedShellClientIds = new Set();
const trustedHostClientIds = new Set();
const trustedDebugClientIds = new Set();
const clientSourceTargets = new Map();
const debugClientScopes = new Map();
const debugHistory = [];
const pendingShellRegistrationLifetimes = new Map();
let nextRequestId = 1;
let requestsUntilClientStatePrune = clientStatePruneInterval;
let activeGatewayBodies = 0;

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
      return ok;
    });
    // waitUntil is the worker-side lease. The host renews it before expiry so an active gateway
    // session cannot lose its in-memory capability merely because the browser reclaims the worker.
    event.waitUntil?.(
      registration.then((registered) =>
        registered
          ? new Promise((resolve) => globalThis.setTimeout(resolve, gatewayHostLifetimeMs))
          : undefined,
      ),
    );
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
    const clientId = trustedShellNavigationClientId(event, url);
    if (clientId) {
      // Keep the navigation event alive until the newly loaded shell presents its capability.
      // Otherwise the worker may be terminated between the fetch witness and the message event,
      // erasing the only authority that distinguishes a real shell navigation from history spoofing.
      event.waitUntil?.(holdShellNavigationForHostRegistration(clientId));
    }
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
  return Promise.race([handleGatewayFetch(event, requestId, startedAt, controller.signal), timeoutResponse])
    .catch((error) => {
      const request = debugRequestForFailure(event.request);
      emitResourceDebug(
        requestId,
        request,
        startedAt,
        "failed",
        `#${requestId} unexpected Service Worker failure: ${errorMessage(error)}`,
        "error",
        502,
      );
      return gatewayFailure(
        502,
        "local Rings gateway request failed",
        "Local Rings node gateway request failed.",
        "gateway_internal_failure",
      );
    })
    .finally(() => {
      settled = true;
      globalThis.clearTimeout(timeout);
    });
}

async function handleGatewayFetch(event, requestId, startedAt, signal = undefined) {
  try {
    // Reject a declared oversize before the FetchEvent enters the bounded waiter queue.
    validateGatewayRequestBodyMetadata(event.request);
  } catch (error) {
    return rejectGatewayRequestInput(event, requestId, startedAt, error);
  }
  if (!gatewayRequestMayHaveBody(event.request)) {
    return handleAdmittedGatewayFetch(event, requestId, startedAt, signal);
  }
  let release;
  try {
    release = await acquireGatewayBodyPermit(signal);
  } catch (error) {
    if (error === gatewayRequestCancelled) {
      return gatewayDeadlineFailure();
    }
    if (error === gatewayBodyAdmissionFull) {
      return gatewayFailure(
        503,
        "local Rings gateway request admission is full",
        "Local Rings gateway is busy. Try again shortly.",
        "gateway_request_admission_full",
      );
    }
    throw error;
  }
  try {
    return await handleAdmittedGatewayFetch(event, requestId, startedAt, signal);
  } finally {
    release();
  }
}

async function handleAdmittedGatewayFetch(event, requestId, startedAt, signal = undefined) {
  await maybePruneTrackedClientState();
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
    return rejectGatewayRequestInput(event, requestId, startedAt, error);
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
      "local Rings gateway returned an invalid response",
      "The local gateway returned an invalid response.",
      "invalid_gateway_response",
    );
  }
}

function rejectGatewayRequestInput(event, requestId, startedAt, error) {
  const request = debugRequestForFailure(event.request);
  const bodyTooLarge = isGatewayRequestBodyTooLarge(error);
  const status = bodyTooLarge ? 413 : 400;
  const summary = bodyTooLarge
    ? "Rings WebView request body is too large."
    : "Malformed Rings WebView gateway request.";
  const code = bodyTooLarge ? "gateway_request_body_too_large" : "invalid_gateway_request";
  emitResourceDebug(
    requestId,
    request,
    startedAt,
    "failed",
    `#${requestId} rejected gateway request: ${errorMessage(error)}`,
    "error",
    status,
  );
  return gatewayFailure(status, errorMessage(error), summary, code);
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
const gatewayBodyAdmissionFull = Object.freeze({});

function acquireGatewayBodyPermit(signal = undefined) {
  if (signal?.aborted) {
    return Promise.reject(gatewayRequestCancelled);
  }
  if (activeGatewayBodies >= maxActiveGatewayBodies) {
    return Promise.reject(gatewayBodyAdmissionFull);
  }
  activeGatewayBodies += 1;
  return Promise.resolve(gatewayBodyPermitRelease());
}

function gatewayBodyPermitRelease() {
  let released = false;
  return () => {
    if (released) return;
    released = true;
    activeGatewayBodies = Math.max(0, activeGatewayBodies - 1);
  };
}

function responseMustNotHaveBody(status) {
  return status === 204 || status === 205 || status === 304;
}

function controlledNavigationBody(request, status, headers, body) {
  if (request.kind !== "navigation") {
    return body;
  }
  prepareControlledNavigationHeaders(headers);
  if (status < 200 || status >= 300) {
    return body;
  }
  if (!body) return body;
  const bytes = bodyBytes(body);
  if (!bytes) {
    return body;
  }
  const mediaType = (headers.get("content-type") || "").split(";", 1)[0].trim().toLowerCase();
  if (mediaType && mediaType !== "text/html" && mediaType !== "application/xhtml+xml") {
    return body;
  }
  const text = decodeUtf8(bytes);
  const xmlDocument = mediaType === "application/xhtml+xml";
  if (!text || !looksLikeHtml(text, xmlDocument)) {
    return body;
  }
  const injected = injectControlledNavigationScripts(
    text,
    request.topLevelNavigation !== false,
    webviewOverlayScriptPath,
    xmlDocument,
  );
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
  headers.set("x-content-type-options", "nosniff");
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

function looksLikeHtml(text, xmlDocument) {
  const xmlDeclaration = xmlDocument ? "(?:<\\?xml[\\s\\S]*?\\?>\\s*)?" : "";
  return new RegExp(
    `^\\uFEFF?\\s*${xmlDeclaration}(?:<!--[\\s\\S]*?-->\\s*)*(?:<!doctype\\s+html\\b|<html\\b|<head\\b|<body\\b)`,
    "i",
  ).test(text);
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
  await Promise.all(clients.map((client) => client.postMessage(entry)));
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
  clientSourceTargets.delete(clientId);
  trustedHostClientIds.add(clientId);
  gatewayHostClientId = clientId;
  gatewayHostCapability = capability;
  finishShellNavigationHostRegistration(clientId);
  return true;
}

function holdShellNavigationForHostRegistration(clientId) {
  pendingShellRegistrationLifetimes.get(clientId)?.();
  return new Promise((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) {
        return;
      }
      settled = true;
      globalThis.clearTimeout(timeout);
      if (pendingShellRegistrationLifetimes.get(clientId) === finish) {
        pendingShellRegistrationLifetimes.delete(clientId);
      }
      resolve();
    };
    const timeout = globalThis.setTimeout(finish, shellRegistrationLifetimeMs);
    pendingShellRegistrationLifetimes.set(clientId, finish);
  });
}

function finishShellNavigationHostRegistration(clientId) {
  pendingShellRegistrationLifetimes.get(clientId)?.();
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
  return (
    typeof clientId === "string" &&
    Boolean(clientId) &&
    Boolean(gatewayHostCapability) &&
    capability === gatewayHostCapability
  );
}

function isValidGatewayHostCapability(capability) {
  return typeof capability === "string" && capability.length >= minimumGatewayHostCapabilityLength;
}

function isTrustedGatewayHostClient(clientId, client) {
  return (
    trustedHostClientIds.has(clientId) &&
    isTrustedShellClient(clientId) &&
    clientFramePermitsGatewayHostRegistration(client) &&
    isTrustedGatewayHostUrl(client.url)
  );
}

function isTrustedShellClient(clientId) {
  return trustedShellClientIds.has(clientId) && !clientSourceTargets.has(clientId);
}

function isTrustedGatewayHostRegistrationClient(clientId, client) {
  // A location-shaped URL is not authority: a controlled target document can rewrite history.
  // Registration requires a shell-navigation witness created by this worker's fetch boundary.
  return (
    isTrustedShellClient(clientId) &&
    clientFramePermitsGatewayHostRegistration(client) &&
    isTrustedGatewayHostUrl(client.url)
  );
}

function clientFramePermitsGatewayHostRegistration(client) {
  return client.frameType === "top-level";
}

function clientFramePermitsDebugRegistration(client) {
  return client.frameType === "top-level" || client.frameType === "auxiliary";
}

function debugClientRegistrationScope(clientId, client, capability) {
  if (
    hasGatewayHostCapability(clientId, capability) &&
    isTrustedShellClient(clientId) &&
    clientFramePermitsDebugRegistration(client) &&
    isTrustedDebugClientUrl(client.url)
  ) {
    return shellDebugScope();
  }
  const target = targetFromGatewayUrl(client.url);
  if (trustedDebugClientIds.has(clientId) && target && clientFramePermitsDebugRegistration(client)) {
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

function replayDebugHistory(client, scope) {
  for (const entry of debugHistory) {
    if (debugScopeAllowsEntry(scope, entry)) {
      client.postMessage(entry);
    }
  }
}

function isTrustedGatewayHostUrl(url) {
  return isTrustedShellUrl(url);
}

function isTrustedDebugClientUrl(url) {
  return isTrustedShellUrl(url);
}

function isTrustedShellUrl(url) {
  try {
    const parsed = new URL(url);
    // WindowClient.url does not provide a trustworthy fragment witness. Authority comes from the
    // top-level fetch navigation recorded in trustedShellClientIds plus the unguessable host
    // capability; this check only constrains the client to the application-owned shell path.
    return parsed.origin === self.location.origin && isTrustedShellEntrypoint(parsed.pathname);
  } catch (_error) {
    return false;
  }
}

function resetGatewayHostForTest() {
  for (const finish of pendingShellRegistrationLifetimes.values()) {
    finish();
  }
  pendingShellRegistrationLifetimes.clear();
  gatewayHostClientId = null;
  gatewayHostCapability = null;
  trustedShellClientIds.clear();
  trustedHostClientIds.clear();
  trustedDebugClientIds.clear();
  clientSourceTargets.clear();
  debugClientScopes.clear();
  debugHistory.splice(0, debugHistory.length);
  requestsUntilClientStatePrune = clientStatePruneInterval;
}

async function maybePruneTrackedClientState() {
  requestsUntilClientStatePrune -= 1;
  if (requestsUntilClientStatePrune > 0) {
    return;
  }
  requestsUntilClientStatePrune = clientStatePruneInterval;
  try {
    await pruneTrackedClientState();
  } catch (error) {
    queueDebug("worker", `Could not prune stale Service Worker client state: ${errorMessage(error)}`, "warning");
  }
}

async function pruneTrackedClientState() {
  const clients = await self.clients.matchAll({
    includeUncontrolled: true,
    type: "window",
  });
  const liveClientIds = new Set(clients.map((client) => client.id));
  for (const clientId of trackedClientIds()) {
    if (liveClientIds.has(clientId)) continue;
    trustedShellClientIds.delete(clientId);
    trustedHostClientIds.delete(clientId);
    trustedDebugClientIds.delete(clientId);
    clientSourceTargets.delete(clientId);
    debugClientScopes.delete(clientId);
  }
  if (gatewayHostClientId && !liveClientIds.has(gatewayHostClientId)) {
    gatewayHostClientId = null;
    gatewayHostCapability = null;
  }
}

function trackedClientIds() {
  return new Set([
    ...trustedShellClientIds,
    ...trustedHostClientIds,
    ...trustedDebugClientIds,
    ...clientSourceTargets.keys(),
    ...debugClientScopes.keys(),
  ]);
}

async function serializeRequest(event, signal = undefined) {
  const request = event.request;
  const sourceTarget = await sourceTargetForClient(event.clientId);
  throwIfGatewayRequestCancelled(signal);
  const kind = requestKind(request);
  const runtimeTarget = runtimeGatewayTarget(request);
  const body = await readGatewayRequestBody(request, signal);
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
  // Authority comes only from a navigation event observed by this worker. History may refine
  // the path/query/fragment, but it cannot change the witnessed virtual origin. After a worker
  // restart no witness exists, and a cross-origin URL mutation must fail closed.
  if (!previousTarget || !sameTargetOrigin(previousTarget, currentTarget)) {
    clientSourceTargets.delete(clientId);
    trustedShellClientIds.delete(clientId);
    trustedHostClientIds.delete(clientId);
    trustedDebugClientIds.delete(clientId);
    debugClientScopes.delete(clientId);
    return undefined;
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
  return Boolean(trustedShellNavigationClientId(event, url));
}

function trustedShellNavigationClientId(event, url) {
  if (
    url.origin !== self.location.origin ||
    !isTrustedShellEntrypoint(url.pathname) ||
    !isTopLevelNavigationRequest(event.request)
  ) {
    return undefined;
  }
  if (rememberTrustedShellClient(event.resultingClientId, event.clientId)) {
    return event.resultingClientId;
  }
  if (rememberTrustedShellClient(event.clientId)) {
    return event.clientId;
  }
  return undefined;
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
  return request.mode === "navigate" || request.destination === "document" || request.destination === "iframe";
}

function isRuntimeReadableRequest(request) {
  return !request.destination;
}

function runtimeKindTag(request) {
  const rawKind = request.headers.get("x-rings-webview-kind");
  if (rawKind == null) {
    return undefined;
  }
  const values = rawKind
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
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
      finish(
        {
          ok: false,
          status: 504,
          errorCode: "local_gateway_timeout",
          errorSummary: "Local Rings node gateway timed out.",
          error: "local Rings node gateway timed out",
        },
        true,
      );
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
    host.postMessage({ type: "rings-webview-gateway-request", requestId, request }, [channel.port2]);
  });
}
