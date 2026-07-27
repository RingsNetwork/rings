(() => {
  "use strict";

  const gatewayPrefix = "/webview/";
  const marker = "__ringsWebviewDebugOverlay";
  const hostId = "rings-webview-debug-overlay";
  const defaultTarget = "https://example.com/";
  const maxEntries = 200;

  const state = {
    entries: [],
    log: undefined,
    network: undefined,
    onion: undefined,
    panel: undefined,
    addressForm: undefined,
    loadingTrack: undefined,
    view: "log",
    levelFilter: "all",
    textFilter: "",
    loading: false,
    originalBodyPaddingTop: undefined,
    originalBodyPaddingBottom: undefined,
    bodySpacingScheduled: false,
  };

  function isWebviewContext() {
    return (
      location.pathname.startsWith(gatewayPrefix)
      || location.hash.startsWith("#webview")
      || Boolean(document.querySelector("[data-rings-webview-default-target],[data-rings-webview-failure]"))
    );
  }

  function initialTargetText() {
    const configured = document.querySelector("[data-rings-webview-default-target]")?.dataset.ringsWebviewDefaultTarget;
    if (configured) return configured;
    if (!location.pathname.startsWith(gatewayPrefix)) return defaultTarget;
    const encoded = location.pathname.slice(gatewayPrefix.length);
    if (!encoded) return defaultTarget;
    try {
      return decodeURIComponent(encoded);
    } catch (_error) {
      return defaultTarget;
    }
  }

  function resourceLine(resource) {
    const status = resource.status == null ? "pending" : String(resource.status);
    return `#${resource.requestId} ${status} ${resource.kind} ${resource.method} ${resource.phase} ${resource.target} ${resource.durationMs} ms`;
  }

  function onionLine(onion) {
    const hops = Array.isArray(onion.hops) ? onion.hops.join(" ") : "";
    return `${onion.target || ""} ${onion.service || ""} ${onion.phase || ""} ${onion.exit || ""} ${hops} ${onion.durationMs || ""}`;
  }

  function entryText(entry) {
    return `${entry.at || ""} ${entry.scope || "worker"} ${entry.level || "info"} ${entry.message || ""} ${entry.resource ? resourceLine(entry.resource) : ""} ${entry.onion ? onionLine(entry.onion) : ""}`.toLowerCase();
  }

  function textMatches(entry) {
    const needle = state.textFilter.trim().toLowerCase();
    return !needle || entryText(entry).includes(needle);
  }

  function levelMatches(entry) {
    return state.levelFilter === "all" || (entry.level || "info") === state.levelFilter;
  }

  function filteredLogEntries() {
    return state.entries.filter((entry) => levelMatches(entry) && textMatches(entry));
  }

  function filteredNetworkEntries() {
    return Array.from(
      new Map(
        state.entries
          .filter((entry) => entry.resource && textMatches(entry))
          .map((entry) => [entry.resource.requestId, entry]),
      ).values(),
    );
  }

  function filteredOnionEntries() {
    return state.entries.filter((entry) => entry.onion && textMatches(entry));
  }

  function appendEmpty(container) {
    const empty = document.createElement("p");
    empty.textContent = state.entries.length === 0 ? "Waiting for gateway activity" : "No matching entries";
    container.append(empty);
  }

  function appendLogRow(container, entry) {
    const row = document.createElement("p");
    row.className = entry.level === "error" ? "error" : entry.level === "warning" ? "warning" : "";
    const time = String(entry.at || "").split("T")[1]?.replace("Z", "").slice(0, 12) || "";
    row.textContent = `${time} ${(entry.level || "info").toUpperCase()} [${entry.scope || "worker"}] ${entry.message || "unknown event"}`;
    container.append(row);
  }

  function appendNetworkHeader(container) {
    const row = document.createElement("div");
    row.className = "network-row heading";
    for (const field of ["ID", "Status", "Kind", "Method", "Phase", "Time", "Target"]) {
      const cell = document.createElement("span");
      cell.textContent = field;
      row.append(cell);
    }
    container.append(row);
  }

  function appendNetworkRow(container, entry) {
    const resource = entry.resource;
    if (!resource) return;
    const row = document.createElement("div");
    row.className = `network-row ${entry.level === "error" ? "error" : entry.level === "warning" ? "warning" : ""}`;
    for (const field of [
      `#${resource.requestId}`,
      resource.status == null ? "pending" : String(resource.status),
      resource.kind,
      resource.method,
      resource.phase,
      `${resource.durationMs} ms`,
      resource.target,
    ]) {
      const cell = document.createElement("span");
      cell.textContent = field;
      row.append(cell);
    }
    container.append(row);
  }

  function compactDid(value) {
    const text = String(value || "");
    return text.length > 18 ? `${text.slice(0, 10)}...${text.slice(-6)}` : text;
  }

  function appendOnionHeader(container) {
    const row = document.createElement("div");
    row.className = "onion-row heading";
    for (const field of ["Phase", "Service", "Hops", "Exit", "Time", "Target"]) {
      const cell = document.createElement("span");
      cell.textContent = field;
      row.append(cell);
    }
    container.append(row);
  }

  function appendOnionRow(container, entry) {
    const onion = entry.onion;
    if (!onion) return;
    const hops = Array.isArray(onion.hops) ? onion.hops : [];
    const row = document.createElement("div");
    row.className = `onion-row ${entry.level === "error" ? "error" : entry.level === "warning" ? "warning" : ""}`;
    for (const field of [
      onion.phase || "route",
      onion.service || "https",
      hops.length ? hops.map(compactDid).join(" -> ") : onion.error || "none",
      compactDid(onion.exit),
      onion.durationMs == null ? "" : `${onion.durationMs} ms`,
      onion.target || "",
    ]) {
      const cell = document.createElement("span");
      cell.textContent = field;
      row.append(cell);
    }
    container.append(row);
  }

  function render() {
    const container = state.view === "network" ? state.network : state.view === "onion" ? state.onion : state.log;
    if (!container) return;
    container.replaceChildren();
    const entries = state.view === "network" ? filteredNetworkEntries() : state.view === "onion" ? filteredOnionEntries() : filteredLogEntries();
    if (entries.length === 0) {
      appendEmpty(container);
      return;
    }
    if (state.view === "network") appendNetworkHeader(container);
    if (state.view === "onion") appendOnionHeader(container);
    for (const entry of entries) {
      if (state.view === "network") appendNetworkRow(container, entry);
      else if (state.view === "onion") appendOnionRow(container, entry);
      else appendLogRow(container, entry);
    }
    container.scrollTop = container.scrollHeight;
  }

  function setLoading(loading) {
    state.loading = Boolean(loading);
    if (!state.addressForm) return;
    state.addressForm.dataset.loading = String(state.loading);
    state.addressForm.setAttribute("aria-busy", String(state.loading));
    if (state.loadingTrack) state.loadingTrack.hidden = !state.loading;
  }

  function syncDocumentLoading() {
    if (document.readyState === "complete") {
      setLoading(false);
      return;
    }
    setLoading(true);
    globalThis.addEventListener("load", () => setLoading(false), { once: true });
    globalThis.addEventListener("pageshow", () => {
      if (document.readyState === "complete") setLoading(false);
    }, { once: true });
  }

  function record(scope, message, level = "info", resource = undefined, onion = undefined, at = undefined) {
    const entry = { at: at || new Date().toISOString(), scope, message, level };
    if (resource) entry.resource = resource;
    if (onion) entry.onion = onion;
    state.entries.push(entry);
    if (state.entries.length > maxEntries) state.entries.splice(0, state.entries.length - maxEntries);
    render();
  }

  function selectView(view) {
    state.view = view;
    state.log.hidden = view !== "log";
    state.network.hidden = view !== "network";
    state.onion.hidden = view !== "onion";
    for (const tab of state.panel.querySelectorAll("[data-view]")) {
      const active = tab.dataset.view === view;
      tab.dataset.active = String(active);
      tab.setAttribute("aria-selected", String(active));
    }
    render();
  }

  function normalizedTarget(input) {
    let text = String(input || "").trim();
    if (!text) throw new Error("Address is empty");
    if (!/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(text)) text = `https://${text}`;
    const url = new URL(text);
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      throw new Error("Only HTTP(S) addresses can be opened");
    }
    return url.href;
  }

  function gatewayPath(input) {
    return `${gatewayPrefix}${encodeURIComponent(normalizedTarget(input))}`;
  }

  function limitedText(value) {
    const text = String(value);
    return text.length > 1200 ? `${text.slice(0, 1200)}...` : text;
  }

  function formatConsoleValue(value) {
    if (value instanceof Error) return limitedText(value.stack || value.message);
    if (typeof value === "string") return limitedText(value);
    try {
      const json = JSON.stringify(value);
      if (json !== undefined) return limitedText(json);
    } catch (_error) {}
    return limitedText(value);
  }

  function installConsoleCapture() {
    const captureMarker = "__ringsWebviewConsoleCapture";
    if (globalThis[captureMarker]) return;
    globalThis[captureMarker] = true;
    for (const method of ["error", "warn", "info", "log"]) {
      const original = console[method]?.bind(console);
      if (!original) continue;
      console[method] = (...args) => {
        const level = method === "error" ? "error" : method === "warn" ? "warning" : "info";
        try {
          record("console", args.map(formatConsoleValue).join(" "), level);
        } catch (_error) {}
        original(...args);
      };
    }
    globalThis.addEventListener("error", (event) => {
      record("page", event.error?.stack || event.message || "Uncaught page error", "error");
    });
    globalThis.addEventListener("unhandledrejection", (event) => {
      record("page", `Unhandled rejection: ${formatConsoleValue(event.reason)}`, "error");
    });
  }

  function applyBodySpacing() {
    if (!document.body) {
      scheduleBodySpacing();
      return;
    }
    if (state.originalBodyPaddingTop === undefined) {
      state.originalBodyPaddingTop = getComputedStyle(document.body).paddingTop || "0px";
      state.originalBodyPaddingBottom = getComputedStyle(document.body).paddingBottom || "0px";
      document.body.dataset.ringsWebviewToolbarReserved = "true";
      document.body.style.paddingTop = `calc(${state.originalBodyPaddingTop} + 46px)`;
      document.documentElement.style.scrollPaddingTop = "52px";
    }
    if (state.panel && !state.panel.hidden) {
      document.body.style.paddingBottom = `calc(${state.originalBodyPaddingBottom} + min(300px, 45vh))`;
    } else {
      document.body.style.paddingBottom = state.originalBodyPaddingBottom;
    }
  }

  function scheduleBodySpacing() {
    if (state.bodySpacingScheduled || document.body) return;
    state.bodySpacingScheduled = true;
    const retry = () => {
      state.bodySpacingScheduled = false;
      applyBodySpacing();
    };
    if (typeof MutationObserver === "function" && document.documentElement) {
      const observer = new MutationObserver(() => {
        if (!document.body) return;
        observer.disconnect();
        retry();
      });
      observer.observe(document.documentElement, { childList: true });
      document.addEventListener("DOMContentLoaded", () => {
        observer.disconnect();
        retry();
      }, { once: true });
      return;
    }
    document.addEventListener("DOMContentLoaded", retry, { once: true });
  }

  async function prepareGatewayForNavigation() {
    const host = globalThis.RingsWebviewHost;
    if (!host || typeof host.ensureReady !== "function") return;
    await host.ensureReady();
    if (typeof host.enableDebug === "function") {
      await host.enableDebug();
    }
  }

  async function openAddress(address) {
    let path;
    try {
      path = gatewayPath(address);
    } catch (error) {
      record("overlay", String(error), "error");
      return;
    }
    record("overlay", "Connecting through the local Rings gateway");
    setLoading(true);
    try {
      await prepareGatewayForNavigation();
      record("overlay", "Request sent to the local Rings gateway");
      location.href = path;
    } catch (error) {
      setLoading(false);
      record("overlay", `gateway unavailable: ${String(error)}`, "error");
    }
  }

  function requestDebugRegistration() {
    const host = globalThis.RingsWebviewHost;
    if (host && typeof host.enableDebug === "function") {
      void host.enableDebug().catch((error) => record("overlay", `Debug listener unavailable: ${String(error)}`, "error"));
      return;
    }
    void navigator.serviceWorker?.ready
      .then((registration) => {
        const worker = navigator.serviceWorker.controller || registration.active;
        worker?.postMessage({ type: "rings-webview-debug-register" });
      })
      .catch(() => record("overlay", "Service Worker debug listener unavailable", "error"));
  }

  function isGatewayResponseDocument() {
    return Boolean(
      document.querySelector("[data-rings-webview-failure]")
      || globalThis.__ringsWebviewGateway,
    );
  }

  function maybeReloadColdGatewayFallback() {
    if (!location.pathname.startsWith(gatewayPrefix) || isGatewayResponseDocument()) return;
    const host = globalThis.RingsWebviewHost;
    if (!host || typeof host.ensureReady !== "function") return;
    const storageKey = `rings-webview-cold-reload:${location.href}`;
    try {
      if (sessionStorage.getItem(storageKey) === "true") {
        record("overlay", "Gateway path loaded without a Service Worker response after retry", "warning");
        return;
      }
      sessionStorage.setItem(storageKey, "true");
    } catch (_error) {}
    void host.ensureReady()
      .then(() => {
        if (isGatewayResponseDocument()) return;
        record("overlay", "Reloading gateway path after Service Worker took control");
        location.reload();
      })
      .catch((error) => record("overlay", `Service Worker controller unavailable: ${String(error)}`, "error"));
  }

  function setDebugOpen(open) {
    if (!state.panel) return;
    const toggle = state.panel.getRootNode().getElementById("toggle");
    state.panel.hidden = !open;
    toggle?.setAttribute("aria-expanded", String(open));
    if (toggle) toggle.dataset.active = String(open);
    applyBodySpacing();
    if (open) render();
  }

  function appendFailureEntry() {
    const failure = document.querySelector("[data-rings-webview-failure]");
    if (!failure || failure.dataset.ringsWebviewFailureRecorded === "true") return;
    failure.dataset.ringsWebviewFailureRecorded = "true";
    const status = Number(failure.dataset.ringsWebviewFailureStatus || 0) || undefined;
    const code = failure.dataset.ringsWebviewFailureCode || "gateway_request_failed";
    const summary = failure.dataset.ringsWebviewFailureSummary || "Gateway request failed.";
    const detail = failure.dataset.ringsWebviewFailureDetail || summary;
    record(
      "gateway",
      `${code}: ${detail}`,
      "error",
      {
        requestId: "failure",
        target: initialTargetText(),
        method: "GET",
        kind: "navigation",
        phase: "failed",
        durationMs: 0,
        status,
      },
    );
    setDebugOpen(true);
  }

  function mount() {
    if (!isWebviewContext()) return;
    if (document.getElementById(hostId)) {
      appendFailureEntry();
      return;
    }
    const host = document.createElement("div");
    host.id = hostId;
    const root = host.attachShadow({ mode: "open" });
    root.innerHTML = `
      <style>
        :host { all: initial; }
        #toolbar { position: fixed; inset: 0 0 auto 0; z-index: 2147483647; display: grid; min-width: 0; min-height: 46px; box-sizing: border-box; grid-template-columns: auto minmax(0, 1fr) auto; gap: 7px; align-items: center; padding: 6px 8px; border-bottom: 1px solid #ddccb0; background: #fffaf0; color: #111827; box-shadow: 0 1px 4px rgba(17, 24, 39, 0.16); font: 12px/1 Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
        #controls { display: flex; min-width: 0; gap: 5px; }
        #address-form { position: relative; display: grid; min-width: 0; grid-template-columns: minmax(0, 1fr) auto; gap: 6px; }
        #address { grid-column: 1; grid-row: 1; width: 100%; min-width: 0; height: 32px; box-sizing: border-box; padding: 5px 9px; border: 1px solid #d9c5a6; border-radius: 5px; background: #fffdf8; color: #111827; font: 12px/1.2 ui-monospace, SFMono-Regular, Menlo, monospace; }
        #address:focus { border-color: #b42318; outline: 3px solid rgba(180, 35, 24, 0.12); }
        #address-form[data-loading=true] #address { border-color: #8ab4f8; box-shadow: inset 0 -2px 0 rgba(26, 115, 232, 0.18); }
        #loading-track { position: relative; z-index: 2; grid-column: 1; grid-row: 1; align-self: end; height: 2px; margin: 0 2px 2px; overflow: hidden; border-radius: 999px; pointer-events: none; }
        #loading-track[hidden] { display: none; }
        #loading-bar { position: absolute; inset: 0 auto 0 0; width: 42%; border-radius: inherit; background: linear-gradient(90deg, #1a73e8, #34a853); transform: translateX(-120%); animation: rings-webview-loading 1.15s cubic-bezier(.4, 0, .2, 1) infinite; }
        @keyframes rings-webview-loading { 0% { transform: translateX(-120%) scaleX(.65); } 45% { transform: translateX(72%) scaleX(1); } 100% { transform: translateX(250%) scaleX(.75); } }
        #toolbar button { display: grid; min-width: 34px; height: 32px; min-height: 32px; box-sizing: border-box; place-items: center; padding: 0 9px; border: 1px solid #d9c5a6; border-radius: 5px; background: #fffaf0; color: #374151; font: 800 12px/1 Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; cursor: pointer; }
        #toolbar button:hover, #toolbar button[data-active=true] { border-color: #111827; background: #111827; color: #fff; }
        #go { grid-column: 2; grid-row: 1; min-width: 38px; }
        #toggle { min-width: 34px; padding: 0; }
        #panel { position: fixed; inset: auto 0 0 0; z-index: 2147483647; display: grid; width: 100vw; height: 300px; max-height: 45vh; box-sizing: border-box; grid-template-rows: auto minmax(0, 1fr); border-top: 1px solid #dadce0; background: #fff; color: #202124; box-shadow: 0 -1px 3px rgba(60, 64, 67, 0.22); overflow: hidden; font: 12px/1.35 ui-monospace, SFMono-Regular, Menlo, monospace; }
        #panel[hidden] { display: none; }
        #bar { display: flex; flex-wrap: wrap; align-items: center; gap: 7px; min-height: 34px; padding: 0 8px; border-bottom: 1px solid #dadce0; background: #f1f3f4; font: 12px/1.35 Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
        #tabs { display: flex; align-self: stretch; gap: 0; }
        #filters { display: flex; min-width: min(520px, 100%); flex: 1 1 360px; gap: 6px; align-items: center; }
        #bar button, #bar select, #bar input { min-height: 24px; box-sizing: border-box; border: 1px solid #dadce0; border-radius: 3px; background: #fff; color: #3c4043; font: inherit; font-weight: 500; }
        #bar button { padding: 3px 8px; cursor: pointer; }
        #tabs button { min-height: 34px; border-width: 0 0 2px; border-color: transparent; border-radius: 0; background: transparent; color: #5f6368; }
        select { width: 108px; padding: 2px 6px; }
        input { min-width: 120px; flex: 1 1 220px; padding: 3px 8px; }
        input:focus, select:focus { border-color: #1a73e8; outline: 1px solid #1a73e8; }
        #tabs button[data-active=true] { border-bottom-color: #1a73e8; color: #202124; background: transparent; }
        #bar button:hover, #bar button[data-active=true]:not([role=tab]) { background: #e8f0fe; color: #202124; }
        #clear { margin-left: auto; }
        .content { min-height: 0; margin: 0; padding: 0 8px 8px; overflow: auto; background: #fff; color: #202124; }
        p { margin: 0 0 5px; overflow-wrap: anywhere; white-space: pre-wrap; }
        p.error { color: #b42318; }
        p.warning { color: #8a4b00; }
        .network-row, .onion-row { display: grid; min-width: 760px; gap: 8px; padding: 4px 0; border-bottom: 1px solid #edf0f2; color: #3c4043; }
        .network-row { grid-template-columns: 72px 66px 88px 72px 90px 78px minmax(260px, 1fr); }
        .onion-row { grid-template-columns: 84px 76px minmax(250px, 1fr) 126px 76px minmax(260px, 1fr); }
        .network-row.error, .onion-row.error { color: #b42318; }
        .network-row.warning, .onion-row.warning { color: #8a4b00; }
        .network-row.heading, .onion-row.heading { position: sticky; top: 0; z-index: 1; background: #fff; color: #5f6368; font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; font-weight: 700; }
        .toolbar-icon { display: block; width: 16px; height: 16px; fill: none; stroke: currentColor; stroke-width: 2; stroke-linecap: round; stroke-linejoin: round; }
        @media (max-width: 640px) {
          #toolbar { grid-template-columns: auto minmax(0, 1fr); grid-template-areas: "controls toggle" "address address"; }
          #controls { grid-area: controls; }
          #address-form { grid-area: address; }
          #toggle { grid-area: toggle; justify-self: end; }
        }
      </style>
      <header id="toolbar" role="toolbar" aria-label="WebView navigation">
        <div id="controls">
          <button id="back" type="button" aria-label="Back" title="Back">&lt;</button>
          <button id="forward" type="button" aria-label="Forward" title="Forward">&gt;</button>
          <button id="reload" type="button" aria-label="Reload" title="Reload">
            <svg class="toolbar-icon" aria-hidden="true" viewBox="0 0 24 24"><path d="M3 12a9 9 0 0 1 15.5-6.2L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-15.5 6.2L3 16"/><path d="M3 21v-5h5"/></svg>
          </button>
        </div>
        <form id="address-form">
          <input id="address" aria-label="Address" autocomplete="off" spellcheck="false" />
          <span id="loading-track" aria-hidden="true" hidden><span id="loading-bar"></span></span>
          <button id="go" type="submit" aria-label="Open address" title="Open address">&gt;</button>
        </form>
        <button id="toggle" type="button" aria-label="Debug" title="Debug" aria-expanded="false">
          <svg class="toolbar-icon" aria-hidden="true" viewBox="0 0 24 24"><path d="m8 2 1.88 1.88"/><path d="M14.12 3.88 16 2"/><path d="M9 7.13v-1a3 3 0 0 1 6 0v1"/><path d="M12 20c-3.3 0-6-2.7-6-6v-3a4 4 0 0 1 4-4h4a4 4 0 0 1 4 4v3c0 3.3-2.7 6-6 6Z"/><path d="M12 20v-9"/><path d="M6 13H2"/><path d="M22 13h-4"/><path d="M3 21c0-2.1 1.6-3.8 3.53-4"/><path d="M21 21c0-2.1-1.6-3.8-3.53-4"/></svg>
        </button>
      </header>
      <section id="panel" aria-label="Rings WebView gateway debug console" hidden>
        <div id="bar">
          <div id="tabs" role="tablist" aria-label="Debug views">
            <button type="button" role="tab" data-view="log" data-active="true" aria-selected="true">Logs</button>
            <button type="button" role="tab" data-view="network" data-active="false" aria-selected="false">Network</button>
            <button type="button" role="tab" data-view="onion" data-active="false" aria-selected="false">Onion</button>
          </div>
          <div id="filters">
            <select id="level-filter" aria-label="Log level filter">
              <option value="all">All levels</option>
              <option value="error">Errors</option>
              <option value="warning">Warnings</option>
              <option value="info">Info</option>
            </select>
            <input id="text-filter" type="search" placeholder="Filter" aria-label="Filter debug entries" />
          </div>
          <button id="clear" type="button">Clear</button>
        </div>
        <div id="log" class="content" role="log" aria-live="polite"></div>
        <div id="network" class="content" role="table" hidden></div>
        <div id="onion" class="content" role="table" hidden></div>
      </section>`;
    const panel = root.getElementById("panel");
    const toggle = root.getElementById("toggle");
    const back = root.getElementById("back");
    const forward = root.getElementById("forward");
    const reload = root.getElementById("reload");
    const addressForm = root.getElementById("address-form");
    const address = root.getElementById("address");
    const loadingTrack = root.getElementById("loading-track");
    const log = root.getElementById("log");
    const network = root.getElementById("network");
    const onion = root.getElementById("onion");
    const levelFilter = root.getElementById("level-filter");
    const textFilter = root.getElementById("text-filter");
    const clear = root.getElementById("clear");
    if (!panel || !toggle || !back || !forward || !reload || !addressForm || !address || !loadingTrack || !log || !network || !onion || !levelFilter || !textFilter || !clear) return;
    state.panel = panel;
    state.addressForm = addressForm;
    state.loadingTrack = loadingTrack;
    state.log = log;
    state.network = network;
    state.onion = onion;
    address.value = initialTargetText();
    toggle.addEventListener("click", () => setDebugOpen(panel.hidden));
    back.addEventListener("click", () => {
      setLoading(true);
      history.back();
    });
    forward.addEventListener("click", () => {
      setLoading(true);
      history.forward();
    });
    reload.addEventListener("click", () => {
      setLoading(true);
      location.reload();
    });
    addressForm.addEventListener("submit", (event) => {
      event.preventDefault();
      void openAddress(address.value);
    });
    for (const tab of root.querySelectorAll("[data-view]")) {
      tab.addEventListener("click", () => selectView(tab.dataset.view));
    }
    levelFilter.addEventListener("change", () => {
      state.levelFilter = levelFilter.value || "all";
      render();
    });
    textFilter.addEventListener("input", () => {
      state.textFilter = textFilter.value || "";
      render();
    });
    clear.addEventListener("click", () => {
      state.entries.splice(0, state.entries.length);
      render();
    });
    document.documentElement.append(host);
    applyBodySpacing();
    installConsoleCapture();
    requestDebugRegistration();
    syncDocumentLoading();
    record("overlay", "Debug panel ready");
    appendFailureEntry();
    maybeReloadColdGatewayFallback();
  }

  function onContextMaybeChanged() {
    if (!isWebviewContext()) return;
    if (document.documentElement) {
      mount();
      return;
    }
    document.addEventListener("DOMContentLoaded", mount, { once: true });
  }

  globalThis[marker] = {
    installed: true,
    mount: onContextMaybeChanged,
    mounted: () => Boolean(document.getElementById(hostId)),
    record,
  };

  navigator.serviceWorker?.addEventListener("message", (event) => {
    const entry = event.data;
    if (entry?.type === "rings-webview-debug") {
      record(
        entry.scope || "worker",
        entry.message || "unknown event",
        entry.level || "info",
        entry.resource,
        entry.onion,
        entry.at,
      );
    }
  });
  globalThis.addEventListener("hashchange", onContextMaybeChanged);
  onContextMaybeChanged();
})();
