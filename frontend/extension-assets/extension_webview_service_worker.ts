/**
 * MV3 service-worker adapter for isolated Extension WebView windows.
 */

import { createSerializedEffectQueue, errorMessage, type RuntimeResponse } from "./extension_runtime.js";

/** Minimal runtime message surface narrowed at the adapter boundary. */
type RuntimeMessage = {
  readonly type?: unknown;
};

const WEBVIEW_OPEN = "rings.webview.open";
const WEBVIEW_ACTIVATE = "rings.webview.activate";
const WEBVIEW_DOCUMENT = "webview.html";
const WEBVIEW_RENDERER_DOCUMENT = "webview_frame.html";
const WEBVIEW_NETWORK_FILTER = "^https?://";
const MAX_TRACKED_NESTED_RENDERERS_PER_TAB = 4_096;
const NESTED_RENDERER_STORAGE_PREFIX = "rings.webview.nestedRenderers.";
const WEBVIEW_BLOCKED_RESOURCE_TYPES = [
  "main_frame",
  "sub_frame",
  "stylesheet",
  "script",
  "image",
  "font",
  "object",
  "xmlhttprequest",
  "ping",
  "csp_report",
  "media",
  "websocket",
  "webtransport",
  "webbundle",
  "other",
] as const;
const webviewRuleEffects = createSerializedEffectQueue();
const rendererRegistryEffects = createSerializedEffectQueue();
const nestedRendererSessions = new Map<number, Map<number, string>>();

/** Registers the finite WebView runtime, navigation, and cleanup effects. */
export function installExtensionWebviewServiceWorker(): void {
  chrome.runtime.onMessage.addListener(handleWebviewMessage);
  chrome.tabs.onRemoved.addListener((tabId: number): void => {
    nestedRendererSessions.delete(tabId);
    void rendererRegistryEffects
      .enqueue((): Promise<void> => chrome.storage.session.remove(rendererRegistryKey(tabId)))
      .catch((error: unknown): void => {
        console.warn("Failed to remove Rings WebView renderer registry", error);
      });
    void removeWebviewNetworkBlock(tabId).catch((error: unknown): void => {
      console.warn("Failed to remove Rings WebView network block", error);
    });
  });
  chrome.webNavigation.onBeforeNavigate.addListener(
    (details: chrome.webNavigation.WebNavigationParentedCallbackDetails): void => {
      void forwardWebviewFrameNavigation(details).catch((error: unknown): void => {
        console.warn("Failed to forward Rings WebView navigation", error);
      });
    },
  );
  chrome.webNavigation.onCommitted.addListener(recordCommittedRendererSession);
  void pruneStaleWebviewNetworkBlocks().catch((error: unknown): void => {
    console.warn("Failed to prune stale Rings WebView network blocks", error);
  });
}

/** Owns only the two messages in the Extension WebView protocol. */
function handleWebviewMessage(
  message: unknown,
  sender: chrome.runtime.MessageSender,
  sendResponse: (response?: RuntimeResponse<unknown>) => void,
): boolean {
  if (isMessage(message, WEBVIEW_OPEN)) {
    openWebviewWindow()
      .then((result: unknown): void => sendResponse({ ok: true, result }))
      .catch((error: unknown): void => sendResponse({ ok: false, error: errorMessage(error) }));
    return true;
  }
  if (isMessage(message, WEBVIEW_ACTIVATE)) {
    activateWebviewSender(sender)
      .then((tabId: number): void => sendResponse({ ok: true, result: { tabId } }))
      .catch((error: unknown): void => sendResponse({ ok: false, error: errorMessage(error) }));
    return true;
  }
  return false;
}

