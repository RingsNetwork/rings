#!/usr/bin/env node

/**
 * Verifies the standalone paper hub (`/papers/`) and full-text + PDF readers (`/papers/rings/`,
 * `/papers/dranking/`) alongside first-party beacon telemetry in headless Chromium.
 */

import { readFile, stat } from "node:fs/promises";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { basename, dirname, extname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium, type Route } from "playwright";

/** Captured beacon payload sent by `/papers/papers-telemetry.js` to `POST /api/telemetry`. */
type TelemetryPayload = {
  readonly event?: string;
  readonly paper?: string;
  readonly format?: string;
  readonly mode?: string;
  readonly source?: string;
  readonly depth?: number;
};

/** Local loopback HTTP server and collected telemetry beacons. */
type RunningServer = {
  readonly server: Server;
  readonly origin: string;
  readonly beacons: TelemetryPayload[];
};

const watchdog = setTimeout((): void => {
  console.error("TIMEOUT: test-papers-reader.mts exceeded 30s watchdog");
  process.exit(1);
}, 30_000);
watchdog.unref();

const scriptDir = dirname(fileURLToPath(import.meta.url));
const frontendRoot = frontendProjectRoot(scriptDir);
const distRoot = resolve(frontendRoot, "dist");

const running = await startTestServer(distRoot);
const browser = await chromium.launch({ headless: true });
try {
  const context = await browser.newContext({ acceptDownloads: true });
  await context.route("https://cdn.jsdelivr.net/**", async (route: Route): Promise<void> => {
    const url = route.request().url();
    const contentType = url.endsWith(".css") ? "text/css; charset=utf-8" : "text/javascript; charset=utf-8";
    await route.fulfill({ status: 200, contentType, body: "" });
  });

  const page = await context.newPage();
  const pageErrors: string[] = [];
  page.on("pageerror", (error: Error): void => {
    pageErrors.push(error.message);
  });

  // 1. Verify `/papers/` hub view + stats hydration
  await page.goto(`${running.origin}/papers/`, { waitUntil: "load" });
  assertEqual(await page.locator(".paper-hub-card").count(), 2, "hub must list both protocol papers");
  await page.waitForFunction((): boolean => {
    const badge = document.querySelector('[data-paper-stats="rings"]');
    return Boolean(badge && !badge.hasAttribute("hidden") && badge.textContent?.includes("128 reads"));
  });
  await waitForBeacon(
    running.beacons,
    (b): boolean => b.event === "paper_hub_view" && b.paper === "hub",
    "hub load must emit paper_hub_view",
  );

  // 2. Verify `/papers/rings/` native Web Full-Text reader + scroll milestones + cite + PDF preview + download
  await page.goto(`${running.origin}/papers/rings/`, { waitUntil: "load" });
  assertEqual(await page.locator("article#paper-manuscript h1").count(), 1, "rings reader must render one h1");
  assertAtLeast(await page.locator(".paper-table").count(), 12, "rings reader must render all 12 tables");
  assertAtLeast(await page.locator("article#paper-manuscript svg").count(), 11, "rings reader must render 11 SVGs");
  assertAtLeast(await page.locator(".paper-block").count(), 90, "rings reader must render formal theorem/proof blocks");
  assertEqual(await page.locator(".paper-algorithm").count(), 4, "rings reader must render all 4 algorithms");
  assertAtLeast(await page.locator(".paper-bib-item").count(), 55, "rings reader must render cited bibliography");

  await waitForBeacon(
    running.beacons,
    (b): boolean => b.event === "paper_read" && b.paper === "rings" && b.mode === "web",
    "rings reader must emit paper_read(mode=web)",
  );

  // Scroll to bottom to trigger 25/50/75/100% progress beacons
  await page.evaluate((): void => {
    window.scrollTo(0, document.body.scrollHeight);
    window.dispatchEvent(new Event("scroll"));
  });
  await waitForBeacon(
    running.beacons,
    (b): boolean => b.event === "paper_read_progress" && b.paper === "rings" && b.depth === 100,
    "scrolling to the end of rings paper must emit depth=100 progress",
  );

  // Cite BibTeX button
  await page.locator('[data-paper-cite="rings"]').click();
  assertEqual(await page.locator("#bibtex-rings").isHidden(), false, "clicking Cite BibTeX must reveal BibTeX box");
  await waitForBeacon(
    running.beacons,
    (b): boolean => b.event === "paper_cite_copy" && b.paper === "rings",
    "clicking Cite BibTeX must emit paper_cite_copy",
  );

  // Switch to Embedded PDF preview
  await page.locator("#btn-mode-pdf").click();
  assertEqual(await page.locator("#paper-pdf-pane").isHidden(), false, "PDF mode must show embedded PDF pane");
  const frameSrc = (await page.locator("#paper-pdf-frame").getAttribute("src")) ?? "";
  if (!frameSrc.includes("/papers/rings.pdf?mode=preview")) {
    throw new Error(`unexpected embedded PDF iframe src: ${frameSrc}`);
  }
  await waitForBeacon(
    running.beacons,
    (b): boolean => b.event === "paper_pdf_preview" && b.paper === "rings",
    "switching to Embedded PDF must emit paper_pdf_preview",
  );

  // Trigger Download PDF button
  const downloadPromise = page.waitForEvent("download");
  await page.locator('.paper-toolbar-actions [data-paper-download="rings"][data-paper-format="pdf"]').click();
  const download = await downloadPromise;
  assertEqual(download.suggestedFilename(), "rings-whitepaper-1.0.pdf", "PDF download must use canonical filename");
  await waitForBeacon(
    running.beacons,
    (b): boolean => b.event === "paper_download" && b.paper === "rings" && b.format === "pdf",
    "clicking Download PDF must emit paper_download(format=pdf)",
  );

  // 3. Verify `/papers/dranking/` companion paper reader
  await page.goto(`${running.origin}/papers/dranking/`, { waitUntil: "load" });
  assertEqual(await page.locator("article#paper-manuscript h1").count(), 1, "dranking reader must render one h1");
  assertEqual(await page.locator(".paper-table").count(), 2, "dranking reader must render 2 tables");
  assertEqual(await page.locator(".paper-algorithm").count(), 2, "dranking reader must render 2 algorithms");
  await waitForBeacon(
    running.beacons,
    (b): boolean => b.event === "paper_read" && b.paper === "dranking" && b.mode === "web",
    "dranking reader must emit paper_read(mode=web)",
  );

  assertEqual(pageErrors.length, 0, `paper reader pages raised browser errors: ${pageErrors.join("; ")}`);
  await context.close();
  console.log(
    `Paper hub & reader browser checks passed (${String(running.beacons.length)} telemetry beacons verified)`,
  );
} finally {
  await browser.close();
  await closeServer(running.server);
  clearTimeout(watchdog);
}

