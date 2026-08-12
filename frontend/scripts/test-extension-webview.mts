#!/usr/bin/env node

/** Verifies the packaged MV3 Extension WebView protocol in Chromium. */
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  browserBootstrapFixture,
  type FixtureExtensionGlobal,
  type FixtureGatewayRecord,
  type FixtureRoute,
  type FixtureSessionRule,
  fixtureNavigationCount,
  frontendProjectRoot,
  installExtensionGatewayFixture,
  launchExtensionWebview,
  openExtensionWebview,
  verifyRealOffscreenGateway,
} from "./extension_webview_test_support.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const extensionPath = resolve(projectRoot, process.argv[2] ?? "dist-extension");
const fixtureBootstrap = await browserBootstrapFixture(projectRoot);
const fixtureRoutes = await extensionFixtureRoutes(projectRoot, fixtureBootstrap);
const harness = await launchExtensionWebview(extensionPath);
const { context, launcher } = harness;

try {
  const realGatewayFailure = await verifyRealOffscreenGateway(launcher);
  assert.equal(realGatewayFailure.ok, false);
  assert.equal(realGatewayFailure.status, 400);
  assert.equal(realGatewayFailure.errorCode, "invalid_webview_request");

  const popup = await openExtensionWebview(harness);
  const pageErrors: string[] = [];
  const consoleMessages: string[] = [];
  const requestFailures: string[] = [];
  popup.on("pageerror", (error: Error): void => {
    pageErrors.push(error.message);
  });
  popup.on("console", (message): void => {
    consoleMessages.push(`${message.type()}: ${message.text()}`);
  });
  popup.on("requestfailed", (request): void => {
    requestFailures.push(`${request.url()}: ${request.failure()?.errorText ?? "unknown failure"}`);
  });
  await installExtensionGatewayFixture(popup, fixtureRoutes);
  const pageCountBeforeRemoteRender = context.pages().length;

  await popup.locator("#webview-address").fill("https://fixture.example/");
  await popup.locator("#webview-address-form").evaluate((form: HTMLFormElement): void => form.requestSubmit());
  await popup.locator("#webview-status").filter({ hasText: "Loaded through Rings onion gateway" }).waitFor();
  const renderer = popup.frameLocator("#webview-frame");
  await renderer
    .locator("h1")
    .waitFor({ timeout: 10_000 })
    .catch(async (error: unknown): Promise<never> => {
      const status = await popup.locator("#webview-status").textContent();
      const frameText = await renderer.locator("body").textContent();
      throw new Error(
        `renderer did not receive the fixture; status=${status}; frame=${frameText}; pageErrors=${pageErrors.join(" | ")}; ${String(error)}`,
      );
    });
  assert.equal(await renderer.locator("h1").textContent(), "Onion fixture");
  assert.equal(await renderer.locator("html").evaluate((): string => document.title), "Fixture");
  assert.equal(await renderer.locator("script[data-rings-webview-bootstrap]").count(), 1);
  await renderer
    .locator("#runtime-result")
    .waitFor({ state: "visible", timeout: 10_000 })
    .catch(async (error: unknown): Promise<never> => {
      const requests = await popup.evaluate(
        (): readonly string[] => (globalThis as FixtureExtensionGlobal).__fixtureWebviewRequests ?? [],
      );
      throw new Error(
        `runtime fetch did not settle; requests=${requests.join(" | ")}; pageErrors=${pageErrors.join(" | ")}; console=${consoleMessages.join(" | ")}; ${String(error)}`,
      );
    });
  assert.equal(await renderer.locator("#runtime-result").textContent(), "runtime through onion bridge");
  const prototypeHardenedFetch = await renderer.locator("body").evaluate(async (): Promise<string> => {
    const descriptor = Object.getOwnPropertyDescriptor(MessagePort.prototype, "postMessage");
    Object.defineProperty(MessagePort.prototype, "postMessage", {
      configurable: true,
      value: (): never => {
        throw new Error("authored MessagePort hook observed the private gateway");
      },
    });
    try {
      return await fetch("https://fixture.example/runtime").then(
        (response: Response): Promise<string> => response.text(),
      );
    } finally {
      if (descriptor) Object.defineProperty(MessagePort.prototype, "postMessage", descriptor);
    }
  });
  assert.equal(prototypeHardenedFetch, "runtime through onion bridge");
  await renderer.locator("#empty-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#empty-result").textContent(), "empty 204 through onion bridge");
  await renderer.locator("#xhr-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#xhr-result").textContent(), "xhr through onion bridge");
  await renderer.locator("#xhr-reuse-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#xhr-reuse-result").textContent(), "xhr state reset through onion bridge");
  await renderer.locator("#xhr-abort-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(
    await renderer.locator("#xhr-abort-result").textContent(),
    "readystatechange:1,loadstart:1,readystatechange:4,abort:4,loadend:4;after:0",
  );
  await renderer.locator("#xhr-timeout-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(
    await renderer.locator("#xhr-timeout-result").textContent(),
    "readystatechange:1,loadstart:1,readystatechange:4,timeout:4,loadend:4;after:4",
  );
  await renderer.locator("#xhr-open-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#xhr-open-result").textContent(), "readystatechange:1,loadstart:1;after:1");
  await renderer.locator("#dynamic-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#dynamic-result").textContent(), "dynamic script through onion bridge");
  await renderer.locator("#redirect-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#redirect-result").textContent(), "redirect followed through onion bridge");
  await renderer.locator("#manual-redirect-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#manual-redirect-result").textContent(), "opaqueredirect:0:no-location");
  await renderer.locator("#worker-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(
    await renderer.locator("#worker-result").textContent(),
    "worker-dep:shadow-local:test:worker runtime through onion bridge:local:rtc=true",
  );
  await renderer.locator("#worker-clone-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#worker-clone-result").textContent(), "before:7:detached=true");
  await renderer.locator("#shared-worker-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#shared-worker-result").textContent(), "shared:1:shared");
  await renderer
    .locator("#module-worker-result")
    .waitFor({ state: "visible", timeout: 10_000 })
    .catch(async (error: unknown): Promise<never> => {
      const requests = await popup.evaluate(
        (): readonly string[] => (globalThis as FixtureExtensionGlobal).__fixtureWebviewRequests ?? [],
      );
      throw new Error(
        `module worker did not settle; requests=${requests.join(" | ")}; failures=${requestFailures.join(" | ")}; pageErrors=${pageErrors.join(" | ")}; console=${consoleMessages.join(" | ")}; ${String(error)}`,
      );
    });
  assert.equal(await renderer.locator("#module-worker-result").textContent(), "module-dep:module");
  await renderer.locator("#module-diamond-result").waitFor({ state: "visible", timeout: 10_000 });
  assert.equal(await renderer.locator("#module-diamond-result").textContent(), "same=true:executions=1");
  assert.equal(
    await popup.evaluate(
      (): number =>
        ((globalThis as FixtureExtensionGlobal).__fixtureWebviewRequests ?? []).filter(
          (entry: string): boolean => entry === "subresource https://fixture.example/module-diamond-shared.js",
        ).length,
    ),
    1,
    "diamond module dependency was fetched more than once for one graph",
  );
  await renderer
    .frameLocator("#nested-remote")
    .locator("#nested-runtime-result")
    .waitFor({ timeout: 10_000 })
    .catch(async (error: unknown): Promise<never> => {
      const requests = await popup.evaluate(
        (): readonly string[] => (globalThis as FixtureExtensionGlobal).__fixtureWebviewRequests ?? [],
      );
      const nestedSource = await renderer.locator("#nested-remote").getAttribute("src");
      const frameUrls = popup.frames().map((frame): string => frame.url());
      throw new Error(
        `nested renderer did not settle; src=${nestedSource}; frames=${frameUrls.join(" | ")}; requests=${requests.join(" | ")}; failures=${requestFailures.join(" | ")}; pageErrors=${pageErrors.join(" | ")}; console=${consoleMessages.join(" | ")}; ${String(error)}`,
      );
    });
  assert.equal(
    await renderer.frameLocator("#nested-remote").locator("#nested-runtime-result").textContent(),
    "nested runtime through onion bridge",
  );
  assert.equal(
    await renderer
      .frameLocator("#nested-remote")
      .locator("body")
      .evaluate((): boolean =>
        Boolean(
          (globalThis as typeof globalThis & { chrome?: { readonly runtime?: { readonly id?: string } } }).chrome
            ?.runtime?.id,
        ),
      ),
    false,
    "web-accessible recursive renderer unexpectedly received extension APIs",
  );
  await renderer
    .frameLocator("#nested-remote")
    .locator("body")
    .evaluate((): void => {
      Object.defineProperty(globalThis, "MessageChannel", {
        configurable: true,
        value: class AuthoredMessageChannel {
          constructor() {
            throw new Error("authored MessageChannel constructor observed a private gateway");
          }
        },
      });
      Object.defineProperty(MessagePort.prototype, "postMessage", {
        configurable: true,
        value: (): never => {
          throw new Error("authored MessagePort hook observed a nested private gateway");
        },
      });
    });
  await renderer.frameLocator("#nested-remote").locator("#nested-navigation").click();
  await renderer
    .frameLocator("#nested-remote")
    .locator("#nested-clean-state")
    .waitFor({ timeout: 10_000 })
    .catch(async (error: unknown): Promise<never> => {
      const requests = await popup.evaluate(
        (): readonly string[] => (globalThis as FixtureExtensionGlobal).__fixtureWebviewRequests ?? [],
      );
      const frameBodies = await Promise.all(
        popup.frames().map(async (frame): Promise<string> => {
          const body = await frame
            .locator("body")
            .textContent()
            .catch((): string => "<unavailable>");
          return JSON.stringify({ url: frame.url(), body: body?.slice(0, 300) });
        }),
      );
      const nestedFrames = await renderer
        .locator("iframe")
        .evaluateAll((frames): readonly string[] =>
          frames.map(
            (frame): string =>
              `${frame.id || "<no-id>"}:${frame.getAttribute("srcdoc")?.slice(0, 80) ?? "<no-srcdoc>"}:${frame.getAttribute("style") ?? "<no-style>"}`,
          ),
        );
      throw new Error(
        `nested navigation did not commit; frames=${popup
          .frames()
          .map((frame): string => frame.url())
          .join(
            " | ",
          )}; nestedFrames=${JSON.stringify(nestedFrames)}; bodies=${frameBodies.join(" | ")}; requests=${requests.join(" | ")}; console=${consoleMessages.join(" | ")}; ${String(error)}`,
      );
    });
  assert.equal(
    await renderer.frameLocator("#nested-remote").locator("#nested-clean-state").textContent(),
    "undefined",
    "nested navigation reused the outgoing document realm",
  );
  await renderer.frameLocator("#nested-srcdoc").locator("#srcdoc-runtime-result").waitFor({ timeout: 10_000 });
  assert.equal(
    await renderer.frameLocator("#nested-srcdoc").locator("#srcdoc-runtime-result").textContent(),
    "srcdoc runtime through onion bridge",
  );
  const nestedAuthority = await popup.evaluate((): FixtureGatewayRecord | undefined =>
    (globalThis as FixtureExtensionGlobal).__fixtureWebviewRecords?.find(
      (record: FixtureGatewayRecord): boolean => record.target === "https://fixture.example/nested-frame",
    ),
  );
  assert.deepEqual(nestedAuthority, {
    target: "https://fixture.example/nested-frame",
    sourceTarget: "https://fixture.example/",
    kind: "navigation",
    topLevelNavigation: false,
  });
  assert.match((await renderer.locator("#onion-image").getAttribute("src")) ?? "", /^blob:/);
  assert.match((await renderer.locator("#multi-resource").getAttribute("src")) ?? "", /^blob:/);
  assert.match((await renderer.locator("#multi-resource").getAttribute("poster")) ?? "", /^blob:/);
  assert.match((await renderer.locator("#styled").getAttribute("style")) ?? "", /blob:/);
  const srcset = (await renderer.locator("#responsive-image").getAttribute("srcset")) ?? "";
  assert.equal(srcset.match(/blob:/g)?.length, 2, `responsive srcset did not preserve both candidates: ${srcset}`);
  await renderer.locator("#imported-style").evaluate(
    (element: Element): Promise<void> =>
      new Promise((resolve, reject): void => {
        const deadline = Date.now() + 5_000;
        const poll = (): void => {
          if (getComputedStyle(element).color === "rgb(1, 2, 3)") resolve();
          else if (Date.now() >= deadline) reject(new Error("quoted CSS @import did not load through onion hydration"));
          else setTimeout(poll, 50);
        };
        poll();
      }),
  );
  assert.match(
    await renderer
      .locator("#nested-style")
      .evaluate((element: Element): string => getComputedStyle(element).backgroundImage),
    /^url\("blob:/,
  );
  assert.equal(await renderer.locator("html").getAttribute("data-popup-blocked"), "true");
  assert.equal(await renderer.locator("html").getAttribute("data-rtc-blocked"), "true");
  assert.equal(await renderer.locator("html").getAttribute("data-web-transport-blocked"), "true");
  assert.equal(context.pages().length, pageCountBeforeRemoteRender, "sandboxed content opened a direct browser page");
  assert.equal(await renderer.locator("html").getAttribute("data-gateway-leaked"), "false");
  assert.equal(
    await renderer
      .locator("html")
      .evaluate((): boolean =>
        Boolean(
          (globalThis as typeof globalThis & { chrome?: { readonly runtime?: { readonly id?: string } } }).chrome
            ?.runtime?.id,
        ),
      ),
    false,
    "sandboxed remote document unexpectedly received extension APIs",
  );

  await renderer.locator("html").evaluate((): void => {
    void fetch("https://fixture.example/source-redirect")
      .then((response: Response): Promise<string> => response.text())
      .then((): void => {
        document.documentElement.setAttribute("data-source-request-done", "true");
      });
  });
  await popup.locator("#webview-address").fill("https://next.example/delayed");
  await popup.locator("#webview-address-form").evaluate((form: HTMLFormElement): void => form.requestSubmit());
  await popup.locator('body[data-loading="true"]').waitFor();
  assert.equal(
    await renderer.locator("h1").textContent(),
    "Onion fixture",
    "pending navigation replaced the committed renderer before render acknowledgement",
  );
  await renderer.locator('html[data-source-request-done="true"]').waitFor({ timeout: 10_000 });
  const sourceRecords = await popup.evaluate((): readonly FixtureGatewayRecord[] =>
    ((globalThis as FixtureExtensionGlobal).__fixtureWebviewRecords ?? []).filter(
      (record: FixtureGatewayRecord): boolean =>
        record.target === "https://fixture.example/source-redirect" ||
        record.target === "https://fixture.example/source-final",
    ),
  );
  assert.deepEqual(
    sourceRecords.map((record: FixtureGatewayRecord): string | undefined => record.sourceTarget),
    ["https://fixture.example/", "https://fixture.example/"],
    "a redirect hop borrowed the concurrently pending renderer's source principal",
  );
  await popup.locator("#webview-status").filter({ hasText: "Loaded through Rings onion gateway" }).waitFor();
  assert.equal(await renderer.locator("h1").textContent(), "Delayed fixture");
  await popup.locator("#webview-back").click();
  await popup.locator("#webview-status").filter({ hasText: "Loaded through Rings onion gateway" }).waitFor();
  assert.equal(await renderer.locator("h1").textContent(), "Onion fixture");

  await renderer.locator("#direct-navigation").click();
  await popup.locator("#webview-address").waitFor();
  await popup
    .waitForFunction(
      (): boolean =>
        (document.querySelector<HTMLInputElement>("#webview-address")?.value ?? "") ===
        "https://fixture.example/direct-navigation",
      undefined,
      { timeout: 10_000 },
    )
    .catch(async (error: unknown): Promise<never> => {
      const requests = await popup.evaluate(
        (): readonly string[] => (globalThis as FixtureExtensionGlobal).__fixtureWebviewRequests ?? [],
      );
      throw new Error(
        `blocked nested navigation was not forwarded; requests=${requests.join(" | ")}; pageErrors=${pageErrors.join(" | ")}; console=${consoleMessages.join(" | ")}; ${String(error)}`,
      );
    });
  await popup.locator("#webview-status").filter({ hasText: "Loaded through Rings onion gateway" }).waitFor();
  assert.equal(await renderer.locator("h1").textContent(), "Onion fixture");
  const committedBeforeFailure = await popup.locator("#webview-address").inputValue();
  await popup.locator("#webview-address").fill("https://fixture.example/navigation-failure");
  await popup.locator("#webview-address-form").evaluate((form: HTMLFormElement): void => form.requestSubmit());
  await popup.locator("#webview-status").filter({ hasText: "HTTP 503" }).waitFor();
  assert.equal(await popup.locator("#webview-address").inputValue(), committedBeforeFailure);
  assert.equal(await renderer.locator("h1").textContent(), "Onion fixture");
  const navigationCount = await fixtureNavigationCount(popup);
  await popup.locator("#webview-reload").click();
  await popup.waitForFunction(
    (previous: number): boolean =>
      ((globalThis as FixtureExtensionGlobal).__fixtureWebviewRequests ?? []).filter((entry: string): boolean =>
        entry.startsWith("navigation "),
      ).length > previous,
    navigationCount,
  );
  await popup.locator("#webview-status").filter({ hasText: "Loaded through Rings onion gateway" }).waitFor();
  assert.equal(await renderer.locator("h1").textContent(), "Onion fixture");
  const fixtureRequests = await popup.evaluate(
    (): readonly string[] => (globalThis as FixtureExtensionGlobal).__fixtureWebviewRequests ?? [],
  );
  assert(fixtureRequests.includes("xhr https://fixture.example/runtime-xhr"));
  assert(!fixtureRequests.some((entry: string): boolean => entry.includes("https://fixture.example/forged")));
  assert(!fixtureRequests.some((entry: string): boolean => entry.includes("/assets/webview-overlay.js")));

  const isolation = await popup.evaluate(
    async (): Promise<{
      readonly tabId?: number;
      readonly ruleTabIds: readonly number[];
      readonly blocksHttp: boolean;
      readonly blockedResourceTypes: readonly string[];
    }> => {
      const extensionChrome = (
        globalThis as typeof globalThis & {
          chrome: {
            tabs: { getCurrent(): Promise<{ readonly id?: number } | undefined> };
            declarativeNetRequest: { getSessionRules(): Promise<readonly FixtureSessionRule[]> };
          };
        }
      ).chrome;
      const current = await extensionChrome.tabs.getCurrent();
      const rules = await extensionChrome.declarativeNetRequest.getSessionRules();
      const rule = rules.find(
        (candidate: FixtureSessionRule): boolean => candidate.condition.tabIds?.includes(current?.id ?? -1) ?? false,
      );
      return {
        ...(typeof current?.id === "number" ? { tabId: current.id } : {}),
        ruleTabIds: rule?.condition.tabIds ?? [],
        blocksHttp: rule?.action.type === "block" && rule.condition.regexFilter === "^https?://",
        blockedResourceTypes: rule?.condition.resourceTypes ?? [],
      };
    },
  );
  assert.equal(typeof isolation.tabId, "number");
  assert.deepEqual(isolation.ruleTabIds, [isolation.tabId]);
  assert.equal(isolation.blocksHttp, true);
  assert.deepEqual([...isolation.blockedResourceTypes].sort(), [
    "csp_report",
    "font",
    "image",
    "main_frame",
    "media",
    "object",
    "other",
    "ping",
    "script",
    "stylesheet",
    "sub_frame",
    "webbundle",
    "websocket",
    "webtransport",
    "xmlhttprequest",
  ]);
  assert.equal(
    consoleMessages.some(
      (message: string): boolean =>
        message.includes("rings-webview.invalid/webview/") && message.includes("Content Security Policy"),
    ),
    false,
    `renderer attempted a direct gateway URL before hydration: ${consoleMessages.join(" | ")}`,
  );
  assert.deepEqual(pageErrors, []);
  console.log("Extension onion WebView window fixture passed");
} finally {
  await harness.close();
}

