#!/usr/bin/env node

/**
 * Runs a Playwright fixture for the real WebView overlay asset.
 */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createServer, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { type Browser, chromium } from "playwright";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const overlayPath = resolve(projectRoot, "assets", "webview-overlay.js");
const overlaySource = await readFile(overlayPath, "utf8");
const serverState = await serveSlowWebviewDocument(overlaySource);

let browser: Browser | undefined;
try {
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  const target = encodeURIComponent("https://www.google.com/");
  await page.goto(`http://127.0.0.1:${serverState.port}/webview/${target}`, {
    waitUntil: "commit",
  });
  await page.locator("#rings-webview-debug-overlay").waitFor({
    state: "attached",
    timeout: 1000,
  });

  const earlyState = await page.evaluate(() => {
    const loadingKey = "loading";
    const overlay = document.getElementById("rings-webview-debug-overlay");
    const addressForm = overlay?.shadowRoot?.getElementById("address-form");
    const loadingTrack = overlay?.shadowRoot?.getElementById("loading-track") as HTMLElement | null | undefined;
    return {
      bodyText: document.body?.textContent || "",
      loading: {
        busy: addressForm?.getAttribute("aria-busy"),
        loading: addressForm?.dataset[loadingKey],
        trackHidden: loadingTrack?.hidden,
      },
      mounted: Boolean(overlay),
      readyState: document.readyState,
    };
  });
  assert.equal(earlyState.mounted, true);
  assert.equal(earlyState.readyState, "loading");
  assert.equal(earlyState.bodyText.includes("late body"), false);
  assert.deepEqual(earlyState.loading, {
    busy: "true",
    loading: "true",
    trackHidden: false,
  });

  serverState.finish();
  await page.locator("#late-body").waitFor({ state: "attached", timeout: 2000 });
  await page.waitForFunction(() => {
    const loadingKey = "loading";
    return (
      document.getElementById("rings-webview-debug-overlay")?.shadowRoot?.getElementById("address-form")?.dataset[
        loadingKey
      ] === "false"
    );
  });
  const bodyPadding = await page.evaluate(() => document.body?.style.paddingTop || "");
  const finalLoading = await page.evaluate(() => {
    const loadingKey = "loading";
    const overlay = document.getElementById("rings-webview-debug-overlay");
    const addressForm = overlay?.shadowRoot?.getElementById("address-form");
    const loadingTrack = overlay?.shadowRoot?.getElementById("loading-track") as HTMLElement | null | undefined;
    return {
      busy: addressForm?.getAttribute("aria-busy"),
      loading: addressForm?.dataset[loadingKey],
      trackHidden: loadingTrack?.hidden,
    };
  });
  assert.match(bodyPadding, /46px/);
  assert.deepEqual(finalLoading, {
    busy: "false",
    loading: "false",
    trackHidden: true,
  });
} finally {
  serverState.finish();
  await browser?.close();
  await closeServer(serverState.server);
}

/**
 * Resolves the frontend project root from either source or generated script paths.
 */
function frontendProjectRoot(currentScriptDir: string): string {
  const parentDir = dirname(currentScriptDir);
  if (parentDir.endsWith("/.generated")) {
    return resolve(parentDir, "..");
  }
  return resolve(currentScriptDir, "..");
}

/**
 * Starts a server that streams the document head first and delays the body.
 */
function serveSlowWebviewDocument(overlay: string): Promise<{
  readonly finish: () => void;
  readonly port: number;
  readonly server: Server;
}> {
  let slowResponse: ServerResponse | undefined;
  let finished = false;
  const server = createServer((request, response) => {
    const requestUrl = new URL(request.url || "/", "http://127.0.0.1/");
    if (requestUrl.pathname === "/assets/webview-overlay.js") {
      response.writeHead(200, {
        "content-type": "application/javascript; charset=utf-8",
      });
      response.end(overlay);
      return;
    }
    if (requestUrl.pathname.startsWith("/webview/")) {
      slowResponse = response;
      response.writeHead(200, {
        "content-type": "text/html; charset=utf-8",
      });
      response.write(`<!doctype html>
<html>
<head>
  <title>Slow WebView Target</title>
  <script src="/assets/webview-overlay.js"></script>
</head>`);
      return;
    }
    response.writeHead(404, { "content-type": "text/plain" });
    response.end("not found");
  });

  const finish = () => {
    if (finished) return;
    finished = true;
    slowResponse?.end('<body><main id="late-body">late body</main></body></html>');
  };

  return new Promise((resolveListen, rejectListen) => {
    server.once("error", rejectListen);
    server.listen(0, "127.0.0.1", () => {
      server.off("error", rejectListen);
      const address = server.address();
      assert(address && typeof address !== "string", "expected TCP listener address");
      resolveListen({
        finish,
        port: (address as AddressInfo).port,
        server,
      });
    });
  });
}

/**
 * Closes an HTTP server and resolves once all handles are released.
 */
function closeServer(server: Server): Promise<void> {
  return new Promise((resolveClose, rejectClose) => {
    server.close((error) => {
      if (error) {
        rejectClose(error);
        return;
      }
      resolveClose();
    });
  });
}