/** Resolves the `frontend/` directory from either source or compiled `.generated/scripts` paths. */
function frontendProjectRoot(currentScriptDir: string): string {
  const parentDir = dirname(currentScriptDir);
  return basename(parentDir) === ".generated" ? resolve(parentDir, "..") : resolve(currentScriptDir, "..");
}

/** Starts an ephemeral loopback server serving static site assets and mock telemetry endpoints. */
async function startTestServer(root: string): Promise<RunningServer> {
  const beacons: TelemetryPayload[] = [];
  const server = createServer((request: IncomingMessage, response: ServerResponse): void => {
    void handleRequest(root, beacons, request, response);
  });
  await new Promise<void>((resolveStarted, rejectStarted): void => {
    server.once("error", rejectStarted);
    server.listen(0, "127.0.0.1", resolveStarted);
  });
  const address = server.address();
  if (!address || typeof address === "string") {
    await closeServer(server);
    throw new Error("paper test server did not expose a TCP address");
  }
  return {
    server,
    origin: `http://127.0.0.1:${String(address.port)}`,
    beacons,
  };
}

/** Handles incoming static asset requests as well as `/api/telemetry` and `/api/telemetry/stats`. */
async function handleRequest(
  root: string,
  beacons: TelemetryPayload[],
  request: IncomingMessage,
  response: ServerResponse,
): Promise<void> {
  const url = new URL(request.url ?? "/", "http://localhost");
  if (url.pathname === "/api/telemetry" && request.method === "POST") {
    const chunks: Buffer[] = [];
    for await (const chunk of request) {
      chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk)));
    }
    const raw = Buffer.concat(chunks).toString("utf-8");
    beacons.push(JSON.parse(raw) as TelemetryPayload);
    response.writeHead(202, { "Cache-Control": "no-store" }).end();
    return;
  }
  if (url.pathname === "/api/telemetry/stats" && request.method === "GET") {
    response.writeHead(200, { "Content-Type": "application/json; charset=utf-8" }).end(
      JSON.stringify({
        ok: true,
        hub: { views: 19 },
        papers: {
          rings: { reads: 128, downloads: 42 },
          dranking: { reads: 64, downloads: 21 },
        },
      }),
    );
    return;
  }

  try {
    const cleanPath = decodeURIComponent(url.pathname.slice(1));
    let filePath = resolve(root, cleanPath || "index.html");
    if (filePath !== root && !filePath.startsWith(`${root}${sep}`)) {
      response.writeHead(400).end("invalid path");
      return;
    }
    const st = await stat(filePath).catch(() => null);
    if (st?.isDirectory()) {
      filePath = join(filePath, "index.html");
    }
    const body = await readFile(filePath);
    response.writeHead(200, { "Content-Type": contentType(filePath) }).end(body);
  } catch {
    response.writeHead(404).end("not found");
  }
}

