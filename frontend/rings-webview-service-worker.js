"use strict";

const gatewayPrefix = "/webview/";
const requestTimeoutMs = 30_000;
let gatewayHostClientId = null;
const debugClientIds = new Set();
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
    const entry = event.data.entry;
    void emitDebug(
      entry.scope || "host",
      entry.message || "unknown event",
      entry.level || "info",
      entry.resource,
      entry.at,
      entry.onion,
    );
    reply?.postMessage({ ok: true });
    return;
  }
  if (event.data?.type === "rings-webview-host-register" && typeof clientId === "string" && clientId) {
    updateGatewayHost(clientId);
    void emitDebug("worker", "Updated local Rings node gateway host");
    reply?.postMessage({ ok: true });
    return;
  }
  if (event.data?.type === "rings-webview-debug-register" && typeof clientId === "string" && clientId) {
    debugClientIds.add(clientId);
    void self.clients.get(clientId).then(async (client) => {
      for (const entry of debugHistory) {
        client?.postMessage(entry);
      }
      await emitDebug("worker", "Registered popup debug client");
    });
    reply?.postMessage({ ok: true });
    return;
  }
  reply?.postMessage({ ok: false, error: "unsupported gateway registration" });
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin || !url.pathname.startsWith(gatewayPrefix)) {
    return;
  }
  event.respondWith(handleGatewayFetch(event));
});