/** Builds the immutable route algebra consumed by the browser-side gateway adapter. */
async function extensionFixtureRoutes(
  root: string,
  bootstrap: string,
): Promise<Readonly<Record<string, FixtureRoute>>> {
  const template = await readFile(resolve(root, "scripts", "fixtures", "extension-webview", "index.html"), "utf8");
  const document = renderFixtureDocument(template, bootstrap);
  return Object.freeze({
    ...documentFixtureRoutes(document),
    ...resourceFixtureRoutes(),
    ...workerFixtureRoutes(),
  });
}

/** Substitutes the controlled gateway URLs and shared Rust bootstrap exactly once. */
function renderFixtureDocument(template: string, bootstrap: string): string {
  const image = controlledTarget("https://fixture.example/image.svg");
  const largeImage = controlledTarget("https://fixture.example/image-large.svg");
  return substituteFixtureTokens(template, {
    __IMAGE_TARGET__: JSON.stringify(image),
    __IMAGE_LARGE_TARGET__: JSON.stringify(largeImage),
    __SRCSET_TARGETS__: JSON.stringify(`${image} 1x, ${largeImage} 2x`),
    __STYLE_TARGET__: JSON.stringify(`background-image:url(${image})`),
    __IMPORT_TARGET__: JSON.stringify(controlledTarget("https://fixture.example/import.css")),
    __RINGS_BOOTSTRAP__: bootstrap.replaceAll("</script", "<\\/script"),
  });
}

