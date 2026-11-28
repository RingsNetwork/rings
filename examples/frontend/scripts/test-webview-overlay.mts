#!/usr/bin/env node

/**
 * Runs a Playwright fixture for the real WebView overlay asset.
 *
 * Every step is observed as an event, never bounded by a wall-clock oracle:
 *
 * ```text
 * goto ──► document commit
 * server ─► script request          (fixture serves /assets/webview-overlay.js)
 * page ───► partial-document parse  (the head's overlay <script> is inserted)
 * page ───► overlay mount           (marker global + host element exist)
 *    ── assert the overlay mounted while the document is still loading ──
 * server.finish()
 * page ───► late body parse         (#late-body is inserted)
 * page ───► load                    (after the overlay's own load listener)
 *    ── assert the final loading state and body spacing ──
 * ```
 *
 * A single hang guard spans the whole run; it only fails a hung test, and its
 * error names the step being awaited and when every other step was reached.
 */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createServer, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { type Browser, chromium } from "playwright";

/** The observed steps, in the order the fixture produces them. */
const STEPS = [
  "document commit",
  "script request",
  "partial-document parse",
  "overlay mount",
  "late body parse",
  "load",
] as const;

/** One observed step of the fixture. */
type Step = (typeof STEPS)[number];

/** The steps the page reports through the exposed binding. */
type PageStep = Extract<Step, "partial-document parse" | "overlay mount" | "late body parse" | "load">;

/** Upper bound on the whole run; reaching it means the test hung, not that it was slow. */
const HANG_GUARD_MS = 120_000;

/** Name of the binding through which the page reports its steps. */
const PAGE_STEP_BINDING = "__ringsOverlayTestStep";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const overlayPath = resolve(projectRoot, "assets", "webview-overlay.js");
const overlaySource = await readFile(overlayPath, "utf8");
const steps = stepLog(HANG_GUARD_MS);
const serverState = await serveSlowWebviewDocument(overlaySource, () => steps.reach("script request"));

let browser: Browser | undefined;
try {
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  await page.exposeFunction(PAGE_STEP_BINDING, (step: PageStep) => steps.reach(step));
  await page.addInitScript(reportPageSteps, PAGE_STEP_BINDING);
  const target = encodeURIComponent("https://www.google.com/");
  steps.track(
    "document commit",
    page.goto(`http://127.0.0.1:${serverState.port}/webview/${target}`, {
      timeout: 0,
      waitUntil: "commit",
    }),
  );
  await steps.await("document commit");
  await steps.await("script request");
  await steps.await("partial-document parse");
  await steps.await("overlay mount");

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
  await steps.await("late body parse");
  await steps.await("load");
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
 * Records when each step is reached and awaits steps under one shared deadline.
 *
 * Each step latches: `reach` is idempotent, so a step reached before it is
 * awaited resolves immediately. Every `await` races the same deadline, fixed
 * at construction, so the guard bounds the whole run rather than each step.
 */
function stepLog(guardMs: number): {
  readonly await: (step: Step) => Promise<void>;
  readonly reach: (step: Step) => void;
  readonly track: (step: Step, event: Promise<unknown>) => void;
} {
  const startedAt = Date.now();
  const deadline = startedAt + guardMs;
  const reachedAt = new Map<Step, number>();
  const outcomes = new Map(
    STEPS.map((step) => {
      let settle: { resolve: () => void; reject: (error: unknown) => void } | undefined;
      const outcome = new Promise<void>((resolveStep, rejectStep) => {
        settle = { resolve: resolveStep, reject: rejectStep };
      });
      // Every outcome is raced in `await`; this only keeps an early failure from being unhandled.
      outcome.catch(() => undefined);
      assert(settle, "promise executors run synchronously");
      return [step, { outcome, settle }] as const;
    }),
  );

  const entry = (step: Step) => {
    const found = outcomes.get(step);
    assert(found, `unknown step ${step}`);
    return found;
  };

  const reach = (step: Step) => {
    if (reachedAt.has(step)) return;
    reachedAt.set(step, Date.now() - startedAt);
    entry(step).settle.resolve();
  };

  /** Renders every step with the time it was reached, or `pending`. */
  const report = (awaited: Step) =>
    [
      `hang guard (${guardMs} ms) expired while awaiting "${awaited}"; steps:`,
      ...STEPS.map((step) => {
        const at = reachedAt.get(step);
        return `  ${step}: ${at === undefined ? "pending" : `reached at +${at} ms`}`;
      }),
    ].join("\n");

  return {
    await: (step) => {
      let timer: NodeJS.Timeout | undefined;
      const expired = new Promise<never>((_, rejectExpired) => {
        timer = setTimeout(() => rejectExpired(new Error(report(step))), Math.max(0, deadline - Date.now()));
      });
      return Promise.race([entry(step).outcome, expired]).finally(() => clearTimeout(timer));
    },
    reach,
    track: (step, event) => {
      event.then(
        () => reach(step),
        (error: unknown) => entry(step).settle.reject(error),
      );
    },
  };
}

/**
 * Page init script: reports each page-side step through the exposed binding.
 *
 * A `MutationObserver` on the whole document sees the parser insert the head's
 * overlay script, the overlay append its host, and the late body arrive. The
 * `load` listener is registered only once the overlay has mounted, i.e. after
 * the overlay's own `load` listener, so `load` is reported after the overlay
 * has cleared its loading state. Serialized into the page, so self-contained.
 */
function reportPageSteps(binding: string): void {
  const page = globalThis as typeof globalThis & {
    __ringsWebviewDebugOverlay?: unknown;
  } & Record<string, unknown>;
  const reported = new Set<string>();
  const report = (step: string) => {
    if (reported.has(step)) return;
    reported.add(step);
    const notify = page[binding];
    if (typeof notify === "function") void notify(step);
  };
  const observe = () => {
    if (document.querySelector('script[src="/assets/webview-overlay.js"]')) {
      report("partial-document parse");
    }
    if (
      !reported.has("overlay mount") &&
      page.__ringsWebviewDebugOverlay &&
      document.getElementById("rings-webview-debug-overlay")
    ) {
      report("overlay mount");
      globalThis.addEventListener("load", () => report("load"), { once: true });
    }
    if (document.getElementById("late-body")) {
      report("late body parse");
    }
  };
  new MutationObserver(observe).observe(document, { childList: true, subtree: true });
  observe();
}

/**
 * Starts a server that streams the document head first and delays the body.
 *
 * `onScriptRequest` fires when the overlay script is served.
 */
function serveSlowWebviewDocument(
  overlay: string,
  onScriptRequest: () => void,
): Promise<{
  readonly finish: () => void;
  readonly port: number;
  readonly server: Server;
}> {
  let slowResponse: ServerResponse | undefined;
  let finished = false;
  const server = createServer((request, response) => {
    const requestUrl = new URL(request.url || "/", "http://127.0.0.1/");
    if (requestUrl.pathname === "/assets/webview-overlay.js") {
      onScriptRequest();
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
