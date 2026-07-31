(() => {
  const gatewayPrefix = "/webview/";
  const workerUrl = "/rings-webview-service-worker.js?gateway-host-protocol=5";
  const debugCapabilityRequestType = "rings-webview-debug-capability-request";
  const debugCapabilityResponseType = "rings-webview-debug-capability-response";
  const gatewayHostCapability = createGatewayHostCapability();
  let ownsGatewayHost = false;
  let registrationPromise;
  let popupDebugCapabilityPromise;
  const debugEntries = [];
  const pendingGatewayRequests = new Map();

  function createGatewayHostCapability() {
    if (typeof globalThis.crypto?.getRandomValues !== "function") {
      return "";
    }
    const values = new Uint8Array(32);
    globalThis.crypto.getRandomValues(values);
    return Array.from(values, (value) => value.toString(16).padStart(2, "0")).join("");
  }

  function recordDebug(scope, message, level = "info", resource = undefined, broadcast = true, onion = undefined) {
    const entry = {
      at: new Date().toISOString(),
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
    debugEntries.push(entry);
    if (debugEntries.length > 200) {
      debugEntries.splice(0, debugEntries.length - 200);
    }
    if (broadcast) {
      broadcastDebugEntry(entry);
    }
  }

  function broadcastDebugEntry(entry) {
    if (!ownsGatewayHost) {
      return;
    }
    const worker = navigator.serviceWorker?.controller;
    if (!worker) {
      return;
    }
    worker.postMessage({
      type: "rings-webview-debug-entry",
      capability: gatewayHostCapability,
      entry,
    });
  }

  function ensureServiceWorkerSupport() {
    if (!navigator.serviceWorker) {
      throw new Error("Service Worker is unavailable in this browser context");
    }
  }

  function waitForController() {
    if (navigator.serviceWorker.controller) {
      return Promise.resolve();
    }
    return new Promise((resolve, reject) => {
      const timeout = globalThis.setTimeout(() => {
        navigator.serviceWorker.removeEventListener("controllerchange", onChange);
        reject(new Error("Service Worker did not take control of this page"));
      }, 5_000);
      function onChange() {
        globalThis.clearTimeout(timeout);
        navigator.serviceWorker.removeEventListener("controllerchange", onChange);
        resolve();
      }
      navigator.serviceWorker.addEventListener("controllerchange", onChange, { once: true });
    });
  }

  async function registration() {
    ensureServiceWorkerSupport();
    if (!registrationPromise) {
      registrationPromise = navigator.serviceWorker
        .register(workerUrl, { scope: "/" })
        .then(async (activeRegistration) => {
          await activeRegistration.update();
          return activeRegistration;
        });
    }
    return registrationPromise;
  }

  function currentOrigin() {
    return globalThis.location?.origin || "";
  }

  function sameOriginUrl(value) {
    try {
      const parsed = new URL(value);
      return parsed.origin === currentOrigin() ? parsed : undefined;
    } catch (_error) {
      return undefined;
    }
  }

  function isTrustedWebviewShellUrl(value) {
    const parsed = sameOriginUrl(value);
    return Boolean(parsed && !parsed.pathname.startsWith(gatewayPrefix) && parsed.hash.startsWith("#webview"));
  }

  function isWebviewShell() {
    return isTrustedWebviewShellUrl(globalThis.location?.href || "");
  }

  function isTrustedWebviewShellWindow(source) {
    if (!source || source === globalThis) {
      return false;
    }
    try {
      return isTrustedWebviewShellUrl(source.location.href);
    } catch (_error) {
      return false;
    }
  }

  function clearPopupOpener() {
    if (!isWebviewShell() || !globalThis.opener) {
      return;
    }
    try {
      globalThis.opener = null;
    } catch (_error) {}
  }

  function requestDebugCapabilityFromOpener() {
    if (!isWebviewShell() || !globalThis.opener || typeof MessageChannel !== "function") {
      return Promise.resolve(undefined);
    }
    if (popupDebugCapabilityPromise) {
      return popupDebugCapabilityPromise;
    }
    popupDebugCapabilityPromise = new Promise((resolve) => {
      const channel = new MessageChannel();
      const finish = (capability) => {
        globalThis.clearTimeout(timeout);
        channel.port1.close();
        clearPopupOpener();
        resolve(capability);
      };
      const timeout = globalThis.setTimeout(() => finish(undefined), 500);
      channel.port1.onmessage = (event) => {
        const capability = event.data?.capability;
        if (
          event.data?.type === debugCapabilityResponseType &&
          typeof capability === "string" &&
          capability.length >= 32
        ) {
          finish(capability);
          return;
        }
        finish(undefined);
      };
      try {
        globalThis.opener.postMessage({ type: debugCapabilityRequestType }, currentOrigin(), [channel.port2]);
      } catch (_error) {
        finish(undefined);
      }
    });
    return popupDebugCapabilityPromise;
  }

  async function debugCapability() {
    return (await requestDebugCapabilityFromOpener()) || gatewayHostCapability;
  }

  async function ensureReady() {
    const activeRegistration = await registration();
    await navigator.serviceWorker.ready;
    await waitForController();
    recordDebug("popup", "Service Worker controls this popup");
    return activeRegistration;
  }

  async function registerGatewayHost() {
    const activeRegistration = await ensureReady();
    const worker = navigator.serviceWorker.controller || activeRegistration.active;
    if (!worker) {
      throw new Error("Service Worker has no active controller");
    }
    const acknowledged = await postWorkerMessage(worker, {
      type: "rings-webview-host-register",
      capability: gatewayHostCapability,
    });
    ownsGatewayHost = Boolean(acknowledged);
    if (acknowledged) {
      recordDebug("host", "Registered the local Rings node as gateway host");
      return true;
    }
    recordDebug("host", "Service Worker rejected gateway host registration", "warning");
    throw new Error("Service Worker rejected gateway host registration");
  }

  async function enableDebug() {
    const activeRegistration = await ensureReady();
    const worker = navigator.serviceWorker.controller || activeRegistration.active;
    if (!worker) {
      throw new Error("Service Worker has no active controller");
    }
    const capability = await debugCapability();
    const acknowledged = await postWorkerMessage(worker, {
      type: "rings-webview-debug-register",
      capability,
    });
    if (!acknowledged) {
      recordDebug("popup", "Service Worker did not acknowledge debug registration; continuing");
    }
    recordDebug("popup", "Registered popup debug listener");
  }

  function postWorkerMessage(worker, message) {
    return new Promise((resolve) => {
      const channel = new MessageChannel();
      const timeout = globalThis.setTimeout(() => {
        channel.port1.close();
        resolve(false);
      }, 500);
      channel.port1.onmessage = (event) => {
        globalThis.clearTimeout(timeout);
        channel.port1.close();
        resolve(Boolean(event.data?.ok));
      };
      worker.postMessage(message, [channel.port2]);
    });
  }

  function takeDebugEntries() {
    return debugEntries.splice(0, debugEntries.length);
  }

  function clearDebugEntries() {
    debugEntries.splice(0, debugEntries.length);
  }

  globalThis.addEventListener("message", (event) => {
    const message = event.data;
    if (message?.type !== debugCapabilityRequestType) {
      return;
    }
    const reply = event.ports?.[0];
    if (event.origin !== currentOrigin() || !reply || !isTrustedWebviewShellWindow(event.source)) {
      reply?.postMessage({ type: debugCapabilityResponseType, ok: false });
      return;
    }
    reply.postMessage({
      type: debugCapabilityResponseType,
      ok: true,
      capability: gatewayHostCapability,
    });
  });

  navigator.serviceWorker?.addEventListener("message", (event) => {
    const message = event.data;
    if (message?.type === "rings-webview-debug") {
      recordDebug(
        message.scope || "worker",
        message.message || "unknown event",
        message.level || "info",
        message.resource,
        false,
        message.onion,
      );
      return;
    }
    if (message?.type === "rings-webview-gateway-host-query") {
      const ready = typeof globalThis.RingsWebviewGateway?.handle === "function";
      event.ports?.[0]?.postMessage({ ready, capability: gatewayHostCapability });
      const worker = navigator.serviceWorker.controller;
      if (ready && worker) {
        void postWorkerMessage(worker, {
          type: "rings-webview-host-register",
          capability: gatewayHostCapability,
        }).then((acknowledged) => {
          ownsGatewayHost = Boolean(acknowledged);
          if (acknowledged) {
            recordDebug("host", "Restored the local Rings node gateway host");
          } else {
            recordDebug("host", "Service Worker rejected restored gateway host registration", "warning");
          }
        });
      }
      return;
    }
    if (message?.type === "rings-webview-gateway-cancel") {
      const requestId = message.requestId;
      if (!Number.isSafeInteger(requestId) || requestId <= 0) {
        return;
      }
      if (!pendingGatewayRequests.delete(requestId)) {
        return;
      }
      globalThis.RingsWebviewGateway?.cancel?.(requestId);
      recordDebug("host", `Cancelled timed out gateway request #${requestId}`, "warning");
      return;
    }
    if (message?.type !== "rings-webview-gateway-request") {
      return;
    }
    const port = event.ports?.[0];
    if (!port) {
      return;
    }
    const requestId = message.requestId;
    if (!Number.isSafeInteger(requestId) || requestId <= 0) {
      port.postMessage({
        ok: false,
        status: 400,
        errorCode: "invalid_gateway_request",
        errorSummary: "Malformed Rings WebView gateway request.",
        error: "gateway request id is invalid",
      });
      return;
    }
    const handler = globalThis.RingsWebviewGateway?.handle;
    if (typeof handler !== "function") {
      recordDebug("host", "Rejected request because the local node gateway is unavailable", "error");
      port.postMessage({
        ok: false,
        status: 503,
        errorCode: "local_gateway_unavailable",
        errorSummary: "Local Rings node gateway is unavailable.",
        error: "the local Rings node gateway is unavailable",
      });
      return;
    }
    const previousPort = pendingGatewayRequests.get(requestId);
    if (previousPort) {
      pendingGatewayRequests.delete(requestId);
      globalThis.RingsWebviewGateway?.cancel?.(requestId);
    }
    pendingGatewayRequests.set(requestId, port);
    const takePendingPort = () => {
      if (pendingGatewayRequests.get(requestId) !== port) {
        return false;
      }
      pendingGatewayRequests.delete(requestId);
      return true;
    };
    recordDebug("host", `Received #${requestId} ${message.request.kind} ${message.request.method} request`);
    Promise.resolve()
      .then(() => handler(message.request, requestId))
      .then((response) => {
        if (!takePendingPort()) {
          return;
        }
        if (response?.ok) {
          recordDebug("host", `Returned #${requestId} gateway response ${response.status}`);
        } else {
          recordDebug(
            "host",
            `Gateway response #${requestId} ${response?.status || 502}: ${response?.error || "unknown error"}`,
            "error",
          );
        }
        port.postMessage(response);
      })
      .catch((error) => {
        if (!takePendingPort()) {
          return;
        }
        recordDebug("host", `Gateway handler failed: ${String(error)}`, "error");
        port.postMessage({
          ok: false,
          status: 502,
          errorCode: "gateway_transport_failed",
          errorSummary: "Gateway transport failed.",
          error: String(error),
        });
      });
  });

  globalThis.RingsWebviewHost = Object.freeze({
    ensureReady,
    registerGatewayHost,
    enableDebug,
    recordDebugEntry: recordDebug,
    takeDebugEntries,
    clearDebugEntries,
  });
})();