/** Requires each fixture token to be declared before replacing all of its data occurrences. */
function substituteFixtureTokens(template: string, replacements: Readonly<Record<string, string>>): string {
  return Object.entries(replacements).reduce((document: string, [token, value]): string => {
    assert(document.includes(token), `fixture token ${token} is absent`);
    return document.replaceAll(token, value);
  }, template);
}

/** Navigation documents remain data; only the typed route table is authored in MTS. */
function documentFixtureRoutes(document: string): Readonly<Record<string, FixtureRoute>> {
  return {
    "https://fixture.example/": htmlRoute(document),
    "https://fixture.example/direct-navigation": htmlRoute(document),
    "https://fixture.example/navigation-failure": { body: "", status: 503 },
    "https://next.example/delayed": htmlRoute(`<!doctype html><html><head><title>Delayed</title></head><body>
      <h1>Delayed fixture</h1>
      <script type="module">await new Promise((resolve) => setTimeout(resolve, 500));</script>
    </body></html>`),
    "https://fixture.example/nested-frame": htmlRoute(`<!doctype html><html><body>
      <p id="nested-runtime-result"></p>
      <button id="nested-navigation" type="button" onclick="location.href='https://fixture.example/nested-frame-next'">
        next nested document
      </button>
      <script>
        fetch("https://fixture.example/nested-runtime").then((response) => response.text()).then((text) => {
          document.getElementById("nested-runtime-result").textContent = text;
        });
        globalThis.outgoingNestedState = "must not survive";
      </script>
    </body></html>`),
    "https://fixture.example/nested-frame-next": htmlRoute(`<!doctype html><html><body>
      <p id="nested-clean-state"></p>
      <script>document.getElementById("nested-clean-state").textContent = typeof outgoingNestedState;</script>
    </body></html>`),
  };
}