/** Maps a served file path extension to its HTTP `Content-Type` header. */
function contentType(filePath: string): string {
  switch (extname(filePath)) {
    case ".css":
      return "text/css; charset=utf-8";
    case ".html":
      return "text/html; charset=utf-8";
    case ".js":
      return "text/javascript; charset=utf-8";
    case ".md":
      return "text/markdown; charset=utf-8";
    case ".pdf":
      return "application/pdf";
    case ".svg":
      return "image/svg+xml";
    default:
      return "application/octet-stream";
  }
}

/** Polls collected telemetry beacons until `predicate` matches or the 3s deadline expires. */
async function waitForBeacon(
  beacons: TelemetryPayload[],
  predicate: (payload: TelemetryPayload) => boolean,
  message: string,
): Promise<void> {
  const deadline = Date.now() + 3_000;
  while (Date.now() < deadline) {
    if (beacons.some(predicate)) {
      return;
    }
    await new Promise<void>((r): void => {
      setTimeout(r, 25);
    });
  }
  throw new Error(`${message}; received beacons: ${JSON.stringify(beacons)}`);
}

/** Closes the loopback HTTP server cleanly. */
async function closeServer(server: Server): Promise<void> {
  await new Promise<void>((resolveClosed, rejectClosed): void => {
    server.close((error?: Error): void => (error ? rejectClosed(error) : resolveClosed()));
  });
}

/** Asserts strict equality on a scalar test expectation. */
function assertEqual<T>(actual: T, expected: T, message: string): void {
  if (actual !== expected) {
    throw new Error(`${message}: expected ${String(expected)}, received ${String(actual)}`);
  }
}

/** Asserts that a measured numeric count is at least `minimum`. */
function assertAtLeast(actual: number, minimum: number, message: string): void {
  if (actual < minimum) {
    throw new Error(`${message}: expected at least ${String(minimum)}, received ${String(actual)}`);
  }
}
