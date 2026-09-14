#!/usr/bin/env node

/**
 * Verifies the frontend's progressive-enhancement boundary in a real browser.
 */

import { readFile } from "node:fs/promises";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { basename, dirname, extname, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { type Browser, chromium } from "playwright";

/** A local static server and the origin assigned by the operating system. */
type RunningServer = {
  readonly server: Server;
  readonly origin: string;
};

/** Canonical project copy extracted from the initial document. */
type ProjectCopy = {
  readonly name: string;
  readonly tagline: string;
  readonly introduction: string;
};

const scriptDir = dirname(fileURLToPath(import.meta.url));
const frontendRoot = frontendProjectRoot(scriptDir);
const distRoot = resolve(frontendRoot, "dist");

const runningServer = await startStaticServer(distRoot);
const browser = await chromium.launch({ headless: true });
try {
  const projectCopy = await testStaticDocumentWithoutJavaScript(browser, runningServer.origin);
  await testYewReplacesStaticDocument(browser, runningServer.origin, projectCopy);
  console.log("Frontend static-shell browser checks passed");
} finally {
  await browser.close();
  await closeServer(runningServer.server);
}

/** Resolves the frontend root from either the source or generated scripts directory. */
function frontendProjectRoot(currentScriptDir: string): string {
  const parentDir = dirname(currentScriptDir);
  return basename(parentDir) === ".generated" ? resolve(parentDir, "..") : resolve(currentScriptDir, "..");
}

/** Starts an ephemeral loopback server rooted at the built frontend output. */
async function startStaticServer(root: string): Promise<RunningServer> {
  const server = createServer((request: IncomingMessage, response: ServerResponse): void => {
    void serveFile(root, request, response);
  });
  await new Promise<void>((resolveStarted, rejectStarted): void => {
    server.once("error", rejectStarted);
    server.listen(0, "127.0.0.1", resolveStarted);
  });
  const address = server.address();
  if (!address || typeof address === "string") {
    await closeServer(server);
    throw new Error("static-shell test server did not expose a TCP address");
  }
  return {
    server,
    origin: `http://127.0.0.1:${address.port}`,
  };
}

/** Serves one path from the build directory without allowing path traversal. */
async function serveFile(root: string, request: IncomingMessage, response: ServerResponse): Promise<void> {
  try {
    const pathname = new URL(request.url ?? "/", "http://localhost").pathname;
    const relativePath = pathname === "/" ? "index.html" : decodeURIComponent(pathname.slice(1));
    const filePath = resolve(root, relativePath);
    if (!filePath.startsWith(`${root}${sep}`)) {
      response.writeHead(400).end("invalid path");
      return;
    }
    const body = await readFile(filePath);
    response.writeHead(200, { "Content-Type": contentType(filePath) }).end(body);
  } catch {
    response.writeHead(404).end("not found");
  }
}

/** Returns the response media type required by the browser for a built asset. */
function contentType(filePath: string): string {
  switch (extname(filePath)) {
    case ".css":
      return "text/css; charset=utf-8";
    case ".html":
      return "text/html; charset=utf-8";
    case ".js":
      return "text/javascript; charset=utf-8";
    case ".svg":
      return "image/svg+xml";
    case ".wasm":
      return "application/wasm";
    default:
      return "application/octet-stream";
  }
}

/** Witnesses that the initial document remains substantive when scripts cannot run. */
async function testStaticDocumentWithoutJavaScript(browser: Browser, origin: string): Promise<ProjectCopy> {
  const context = await browser.newContext({ javaScriptEnabled: false });
  try {
    const page = await context.newPage();
    await page.goto(origin, { waitUntil: "load" });
    const staticRoot = page.locator("#static-project-introduction");
    await staticRoot.waitFor({ state: "visible" });
    assertEqual(await staticRoot.locator("h1").count(), 1, "the no-JS document must expose one project h1");
    const text = await staticRoot.innerText();
    assertAtLeast(text.split(/\s+/u).length, 120, "the no-JS document must expose substantive project copy");
    assertAtLeast(await staticRoot.locator("h2").count(), 2, "the no-JS document must expose project sections");
    assertAtLeast(await staticRoot.locator("h3").count(), 4, "the no-JS document must expose project features");
    return {
      name: await staticRoot.locator('[data-project-content="name"]').innerText(),
      tagline: await staticRoot.locator('[data-project-content="tagline"]').innerText(),
      introduction: await staticRoot.locator('[data-project-content="introduction"]').innerText(),
    };
  } finally {
    await context.close();
  }
}

/** Witnesses that a successful Yew mount replaces, rather than duplicates, the static shell. */
async function testYewReplacesStaticDocument(
  browser: Browser,
  origin: string,
  projectCopy: ProjectCopy,
): Promise<void> {
  // Service-worker registration intentionally reloads a first-time gateway host. Other suites
  // cover that lifecycle; this test isolates the Yew replacement boundary from that navigation.
  const context = await browser.newContext({ serviceWorkers: "block" });
  try {
    const page = await context.newPage();
    const pageErrors: string[] = [];
    page.on("pageerror", (error: Error): void => {
      pageErrors.push(error.message);
    });
    await page.goto(origin, { waitUntil: "load" });
    await page.locator("#landing-title").waitFor({ state: "visible" });
    assertEqual(await page.locator("#static-project-introduction").count(), 0, "Yew must replace the static shell");
    assertEqual(await page.locator("#landing-title").count(), 1, "the mounted app must expose one landing title");
    assertEqual(
      await page.locator(".landing-kicker").innerText(),
      projectCopy.name,
      "Yew must use the initial project name",
    );
    assertEqual(
      await page.locator("#landing-title").innerText(),
      projectCopy.tagline,
      "Yew must use the initial tagline",
    );
    assertEqual(
      await page.locator(".landing-lede").innerText(),
      projectCopy.introduction,
      "Yew must use the initial project introduction",
    );
    assertEqual(pageErrors.length, 0, `the mounted app raised browser errors: ${pageErrors.join("; ")}`);
  } finally {
    await context.close();
  }
}

/** Closes the local server and reports close failures. */
async function closeServer(server: Server): Promise<void> {
  await new Promise<void>((resolveClosed, rejectClosed): void => {
    server.close((error?: Error): void => (error ? rejectClosed(error) : resolveClosed()));
  });
}

/** Requires an exact scalar value at a test boundary. */
function assertEqual<T>(actual: T, expected: T, message: string): void {
  if (actual !== expected) {
    throw new Error(`${message}: expected ${String(expected)}, received ${String(actual)}`);
  }
}

/** Requires a measured structural value to meet its lower bound. */
function assertAtLeast(actual: number, minimum: number, message: string): void {
  if (actual < minimum) {
    throw new Error(`${message}: expected at least ${String(minimum)}, received ${String(actual)}`);
  }
}
