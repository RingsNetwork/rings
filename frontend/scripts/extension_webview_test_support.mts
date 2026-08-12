/**
 * Shared types and small host-side helpers for the Extension WebView browser fixture.
 */

import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { type BrowserContext, chromium, type Page } from "playwright";

/** Bridge request shape captured by the test fixture. */
export type FixtureGatewayRequest = {
  readonly requested?: unknown;
  readonly sourceTarget?: unknown;
  readonly method?: unknown;
  readonly kind?: unknown;
  readonly topLevelNavigation?: unknown;
  readonly body?: unknown;
  readonly credentials?: unknown;
  readonly headers?: readonly { readonly name?: unknown; readonly value?: unknown }[];
};

/** Normalized request witness retained by the deterministic bridge fixture. */
export type FixtureGatewayRecord = {
  readonly target: string;
  readonly sourceTarget?: string;
  readonly kind: string;
  readonly topLevelNavigation: boolean;
};

/** Credential-bearing request record used by focused browser regressions. */
export type FixtureCredentialRecord = {
  readonly target: string;
  readonly kind: string;
  readonly credentials: string;
};

/** One serializable response owned by the deterministic extension gateway. */
export type FixtureRoute = {
  readonly body: string;
  readonly contentType?: string;
  readonly delayMs?: number;
  readonly errorOnHeaderNames?: readonly string[];
  readonly headers?: readonly { readonly name: string; readonly value: string }[];
  readonly status?: number;
};

/** Extension page global used to open and mock the WebView bridge. */
export type FixtureExtensionGlobal = typeof globalThis & {
  __fixtureWebviewRequests?: string[];
  __fixtureWebviewRecords?: FixtureGatewayRecord[];
  __fixtureCredentialRecords?: FixtureCredentialRecord[];
  RingsExtensionNodeBridge: {
    start(settings: {
      readonly walletKind: "webcrypto";
      readonly networkId: string;
      readonly iceServers: string;
      readonly stabilizeInterval: string;
      readonly storageName: string;
      readonly seedUrl: string;
    }): Promise<{ readonly online?: boolean; readonly starting?: boolean; readonly error?: string }>;
    status(): Promise<{ readonly online?: boolean; readonly starting?: boolean; readonly error?: string }>;
    stop(): Promise<unknown>;
    openWebview(): Promise<unknown>;
    webviewRequest(request: FixtureGatewayRequest): Promise<unknown>;
  };
};

/** Minimal session-rule shape inspected inside the extension page. */
export type FixtureSessionRule = {
  readonly action: { readonly type: string };
  readonly condition: {
    readonly tabIds?: readonly number[];
    readonly regexFilter?: string;
    readonly resourceTypes?: readonly string[];
  };
};

/** Owned browser process and extension launcher used by a Chromium fixture. */
export type ExtensionWebviewHarness = {
  readonly context: BrowserContext;
  readonly launcher: Page;
  readonly close: () => Promise<void>;
};

/** Launches one isolated unpacked extension profile and waits for its bridge. */
export async function launchExtensionWebview(extensionPath: string): Promise<ExtensionWebviewHarness> {
  const userDataDir = await mkdtemp(join(tmpdir(), "rings-webview-extension-"));
  let context: BrowserContext | undefined;
  try {
    context = await chromium.launchPersistentContext(userDataDir, {
      headless: false,
      args: [`--disable-extensions-except=${extensionPath}`, `--load-extension=${extensionPath}`],
    });
    const serviceWorker =
      context.serviceWorkers()[0] ?? (await context.waitForEvent("serviceworker", { timeout: 10_000 }));
    const extensionId = new URL(serviceWorker.url()).host;
    const launcher = await context.newPage();
    await launcher.goto(`chrome-extension://${extensionId}/index.html`);
    await launcher.waitForFunction((): boolean =>
      Boolean((globalThis as FixtureExtensionGlobal).RingsExtensionNodeBridge),
    );
    return {
      context,
      launcher,
      close: async (): Promise<void> => {
        await context?.close();
        await rm(userDataDir, { force: true, recursive: true });
      },
    };
  } catch (error: unknown) {
    await context?.close();
    await rm(userDataDir, { force: true, recursive: true });
    throw error;
  }
}

/** Opens the packaged onion WebView and waits for its privileged bridge. */
export async function openExtensionWebview(harness: ExtensionWebviewHarness): Promise<Page> {
  const popupPromise = harness.context.waitForEvent("page", {
    predicate: (page: Page): boolean => page.url().includes("/webview.html"),
    timeout: 10_000,
  });
  await harness.launcher.evaluate(
    (): Promise<unknown> => (globalThis as FixtureExtensionGlobal).RingsExtensionNodeBridge.openWebview(),
  );
  const popup = await popupPromise;
  await popup.waitForLoadState("domcontentloaded");
  await popup.waitForFunction((): boolean => Boolean((globalThis as FixtureExtensionGlobal).RingsExtensionNodeBridge));
  return popup;
}

