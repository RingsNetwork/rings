#!/usr/bin/env node

/**
 * Runs a local Playwright fixture that validates the packaged extension wallet bridge.
 */

import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { basename, dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { type BrowserContext, chromium, type Page } from "playwright";

/**
 * Wallet kinds exercised by the extension bridge fixture.
 */
type WalletKind = "eip191" | "metamask" | "ed25519";

/**
 * Minimal settings sent to the extension node bridge during the fixture.
 */
type NodeStartSettings = {
  readonly walletKind: WalletKind;
  readonly networkId: string;
  readonly iceServers: string;
  readonly stabilizeInterval: string;
  readonly storageName: string;
  readonly seedUrl: string;
};

/**
 * Snapshot fields asserted after starting the retained extension node.
 */
type NodeSnapshot = {
  readonly starting?: boolean;
  readonly [key: string]: unknown;
};

/**
 * Normalized wallet result shape returned by the extension bridge.
 */
type WalletConnectResult = {
  readonly account: string;
  readonly accountType: string;
  readonly signature?: string | readonly number[];
};

/**
 * Result envelope used when page evaluation catches an expected failure.
 */
type Attempt<T> =
  | {
      readonly ok: true;
      readonly value: T;
    }
  | {
      readonly ok: false;
      readonly error: string;
    };

/**
 * Global bridge objects expected to exist inside the extension page.
 */
type ExtensionPageGlobal = typeof globalThis & {
  readonly RingsExtensionNodeBridge: {
    start(settings: NodeStartSettings): Promise<NodeSnapshot>;
    stop(): Promise<unknown>;
  };
  readonly RingsExtensionWalletBridge: {
    connect(wallet: WalletKind): Promise<WalletConnectResult>;
    sign(wallet: WalletKind, proof: string, account?: string): Promise<WalletConnectResult>;
  };
};

/**
 * Fixture-page helper exposed by wallet-fixture.html.
 */
type FixtureWindow = Window & {
  __ringsFixtureChooseEip191Wallet(wallet: string): void;
};

/**
 * Local HTTP fixture server handle.
 */
type FixtureServer = {
  readonly close: (callback: () => void) => void;
  readonly port: number;
};

/**
 * Recorded wallet-provider call shape rendered by the fixture page.
 */
type FixtureCall = {
  readonly wallet?: unknown;
  readonly method?: unknown;
  readonly payload?: {
    readonly method?: unknown;
  };
};

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const extensionPath = resolve(projectRoot, process.argv[2] ?? "dist-extension");
const fixtureRoot = resolve(projectRoot, "test-pages");
const { HEADLESS: headlessMode } = process.env;

const server = await serveFixture(fixtureRoot);
const userDataDir = await mkdtemp(join(tmpdir(), "rings-node-extension-"));

let context: BrowserContext | undefined;
try {
  context = await chromium.launchPersistentContext(userDataDir, {
    headless: headlessMode === "1",
    args: [`--disable-extensions-except=${extensionPath}`, `--load-extension=${extensionPath}`],
  });

  let serviceWorker = context.serviceWorkers()[0];
  if (!serviceWorker) {
    serviceWorker = await context.waitForEvent("serviceworker", { timeout: 10000 });
  }
  const extensionId = new URL(serviceWorker.url()).host;

  const fixturePage = await context.newPage();
  await fixturePage.goto(`http://127.0.0.1:${server.port}/wallet-fixture.html`);

  const extensionPage = await context.newPage();
  await extensionPage.goto(`chrome-extension://${extensionId}/index.html`);
  await extensionPage.waitForFunction((): boolean =>
    Boolean((globalThis as ExtensionPageGlobal).RingsExtensionWalletBridge),
  );
  await extensionPage.waitForFunction((): boolean =>
    Boolean((globalThis as ExtensionPageGlobal).RingsExtensionNodeBridge),
  );

  await fixturePage.bringToFront();
  await fixturePage.waitForTimeout(250);

  await chooseEip191Wallet(fixturePage, "metamask");
  const nodeStartPromise = extensionPage.evaluate(async (): Promise<Attempt<NodeSnapshot>> => {
    try {
      return {
        ok: true,
        value: await (globalThis as ExtensionPageGlobal).RingsExtensionNodeBridge.start({
          walletKind: "eip191",
          networkId: "1",
          iceServers: "stun://stun.l.google.com:19302",
          stabilizeInterval: "1",
          storageName: "rings-frontend-wallet-fixture",
          seedUrl: "",
        }),
      };
    } catch (error: unknown) {
      return {
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  });
  const nodeStart = await nodeStartPromise;
  if (!nodeStart.ok) {
    throw new Error(nodeStart.error);
  }
  assert.equal(nodeStart.value.starting, true);
  await waitForFixtureCall(fixturePage, "browser-selector", "eth_requestAccounts");
  assert.equal(await extensionPage.locator("#rings-extension-provider-chooser").count(), 0);
  await extensionPage.evaluate(
    (): Promise<unknown> => (globalThis as ExtensionPageGlobal).RingsExtensionNodeBridge.stop().catch((): null => null),
  );
  await clearFixtureCalls(fixturePage);

  await chooseEip191Wallet(fixturePage, "phantom");
  const rejectedConnectPromise = extensionPage.evaluate(async (): Promise<Attempt<WalletConnectResult>> => {
    try {
      return {
        ok: true,
        value: await (globalThis as ExtensionPageGlobal).RingsExtensionWalletBridge.connect("eip191"),
      };
    } catch (error: unknown) {
      return {
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  });
  assert.equal(await fixturePage.locator("#rings-eip191-provider-chooser").count(), 0);
  const rejectedConnect = await rejectedConnectPromise;
  assert.equal(rejectedConnect.ok, false);
  if (rejectedConnect.ok) {
    throw new Error("expected rejected EIP-191 connection");
  }
  assert.match(rejectedConnect.error, /Phantom request rejected/);
  assert.equal(await extensionPage.locator("#rings-extension-provider-chooser").count(), 0);

  await fixturePage.bringToFront();
  await fixturePage.waitForTimeout(250);
  await chooseEip191Wallet(fixturePage, "metamask");
  const eip191ConnectPromise = extensionPage.evaluate(
    (): Promise<WalletConnectResult> =>
      (globalThis as ExtensionPageGlobal).RingsExtensionWalletBridge.connect("eip191"),
  );
  assert.equal(await fixturePage.locator("#rings-eip191-provider-chooser").count(), 0);
  const eip191Connect = await eip191ConnectPromise;

  const eip191Sign = await extensionPage.evaluate(
    (): Promise<WalletConnectResult> =>
      (globalThis as ExtensionPageGlobal).RingsExtensionWalletBridge.sign(
        "eip191",
        "rings test proof",
        "0x1234567890abcdef1234567890abcdef12345678",
      ),
  );

  const ed25519Connect = await extensionPage.evaluate(
    (): Promise<WalletConnectResult> =>
      (globalThis as ExtensionPageGlobal).RingsExtensionWalletBridge.connect("ed25519"),
  );
  const ed25519Sign = await extensionPage.evaluate(
    (): Promise<WalletConnectResult> =>
      (globalThis as ExtensionPageGlobal).RingsExtensionWalletBridge.sign("ed25519", "rings test proof"),
  );

  await fixturePage.bringToFront();
  await fixturePage.waitForTimeout(250);
  await chooseEip191Wallet(fixturePage, "metamask");
  const legacyEip191ConnectPromise = extensionPage.evaluate(
    (): Promise<WalletConnectResult> =>
      (globalThis as ExtensionPageGlobal).RingsExtensionWalletBridge.connect("metamask"),
  );
  const legacyEip191Connect = await legacyEip191ConnectPromise;

  assert.equal(eip191Connect.account, "0x1234567890abcdef1234567890abcdef12345678");
  assert.equal(eip191Connect.accountType, "eip191");
  assert.equal(eip191Sign.signature, "0x00112233445566778899aabbccddeeff");
  assert.equal(ed25519Connect.account, "Bridge1111111111111111111111111111111111");
  assert.equal(ed25519Connect.accountType, "ed25519");
  assert.equal(legacyEip191Connect.account, "0x1234567890abcdef1234567890abcdef12345678");
  assert.deepEqual(ed25519Sign.signature, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);

  const calls = await fixturePage.locator("#calls").textContent();
  assert.match(calls ?? "", /phantom-evm/);
  assert.match(calls ?? "", /personal_sign/);
  assert.match(calls ?? "", /signMessage/);

  console.log("Extension wallet bridge fixture passed");
} finally {
  await context?.close();
  await rm(userDataDir, { force: true, recursive: true });
  await new Promise<void>((resolveClose): void => server.close(resolveClose));
}

/**
 * Resolves the frontend project root from either source or generated script paths.
 */
function frontendProjectRoot(currentScriptDir: string): string {
  const parentDir = dirname(currentScriptDir);
  if (basename(parentDir) === ".generated") {
    return resolve(parentDir, "..");
  }
  return resolve(currentScriptDir, "..");
}

/**
 * Selects which mock EIP-191 provider the fixture page should expose next.
 */
async function chooseEip191Wallet(page: Page, wallet: string): Promise<void> {
  await page.evaluate((nextWallet: string): void => {
    (window as unknown as FixtureWindow).__ringsFixtureChooseEip191Wallet(nextWallet);
  }, wallet);
}

/**
 * Waits until the fixture page records a specific wallet method call.
 */
async function waitForFixtureCall(page: Page, wallet: string, method: string): Promise<void> {
  await page.waitForFunction(
    ({ wallet, method }: { readonly wallet: string; readonly method: string }): boolean => {
      const text = document.querySelector("#calls")?.textContent ?? "[]";
      const parsed = JSON.parse(text) as unknown;
      if (!Array.isArray(parsed)) {
        return false;
      }
      return parsed.some((rawCall: unknown): boolean => {
        const call = rawCall as FixtureCall;
        const payloadMethod = call.payload?.method ?? call.method;
        return call.wallet === wallet && payloadMethod === method;
      });
    },
    { wallet, method },
    { timeout: 10000 },
  );
}

/**
 * Clears the fixture page call log between scenario phases.
 */
async function clearFixtureCalls(page: Page): Promise<void> {
  await page.evaluate((): void => {
    const calls = document.querySelector("#calls");
    if (calls) {
      calls.textContent = "";
    }
  });
}

/**
 * Serves wallet-fixture.html from 127.0.0.1 for active-tab wallet injection.
 */
function serveFixture(root: string): Promise<FixtureServer> {
  const mimeTypes = new Map<string, string>([
    [".html", "text/html; charset=utf-8"],
    [".js", "text/javascript; charset=utf-8"],
    [".css", "text/css; charset=utf-8"],
  ]);
  const server: Server = createServer(async (request, response): Promise<void> => {
    try {
      const url = new URL(request.url ?? "/", "http://127.0.0.1");
      const pathname = url.pathname === "/" ? "/wallet-fixture.html" : url.pathname;
      const filePath = resolve(root, `.${pathname}`);
      if (!filePath.startsWith(root)) {
        response.writeHead(403).end("forbidden");
        return;
      }
      const body = await readFile(filePath);
      response.writeHead(200, {
        "content-type": mimeTypes.get(extname(filePath)) ?? "application/octet-stream",
      });
      response.end(body);
    } catch (error: unknown) {
      response.writeHead(404).end(String(error));
    }
  });
  return new Promise<FixtureServer>((resolveServer, reject): void => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", (): void => {
      const address = server.address();
      if (!isAddressInfo(address)) {
        reject(new Error("fixture server did not expose a TCP port"));
        return;
      }
      resolveServer({
        close: (callback: () => void): void => {
          server.close(callback);
        },
        port: address.port,
      });
    });
  });
}

/**
 * Narrows a Node server address to a TCP address with a port.
 */
function isAddressInfo(address: AddressInfo | string | null): address is AddressInfo {
  return typeof address === "object" && address !== null && "port" in address;
}