/** Fetch, XHR, redirect, stylesheet, and image responses share one declarative table. */
function resourceFixtureRoutes(): Readonly<Record<string, FixtureRoute>> {
  return {
    "https://fixture.example/runtime": textRoute("runtime through onion bridge"),
    "https://fixture.example/empty": { body: "", status: 204 },
    "https://fixture.example/runtime-xhr": textRoute("xhr through onion bridge"),
    "https://fixture.example/runtime-xhr-reused": textRoute("xhr state reset through onion bridge", {
      errorOnHeaderNames: ["x-stale"],
    }),
    "https://fixture.example/runtime-xhr-slow": textRoute("late response must be ignored", { delayMs: 200 }),
    "https://fixture.example/source-redirect": redirectRoute("https://fixture.example/source-final", 100),
    "https://fixture.example/source-final": textRoute("source principal remained stable"),
    "https://fixture.example/nested-runtime": textRoute("nested runtime through onion bridge"),
    "https://fixture.example/srcdoc-runtime": textRoute("srcdoc runtime through onion bridge"),
    "https://fixture.example/dynamic.js": scriptRoute(
      'document.getElementById("dynamic-result").textContent = "dynamic script through onion bridge";' +
        'document.getElementById("dynamic-result").hidden = false;',
    ),
    "https://fixture.example/image.svg": svgRoute(
      '<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" fill="cyan"/></svg>',
    ),
    "https://fixture.example/image-large.svg": svgRoute(
      '<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="blue"/></svg>',
    ),
    "https://fixture.example/import.css": {
      body: '@import "./nested.css"; #imported-style { color: rgb(1, 2, 3); }',
      contentType: "text/css",
    },
    "https://fixture.example/nested.css": {
      body: '#nested-style { background-image: url("./nested.svg"); }',
      contentType: "text/css",
    },
    "https://fixture.example/nested.svg": svgRoute(
      '<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1" fill="red"/></svg>',
    ),
    "https://fixture.example/redirect-start": redirectRoute("https://fixture.example/redirect-end"),
    "https://fixture.example/redirect-end": textRoute("redirect followed through onion bridge"),
    "https://fixture.example/worker-runtime": textRoute("worker runtime through onion bridge"),
  };
}