/** Forwards a blocked nested navigation only to the packaged host that owns its tab. */
async function forwardWebviewFrameNavigation(
  details: chrome.webNavigation.WebNavigationParentedCallbackDetails,
): Promise<void> {
  if (details.frameId === 0 || !isHttpUrl(details.url) || !(await hasWebviewNetworkBlock(details.tabId))) return;
  if (details.parentFrameId === 0) {
    sendWebviewNavigation(details.tabId, details.url);
    return;
  }
  const sessionId = await nestedRendererSession(details.tabId, details.frameId);
  if (sessionId) {
    sendWebviewNavigation(details.tabId, details.url, sessionId);
  }
}

/** Records one committed recursive renderer with an explicit per-tab memory bound. */
function recordCommittedRendererSession(details: chrome.webNavigation.WebNavigationFramedCallbackDetails): void {
  if (details.frameId === 0) return;
  const sessionId = nestedSessionFromRendererUrl(details.url);
  if (!sessionId) {
    forgetNestedRendererSession(details.tabId, details.frameId);
    return;
  }
  const sessions = nestedRendererSessions.get(details.tabId) ?? new Map<number, string>();
  sessions.delete(details.frameId);
  sessions.set(details.frameId, sessionId);
  while (sessions.size > MAX_TRACKED_NESTED_RENDERERS_PER_TAB) {
    const oldestFrameId = sessions.keys().next().value;
    if (typeof oldestFrameId !== "number") break;
    sessions.delete(oldestFrameId);
  }
  nestedRendererSessions.set(details.tabId, sessions);
  persistNestedRendererSessions(details.tabId);
}

/** Uses the committed-frame registry, restoring it after an MV3 service-worker restart. */
async function nestedRendererSession(tabId: number, frameId: number): Promise<string | undefined> {
  await restoreNestedRendererSessions(tabId);
  const cached = nestedRendererSessions.get(tabId)?.get(frameId);
  if (cached) return cached;
  const frame = await chrome.webNavigation.getFrame({ tabId, frameId });
  const recovered = frame ? nestedSessionFromRendererUrl(frame.url) : undefined;
  if (recovered) {
    const sessions = nestedRendererSessions.get(tabId) ?? new Map<number, string>();
    sessions.set(frameId, recovered);
    nestedRendererSessions.set(tabId, sessions);
    persistNestedRendererSessions(tabId);
  }
  return recovered;
}

/** Releases one frame-session entry after navigation or a non-renderer commit. */
function forgetNestedRendererSession(tabId: number, frameId: number): void {
  const sessions = nestedRendererSessions.get(tabId);
  sessions?.delete(frameId);
  if (sessions?.size === 0) nestedRendererSessions.delete(tabId);
  persistNestedRendererSessions(tabId);
}

/** Restores a validated, bounded renderer registry exactly once per live worker. */
async function restoreNestedRendererSessions(tabId: number): Promise<void> {
  if (nestedRendererSessions.has(tabId)) return;
  const key = rendererRegistryKey(tabId);
  const stored = await chrome.storage.session.get(key);
  if (nestedRendererSessions.has(tabId)) return;
  const value = stored[key];
  if (typeof value !== "object" || value === null) return;
  const sessions = new Map<number, string>();
  for (const [rawFrameId, rawSessionId] of Object.entries(value)) {
    const frameId = Number(rawFrameId);
    if (!Number.isSafeInteger(frameId) || frameId <= 0 || !isNestedSessionId(rawSessionId)) continue;
    sessions.set(frameId, rawSessionId);
    if (sessions.size === MAX_TRACKED_NESTED_RENDERERS_PER_TAB) break;
  }
  if (sessions.size > 0) nestedRendererSessions.set(tabId, sessions);
}

/** Persists the current immutable registry projection for one WebView tab. */
function persistNestedRendererSessions(tabId: number): void {
  void rendererRegistryEffects
    .enqueue(async (): Promise<void> => {
      const sessions = nestedRendererSessions.get(tabId);
      const key = rendererRegistryKey(tabId);
      if (!sessions || sessions.size === 0) {
        await chrome.storage.session.remove(key);
        return;
      }
      await chrome.storage.session.set({ [key]: Object.fromEntries(sessions) });
    })
    .catch((error: unknown): void => {
      console.warn("Failed to persist Rings WebView renderer registry", error);
    });
}

