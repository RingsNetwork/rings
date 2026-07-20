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
    return gatewayFailure(503, "Start a local Rings node before opening WebView.");
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
    return gatewayFailure(response?.status || 502, response?.error || "gateway request failed");
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
    return gatewayFailure(502, `invalid gateway response: ${String(error)}`);
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

async function emitDebug(scope, message, level = "info", resource = undefined) {
  const entry = {
    type: "rings-webview-debug",
    at: new Date().toISOString(),
    scope,
    message,
    level,
  };
  if (resource) {
    entry.resource = resource;
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

function gatewayFailure(status, message) {
  return new Response(gatewayFailureDocument(status, message), {
    status,
    headers: {
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
      "referrer-policy": "no-referrer",
    },
  });
}

function gatewayFailureDocument(status, message) {
  const detail = JSON.stringify(String(message)).replace(/</g, "\\u003c");
  return `<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Rings gateway failure ${status}</title>
<style>
  body { margin: 0; background: #fffaf0; color: #111827; font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; }
  main { max-width: 900px; margin: 48px auto; padding: 0 20px; }
  h1 { margin: 0 0 12px; font-size: 22px; }
  pre { margin: 0; padding: 14px; border: 1px solid #d9c5a6; border-radius: 5px; background: #fffdf8; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere; }
  #debug { margin-top: 16px; border: 1px solid #d9c5a6; border-radius: 5px; overflow: hidden; }
  #debug summary { padding: 9px 12px; cursor: pointer; font-weight: 700; }
  #events { max-height: 320px; margin: 0; padding: 10px 12px; border-top: 1px solid #d9c5a6; overflow: auto; background: #111827; color: #f8fafc; white-space: pre-wrap; overflow-wrap: anywhere; }
  #events p { margin: 0 0 6px; }
  #events p.error { color: #fca5a5; }
  #controls { position: fixed; right: 16px; bottom: 16px; display: flex; gap: 5px; }
  #controls button { display: grid; width: 36px; height: 36px; padding: 0; place-items: center; border: 1px solid #1d2939; border-radius: 5px; background: #111827; color: #fffaf0; font: 700 16px/1 ui-monospace, SFMono-Regular, Menlo, monospace; cursor: pointer; }
  #controls button:last-child { width: auto; min-width: 68px; padding: 0 10px; font-size: 12px; }
</style>
<main>
  <h1>Rings gateway failure ${status}</h1>
  <pre id="failure"></pre>
  <details id="debug"><summary>Gateway events</summary><div id="events" role="log" aria-live="polite"></div></details>
</main>
<div id="controls" role="toolbar" aria-label="WebView navigation">
  <button id="back" type="button" aria-label="Back" title="Back">&lt;</button>
  <button id="forward" type="button" aria-label="Forward" title="Forward">&gt;</button>
  <button id="reload" type="button" aria-label="Reload" title="Reload">&#x21bb;</button>
  <button id="debug-toggle" type="button" aria-expanded="false">Debug</button>
</div>
<script>
(() => {
  const detail = ${detail};
  const failure = document.getElementById("failure");
  const events = document.getElementById("events");
  const debug = document.getElementById("debug");
  const debugToggle = document.getElementById("debug-toggle");
  failure.textContent = detail;
  function append(entry) {
    const row = document.createElement("p");
    row.className = entry.level === "error" ? "error" : "";
    const resource = entry.resource;
    const progress = resource
      ? " #" + resource.requestId + " " + (resource.status == null ? "pending" : resource.status) + " " + resource.kind + " " + resource.method + " " + resource.phase + " " + resource.target + " " + resource.durationMs + " ms"
      : "";
    row.textContent = "[" + (entry.scope || "worker") + "] " + (entry.message || "unknown event") + progress;
    events.append(row);
    events.scrollTop = events.scrollHeight;
  }
  document.getElementById("back").addEventListener("click", () => history.back());
  document.getElementById("forward").addEventListener("click", () => history.forward());
  document.getElementById("reload").addEventListener("click", () => location.reload());
  debugToggle.addEventListener("click", () => {
    debug.open = !debug.open;
    debugToggle.setAttribute("aria-expanded", String(debug.open));
  });
  navigator.serviceWorker?.addEventListener("message", (event) => {
    if (event.data?.type === "rings-webview-debug") append(event.data);
  });
  navigator.serviceWorker?.ready.then((registration) => {
    const worker = navigator.serviceWorker.controller || registration.active;
    worker?.postMessage({ type: "rings-webview-debug-register" });
  }).catch(() => append({ scope: "overlay", message: "Service Worker debug listener unavailable", level: "error" }));
})();
</script>`;
}