async function handleGatewayFetch(event) {
  const requestId = nextRequestId;
  nextRequestId += 1;
  const startedAt = performance.now();
  const request = await serializeRequest(event);
  await emitResourceDebug(
    requestId,
    request,
    startedAt,
    "intercepted",
    `#${requestId} intercepted ${request.kind} ${request.method} ${requestedTarget(request.requested)} (mode=${event.request.mode}, destination=${event.request.destination || "none"})`,
  );
  const host = await gatewayHostClient();
  if (!host) {
    await emitResourceDebug(
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
  await emitResourceDebug(
    requestId,
    request,
    startedAt,
    "dispatched",
    `#${requestId} dispatched to the local gateway host`,
  );
  const response = await requestGatewayResponse(host, request);
  if (!response?.ok) {
    const status = response?.status || 502;
    await emitResourceDebug(
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
    await emitResourceDebug(
      requestId,
      request,
      startedAt,
      "completed",
      `#${requestId} returned ${response.status}`,
      "info",
      response.status,
    );
    return new Response(responseMustNotHaveBody(response.status) ? null : response.body || null, {
      status: response.status,
      headers,
    });
  } catch (error) {
    await emitResourceDebug(
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

function responseMustNotHaveBody(status) {
  return status === 204 || status === 205 || status === 304;
}

async function emitResourceDebug(requestId, request, startedAt, phase, message, level = "info", status = undefined) {
  const resource = {
    requestId,
    target: requestedTarget(request.requested),
    method: request.method,
    kind: request.kind,
    phase,
    durationMs: Math.max(0, Math.round(performance.now() - startedAt)),
  };
  if (status !== undefined) {
    resource.status = status;
  }
  await emitDebug("worker", message, level, resource);
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
  const clients = await debugClients();
  await Promise.all(
    clients.map((client) => client.postMessage(entry)),
  );
}

async function debugClients() {
  const clientsById = new Map();
  for (const clientId of debugClientIds) {
    const client = await self.clients.get(clientId);
    if (client) {
      clientsById.set(client.id, client);
    } else {
      debugClientIds.delete(clientId);
    }
  }
  const candidates = await self.clients.matchAll({
    type: "window",
    includeUncontrolled: true,
  });
  for (const client of candidates) {
    if (isWebviewPopup(client.url)) {
      debugClientIds.add(client.id);
      clientsById.set(client.id, client);
    }
  }
  return [...clientsById.values()];
}

function isWebviewPopup(url) {
  try {
    const parsed = new URL(url);
    return parsed.hash.startsWith("#webview") || parsed.pathname.startsWith(gatewayPrefix);
  } catch (_error) {
    return false;
  }
}

function requestedTarget(url) {
  const path = new URL(url).pathname;
  const encoded = path.slice(gatewayPrefix.length);
  try {
    return encoded ? decodeURIComponent(encoded) : url;
  } catch (_error) {
    return url;
  }
}

async function gatewayHostClient() {
  const registered = await registeredGatewayHostClient();
  if (registered) {
    return registered;
  }
  const candidateClients = await self.clients.matchAll({
    type: "window",
    includeUncontrolled: true,
  });
  if (candidateClients.length === 0) {
    return undefined;
  }
  const discovered = await Promise.all(
    candidateClients.map(async (client) => ({
      client,
      ready: await queryGatewayHost(client),
    })),
  );
  const host = discovered.find(({ ready }) => ready)?.client;
  if (!host) {
    return undefined;
  }
  updateGatewayHost(host.id);
  return host;
}

async function registeredGatewayHostClient() {
  if (!gatewayHostClientId) {
    return undefined;
  }
  const client = await self.clients.get(gatewayHostClientId);
  if (!client) {
    gatewayHostClientId = null;
  }
  return client;
}

function updateGatewayHost(clientId) {
  gatewayHostClientId = clientId;
}

function queryGatewayHost(client) {
  return new Promise((resolve) => {
    const channel = new MessageChannel();
    const timeout = globalThis.setTimeout(() => {
      channel.port1.close();
      resolve(false);
    }, 500);
    channel.port1.onmessage = (event) => {
      globalThis.clearTimeout(timeout);
      channel.port1.close();
      resolve(Boolean(event.data?.ready));
    };
    client.postMessage(
      { type: "rings-webview-gateway-host-query" },
      [channel.port2],
    );
  });
}

async function serializeRequest(event) {
  const request = event.request;
  const sourceTarget = await sourceTargetForClient(event.clientId);
  const body = request.method === "GET" || request.method === "HEAD"
    ? undefined
    : await request.clone().arrayBuffer();
  return {
    requested: request.url,
    sourceTarget,
    method: request.method,
    credentials: request.credentials,
    headers: [...request.headers]
      .filter(([name]) => name.toLowerCase() !== "x-rings-webview-kind")
      .map(([name, value]) => ({ name, value })),
    body,
    kind: requestKind(request),
  };
}

async function sourceTargetForClient(clientId) {
  if (!clientId) {
    return undefined;
  }
  const client = await self.clients.get(clientId);
  if (!client) {
    return undefined;
  }
  const url = new URL(client.url);
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

function requestKind(request) {
  const taggedKind = request.headers.get("x-rings-webview-kind");
  if (taggedKind === "fetch" || taggedKind === "xhr" || taggedKind === "navigation") {
    return taggedKind;
  }
  if (
    request.mode === "navigate"
    || request.destination === "document"
    || request.destination === "iframe"
  ) {
    return "navigation";
  }
  return "subresource";
}

function requestGatewayResponse(host, request) {
  return new Promise((resolve) => {
    const channel = new MessageChannel();
    const timeout = globalThis.setTimeout(() => {
      channel.port1.close();
      resolve({ ok: false, status: 504, error: "local Rings node gateway timed out" });
    }, requestTimeoutMs);
    channel.port1.onmessage = (event) => {
      globalThis.clearTimeout(timeout);
      channel.port1.close();
      resolve(event.data);
    };
    host.postMessage(
      { type: "rings-webview-gateway-request", request },
      [channel.port2],
    );
  });
}

function gatewayFailure(status, message, summary = undefined, code = undefined) {
  return new Response(gatewayFailureDocument(
    status,
    gatewayFailureSummary(message),
    gatewayFailureReason(status, message, summary),
    code || gatewayFailureCode(status),
  ), {
    status,
    headers: {
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
      "referrer-policy": "no-referrer",
    },
  });
}

function gatewayFailureSummary(message) {
  let text = String(message || "gateway request failed").trim();
  text = text.replace(
    /^gateway transport:\s+gateway transport failed:/,
    "gateway transport failed:",
  );
  text = text.replace(
    /JsValue\(Error: ([^\n)]*)[\s\S]*\)/,
    "Error: $1",
  );
  const firstLine = text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .find(Boolean);
  return firstLine || "gateway request failed";
}

function gatewayFailureReason(status, message, summary) {
  const cleanSummary = String(summary || "").trim();
  if (cleanSummary) return cleanSummary;
  const detail = gatewayFailureSummary(message);
  if (status === 503 && detail.includes("no live onion exit offers service \"https\"")) {
    return "No live HTTPS onion exit is available.";
  }
  if (status === 503 && detail.includes("no live onion exit")) {
    return "No live onion exit is available for this request.";
  }
  if (status === 503 && detail.includes("Start a local Rings node")) {
    return "Local Rings node gateway is unavailable.";
  }
  if (status === 502) return "Gateway transport failed.";
  return "Gateway request failed.";
}

function gatewayFailureCode(status) {
  if (status === 400) return "invalid_webview_request";
  if (status === 403) return "webview_request_rejected";
  if (status === 404) return "controlled_asset_not_found";
  if (status === 502) return "gateway_transport_failed";
  if (status === 503) return "gateway_unavailable";
  return "gateway_request_failed";
}

function gatewayFailureDocument(status, message, reason, code) {
  const detail = escapeHtml(message);
  const summary = escapeHtml(reason);
  const reasonCode = escapeHtml(code);
  const statusText = escapeHtml(status);
  return `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Rings gateway failure ${statusText}</title>
<style>
  body { margin: 0; min-height: 100vh; background: #fffaf0; color: #111827; font: 14px/1.5 Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
  main { min-height: 100vh; box-sizing: border-box; display: grid; place-items: center; padding: 24px; }
  [data-rings-webview-failure] { width: min(760px, 100%); }
  h1 { margin: 0 0 8px; font-size: 20px; line-height: 1.2; }
  p { margin: 0; color: #6b5f50; }
  code { display: inline-block; margin-right: 8px; padding: 2px 6px; border: 1px solid #d9c5a6; border-radius: 4px; background: #fffdf8; color: #374151; font-weight: 800; }
</style>
<body>
<main>
  <section
    data-rings-webview-failure="true"
    data-rings-webview-failure-status="${statusText}"
    data-rings-webview-failure-code="${reasonCode}"
    data-rings-webview-failure-summary="${summary}"
    data-rings-webview-failure-detail="${detail}"
  >
    <h1>Rings gateway failure ${statusText}</h1>
    <p><code>${reasonCode}</code>${summary}</p>
  </section>
</main>
<script src="/assets/webview-overlay.js"></script>
</body>
</html>`;
}

function escapeHtml(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