/** Builds the session-storage key for one positive Chrome tab identity. */
function rendererRegistryKey(tabId: number): string {
  if (!isWebviewTabId(tabId)) throw new Error("invalid WebView renderer-registry tab identifier");
  return `${NESTED_RENDERER_STORAGE_PREFIX}${tabId}`;
}

/** Parses only this extension's packaged recursive-renderer identity. */
function nestedSessionFromRendererUrl(value: string): string | undefined {
  try {
    const expected = new URL(chrome.runtime.getURL(WEBVIEW_RENDERER_DOCUMENT));
    const candidate = new URL(value);
    if (candidate.origin !== expected.origin || candidate.pathname !== expected.pathname) return undefined;
    const sessionId = candidate.searchParams.get("nestedSession");
    return isNestedSessionId(sessionId) ? sessionId : undefined;
  } catch (_error: unknown) {
    return undefined;
  }
}

/** Recognizes only UUIDv4 session witnesses generated by a packaged renderer. */
function isNestedSessionId(value: unknown): value is string {
  return (
    typeof value === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu.test(value)
  );
}

/** Emits one browser navigation with either top-level or nested-session authority. */
function sendWebviewNavigation(tabId: number, url: string, sessionId?: string): void {
  chrome.runtime.sendMessage(
    { type: "rings.webview.navigate", tabId, url, ...(sessionId ? { sessionId } : {}) },
    (): void => {
      void chrome.runtime.lastError;
    },
  );
}

/** Uses the session-scoped deny rule as the WebView-tab identity witness. */
async function hasWebviewNetworkBlock(tabId: number): Promise<boolean> {
  const rules = await chrome.declarativeNetRequest.getSessionRules();
  return rules.some((rule: chrome.declarativeNetRequest.Rule): boolean => isWebviewNetworkBlockRule(rule, tabId));
}

/** Opens one application-owned popup and arms its tab before remote rendering begins. */
async function openWebviewWindow(): Promise<{ readonly tabId: number }> {
  const created = await createWebviewWindow();
  const tabId = created.tabs?.[0]?.id;
  if (typeof tabId !== "number") throw new Error("Chrome created the WebView window without a tab");
  await installWebviewNetworkBlock(tabId);
  return { tabId };
}

/** Promise wrapper around `chrome.windows.create` for the WebView popup. */
function createWebviewWindow(): Promise<chrome.windows.Window> {
  return new Promise((resolve, reject): void => {
    chrome.windows.create(
      {
        url: chrome.runtime.getURL(WEBVIEW_DOCUMENT),
        type: "popup",
        width: 1280,
        height: 860,
        focused: true,
      },
      (created?: chrome.windows.Window): void => {
        const runtimeError = chrome.runtime.lastError;
        if (runtimeError) reject(new Error(runtimeError.message));
        else if (created) resolve(created);
        else reject(new Error("Chrome did not return the WebView window"));
      },
    );
  });
}

/** Validates a WebView host activation message and re-arms its session rule. */
async function activateWebviewSender(sender: chrome.runtime.MessageSender): Promise<number> {
  const tabId = sender.tab?.id;
  const expectedUrl = chrome.runtime.getURL(WEBVIEW_DOCUMENT);
  if (typeof tabId !== "number" || sender.url !== expectedUrl) {
    throw new Error("WebView activation requires the packaged WebView host");
  }
  await installWebviewNetworkBlock(tabId);
  return tabId;
}

/** Installs the tab-scoped direct-network deny rule as one atomic replacement. */
async function installWebviewNetworkBlock(tabId: number): Promise<void> {
  const rule = webviewNetworkBlockRule(tabId);
  await webviewRuleEffects.enqueue(
    (): Promise<void> =>
      chrome.declarativeNetRequest.updateSessionRules({ removeRuleIds: [rule.id], addRules: [rule] }),
  );
}