/** Authored Worker modules are fixture data and are materialized through the onion gateway. */
function workerFixtureRoutes(): Readonly<Record<string, FixtureRoute>> {
  return {
    "https://fixture.example/worker.js": scriptRoute(`
      function invokeLocal(importScripts) { return importScripts("local"); }
      const shadowedImport = invokeLocal((value) => "shadow-" + value);
      self.importScripts("./worker-dep.js");
      Object.defineProperty(MessagePort.prototype, "postMessage", {
        configurable: true,
        value: () => { throw new Error("worker observed the private gateway port"); }
      });
      self.onmessage = (event) => {
        let rtcBlocked = false;
        try { new RTCPeerConnection(); } catch (_error) { rtcBlocked = true; }
        Promise.all([
          fetch("https://fixture.example/worker-runtime").then((response) => response.text()),
          fetch("data:text/plain,local").then((response) => response.text())
        ]).then(([text, local]) => {
          self.postMessage(
            workerPrefix + ":" + shadowedImport + ":" + event.data + ":" + text + ":" + local + ":rtc=" + rtcBlocked
          );
        });
      };
    `),
    "https://fixture.example/worker-clone.js": scriptRoute(`
      self.onmessage = (event) => {
        self.postMessage(event.data.payload.label + ":" + new Uint8Array(event.data.buffer)[0]);
      };
    `),
    "https://fixture.example/worker-dep.js": scriptRoute('globalThis.workerPrefix = "worker-dep";'),
    "https://fixture.example/shared-worker.js": scriptRoute(`
      let connectionCount = 0;
      self.onconnect = (event) => {
        connectionCount += 1;
        const port = event.ports[0];
        port.onmessage = (message) => port.postMessage("shared:" + connectionCount + ":" + message.data);
        port.start();
      };
    `),
    "https://fixture.example/module-worker.js": scriptRoute(`
      import { prefix } from "./module-worker-dep.js";
      self.onmessage = (event) => self.postMessage(prefix + ":" + event.data);
    `),
    "https://fixture.example/module-worker-dep.js": scriptRoute('export const prefix = "module-dep";'),
    "https://fixture.example/module-diamond.js": scriptRoute(`
      import { leftToken } from "./module-diamond-left.js";
      import { rightToken } from "./module-diamond-right.js";
      self.onmessage = () =>
        self.postMessage("same=" + (leftToken === rightToken) + ":executions=" + globalThis.diamondExecutions);
    `),
    "https://fixture.example/module-diamond-left.js": scriptRoute(
      'export { sharedToken as leftToken } from "./module-diamond-shared.js";',
    ),
    "https://fixture.example/module-diamond-right.js": scriptRoute(
      'export { sharedToken as rightToken } from "./module-diamond-shared.js";',
    ),
    "https://fixture.example/module-diamond-shared.js": scriptRoute(`
      globalThis.diamondExecutions = (globalThis.diamondExecutions ?? 0) + 1;
      export const sharedToken = {};
    `),
  };
}