/** Exercises the packaged JS -> service worker -> offscreen WASM -> Rust gateway path. */
export async function verifyRealOffscreenGateway(page: Page): Promise<{
  readonly ok?: boolean;
  readonly status?: number;
  readonly errorCode?: string;
}> {
  return page.evaluate(
    async (): Promise<{ readonly ok?: boolean; readonly status?: number; readonly errorCode?: string }> => {
      const bridge = (globalThis as FixtureExtensionGlobal).RingsExtensionNodeBridge;
      await bridge.start({
        walletKind: "webcrypto",
        networkId: "1",
        iceServers: "stun://stun.l.google.com:19302",
        stabilizeInterval: "1",
        storageName: `rings-webview-real-bridge-${crypto.randomUUID()}`,
        seedUrl: "",
      });
      try {
        let snapshot = await bridge.status();
        for (let attempt = 0; attempt < 100 && snapshot.starting; attempt += 1) {
          await new Promise((resolve): void => {
            setTimeout(resolve, 50);
          });
          snapshot = await bridge.status();
        }
        if (!snapshot.online) throw new Error(snapshot.error ?? "real offscreen node did not become online");
        return (await bridge.webviewRequest({
          requested: "not-an-https-url",
          method: "GET",
          headers: [],
          body: [],
          credentials: "include",
          kind: "navigation",
          topLevelNavigation: true,
        })) as { readonly ok?: boolean; readonly status?: number; readonly errorCode?: string };
      } finally {
        await bridge.stop();
      }
    },
  );
}

/** Installs one canonical typed route-table gateway for all extension browser tests. */
export async function installExtensionGatewayFixture(
  page: Page,
  routes: Readonly<Record<string, FixtureRoute>>,
): Promise<void> {
  await page.evaluate((serializedRoutes: Readonly<Record<string, FixtureRoute>>): void => {
    const fixtureGlobal = globalThis as FixtureExtensionGlobal;
    fixtureGlobal.__fixtureWebviewRequests = [];
    fixtureGlobal.__fixtureWebviewRecords = [];
    fixtureGlobal.__fixtureCredentialRecords = [];
    fixtureGlobal.RingsExtensionNodeBridge.webviewRequest = async (
      request: FixtureGatewayRequest,
    ): Promise<unknown> => {
      const target = fixtureGatewayTarget(request.requested);
      const kind = typeof request.kind === "string" ? request.kind : "unknown";
      fixtureGlobal.__fixtureWebviewRequests?.push(`${kind} ${target}`);
      fixtureGlobal.__fixtureWebviewRecords?.push({
        target,
        ...(typeof request.sourceTarget === "string" ? { sourceTarget: request.sourceTarget } : {}),
        kind,
        topLevelNavigation: request.topLevelNavigation === true,
      });
      fixtureGlobal.__fixtureCredentialRecords?.push({
        target,
        kind,
        credentials: typeof request.credentials === "string" ? request.credentials : "unknown",
      });
      const route = serializedRoutes[target];
      if (!route) throw new Error(`unexpected extension fixture target ${target}`);
      const rejectedHeaders = new Set(route.errorOnHeaderNames?.map((name: string): string => name.toLowerCase()));
      const leaked = request.headers?.find(
        (header): boolean => typeof header.name === "string" && rejectedHeaders.has(header.name.toLowerCase()),
      );
      if (leaked?.name) throw new Error(`fixture request retained forbidden header ${leaked.name}`);
      if (route.delayMs) {
        await new Promise((resolve): void => {
          setTimeout(resolve, route.delayMs);
        });
      }
      const headers = route.headers ?? (route.contentType ? [{ name: "Content-Type", value: route.contentType }] : []);
      return {
        ok: true,
        status: route.status ?? 200,
        headers,
        body: Array.from(new TextEncoder().encode(route.body)),
      };
    };

    /** Decodes only the controlled-origin route emitted by the fixture host. */
    function fixtureGatewayTarget(value: unknown): string {
      if (typeof value !== "string") throw new Error("fixture gateway request has no route");
      const marker = "/webview/";
      const markerIndex = value.indexOf(marker);
      if (markerIndex < 0) throw new Error(`fixture gateway request is outside ${marker}`);
      return decodeURIComponent(value.slice(markerIndex + marker.length));
    }
  }, routes);
}

/** Counts completed fixture navigation calls across fresh renderer realms. */
export async function fixtureNavigationCount(page: Page): Promise<number> {
  return page.evaluate(
    (): number =>
      ((globalThis as FixtureExtensionGlobal).__fixtureWebviewRequests ?? []).filter((entry: string): boolean =>
        entry.startsWith("navigation "),
      ).length,
  );
}

/** Counts requests to one exact onion target in the shared extension fixture. */
export async function fixtureRequestCount(page: Page, target: string): Promise<number> {
  return page.evaluate(
    (expectedTarget: string): number =>
      ((globalThis as FixtureExtensionGlobal).__fixtureCredentialRecords ?? []).filter(
        (record: FixtureCredentialRecord): boolean => record.target === expectedTarget,
      ).length,
    target,
  );
}

/** Builds the exact shared bootstrap sources with the Extension worker capability enabled. */
export async function browserBootstrapFixture(frontendRoot: string): Promise<string> {
  const webviewRoot = resolve(frontendRoot, "..", "crates", "webview", "src");
  const [transforms, runtime] = await Promise.all([
    readFile(join(webviewRoot, "browser_runtime_transforms.mjs"), "utf8"),
    readFile(join(webviewRoot, "browser_runtime.mjs"), "utf8"),
  ]);
  const config = JSON.stringify({
    prefix: "/webview/",
    targetBase: "https://fixture.example/",
    marker: "__ringsWebviewGateway",
    blockWorkers: false,
    delegateNavigation: true,
  });
  return `globalThis.__ringsWebviewBootstrapConfig=${config};\n${transforms}\n${runtime}`;
}

/** Resolves the frontend root from source or generated-script execution. */
export function frontendProjectRoot(currentScriptDir: string): string {
  const parentDir = dirname(currentScriptDir);
  return basename(parentDir) === ".generated" ? resolve(parentDir, "..") : resolve(currentScriptDir, "..");
}