/** Removes the deterministic tab-scoped network rule after the popup closes. */
async function removeWebviewNetworkBlock(tabId: number): Promise<void> {
  const ruleId = webviewRuleId(tabId);
  await webviewRuleEffects.enqueue(
    (): Promise<void> => chrome.declarativeNetRequest.updateSessionRules({ removeRuleIds: [ruleId] }),
  );
}

/** Pure constructor for the complete fail-closed HTTP(S) rule. */
function webviewNetworkBlockRule(tabId: number): chrome.declarativeNetRequest.Rule {
  return {
    id: webviewRuleId(tabId),
    priority: 1,
    action: { type: "block" },
    condition: {
      tabIds: [tabId],
      regexFilter: WEBVIEW_NETWORK_FILTER,
      resourceTypes: [...WEBVIEW_BLOCKED_RESOURCE_TYPES],
    },
  };
}

/** Semantic predicate shared by navigation authorization and stale-rule cleanup. */
function isWebviewNetworkBlockRule(rule: chrome.declarativeNetRequest.Rule, tabId: number): boolean {
  return (
    isWebviewTabId(tabId) &&
    rule.id === webviewRuleId(tabId) &&
    rule.action.type === "block" &&
    rule.condition.regexFilter === WEBVIEW_NETWORK_FILTER &&
    rule.condition.tabIds?.length === 1 &&
    rule.condition.tabIds[0] === tabId &&
    WEBVIEW_BLOCKED_RESOURCE_TYPES.every(
      (resourceType): boolean => rule.condition.resourceTypes?.includes(resourceType) ?? false,
    )
  );
}

/** Removes session rules whose owning WebView tab no longer exists. */
async function pruneStaleWebviewNetworkBlocks(): Promise<void> {
  const rules = await chrome.declarativeNetRequest.getSessionRules();
  const staleRuleIds: number[] = [];
  for (const rule of rules) {
    const tabId = rule.condition.tabIds?.length === 1 ? rule.condition.tabIds[0] : undefined;
    if (typeof tabId !== "number" || !isWebviewNetworkBlockRule(rule, tabId)) continue;
    if (!(await isWebviewHostTab(tabId))) staleRuleIds.push(rule.id);
  }
  if (staleRuleIds.length > 0) {
    await webviewRuleEffects.enqueue(
      (): Promise<void> => chrome.declarativeNetRequest.updateSessionRules({ removeRuleIds: staleRuleIds }),
    );
  }
}

/** Resolves whether a live extension context still owns this WebView tab. */
async function isWebviewHostTab(tabId: number): Promise<boolean> {
  const documentUrl = chrome.runtime.getURL(WEBVIEW_DOCUMENT);
  const contexts = await chrome.runtime.getContexts({ documentUrls: [documentUrl], tabIds: [tabId] });
  return contexts.some(
    (context: chrome.runtime.ExtensionContext): boolean =>
      context.tabId === tabId && context.documentUrl === documentUrl,
  );
}

/** Maps a Chrome tab identifier into the extension's reserved session-rule range. */
function webviewRuleId(tabId: number): number {
  if (!isWebviewTabId(tabId)) throw new Error("invalid WebView tab identifier");
  return tabId;
}

/** Pure total predicate for Chrome tab and declarative-rule identifiers. */
function isWebviewTabId(tabId: number): boolean {
  return Number.isSafeInteger(tabId) && tabId > 0 && tabId <= 2_147_483_647;
}

/** Returns whether a browser frame attempted a direct HTTP(S) navigation. */
function isHttpUrl(url: string): boolean {
  return url.startsWith("http://") || url.startsWith("https://");
}

/** Narrows one unknown runtime value to an exact message tag. */
function isMessage(value: unknown, type: string): value is RuntimeMessage {
  return typeof value === "object" && value !== null && (value as RuntimeMessage).type === type;
}
