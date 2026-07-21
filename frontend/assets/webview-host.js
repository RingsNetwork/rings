(() => {
  "use strict";

  const workerUrl = "/rings-webview-service-worker.js?gateway-host-protocol=3";
  let registrationPromise;
  const debugEntries = [];

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
    const worker = navigator.serviceWorker?.controller;
    if (!worker) {
      return;
    }
    worker.postMessage({
      type: "rings-webview-debug-entry",
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
    await postWorkerMessage(worker, { type: "rings-webview-host-register" });
    recordDebug("host", "Registered the local Rings node as gateway host");
  }

  async function enableDebug() {
    const activeRegistration = await ensureReady();
    const worker = navigator.serviceWorker.controller || activeRegistration.active;
    if (!worker) {
      throw new Error("Service Worker has no active controller");
    }
    const acknowledged = await postWorkerMessage(worker, { type: "rings-webview-debug-register" });
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
      event.ports?.[0]?.postMessage({ ready });
      const worker = navigator.serviceWorker.controller;
      if (ready && worker) {
        void postWorkerMessage(worker, { type: "rings-webview-host-register" })
          .then(() => recordDebug("host", "Restored the local Rings node gateway host"));
      }
      return;
    }
    if (message?.type !== "rings-webview-gateway-request") {
      return;
    }
    const port = event.ports?.[0];
    if (!port) {
      return;
    }
    const handler = globalThis.RingsWebviewGateway?.handle;
    if (typeof handler !== "function") {
      recordDebug("host", "Rejected request because the local node gateway is unavailable", "error");
      port.postMessage({
        ok: false,
        status: 503,
        error: "the local Rings node gateway is unavailable",
      });
      return;
    }
    recordDebug("host", `Received ${message.request.kind} ${message.request.method} request`);
    Promise.resolve(handler(message.request))
      .then((response) => {
        if (response?.ok) {
          recordDebug("host", `Returned gateway response ${response.status}`);
        } else {
          recordDebug("host", `Gateway response ${response?.status || 502}: ${response?.error || "unknown error"}`, "error");
        }
        port.postMessage(response);
      })
      .catch((error) => {
        recordDebug("host", `Gateway handler failed: ${String(error)}`, "error");
        port.postMessage({
          ok: false,
          status: 502,
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