/** Maps an authored HTTPS URL into the renderer's non-network placeholder origin. */
function controlledTarget(target: string): string {
  return `https://rings-webview.invalid/webview/${encodeURIComponent(target)}`;
}

/** Constructs a successful HTML route. */
function htmlRoute(body: string): FixtureRoute {
  return { body, contentType: "text/html; charset=utf-8" };
}

/** Constructs a successful UTF-8 text route with optional deterministic behavior. */
function textRoute(body: string, behavior: Omit<FixtureRoute, "body" | "contentType"> = {}): FixtureRoute {
  return { body, contentType: "text/plain; charset=utf-8", ...behavior };
}

/** Constructs a successful JavaScript route. */
function scriptRoute(body: string): FixtureRoute {
  return { body, contentType: "text/javascript; charset=utf-8" };
}

/** Constructs a successful SVG image route. */
function svgRoute(body: string): FixtureRoute {
  return { body, contentType: "image/svg+xml" };
}

/** Constructs a gateway-relative redirect without exposing the placeholder origin to authored code. */
function redirectRoute(target: string, delayMs?: number): FixtureRoute {
  return {
    body: "",
    status: 302,
    headers: [{ name: "Location", value: controlledTarget(target) }],
    ...(delayMs === undefined ? {} : { delayMs }),
  };
}
