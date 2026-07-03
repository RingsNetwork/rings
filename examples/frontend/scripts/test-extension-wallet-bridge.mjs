#!/usr/bin/env node

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";
import { chromium } from "playwright";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(scriptDir, "..");
const extensionPath = resolve(projectRoot, process.argv[2] ?? "dist-extension");
const fixtureRoot = resolve(projectRoot, "test-pages");

const server = await serveFixture(fixtureRoot);
const userDataDir = await mkdtemp(join(tmpdir(), "rings-node-extension-"));

let context;
try {
  context = await chromium.launchPersistentContext(userDataDir, {
    headless: process.env.HEADLESS === "1",
    args: [
      `--disable-extensions-except=${extensionPath}`,
      `--load-extension=${extensionPath}`,
    ],
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
  await extensionPage.waitForFunction(() => Boolean(globalThis.RingsExtensionWalletBridge));
  await extensionPage.waitForFunction(() => Boolean(globalThis.RingsExtensionNodeBridge));

  await fixturePage.bringToFront();
  await fixturePage.waitForTimeout(250);

  await chooseEip191Wallet(fixturePage, "metamask");
  const nodeStartPromise = extensionPage.evaluate(async () => {
    try {
      return {
        ok: true,
        value: await globalThis.RingsExtensionNodeBridge.start({
          walletKind: "eip191",
          networkId: "1",
          iceServers: "stun://stun.l.google.com:19302",
          stabilizeInterval: "3",
          storageName: "rings-frontend-wallet-fixture",
          seedUrl: "",
        }),
      };
    } catch (error) {
      return {
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  });
  const nodeStart = await nodeStartPromise;
  assert.equal(nodeStart.ok, true);
  assert.equal(nodeStart.value.starting, true);
  await waitForFixtureCall(fixturePage, "browser-selector", "eth_requestAccounts");
  assert.equal(await extensionPage.locator("#rings-extension-provider-chooser").count(), 0);
  await extensionPage.evaluate(() => globalThis.RingsExtensionNodeBridge.stop().catch(() => {}));
  await clearFixtureCalls(fixturePage);

  await chooseEip191Wallet(fixturePage, "phantom");
  const rejectedConnectPromise = extensionPage.evaluate(async () => {
    try {
      return {
        ok: true,
        value: await globalThis.RingsExtensionWalletBridge.connect("eip191"),
      };
    } catch (error) {
      return {
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  });
  assert.equal(await fixturePage.locator("#rings-eip191-provider-chooser").count(), 0);
  const rejectedConnect = await rejectedConnectPromise;
  assert.equal(rejectedConnect.ok, false);
  assert.match(rejectedConnect.error, /Phantom request rejected/);
  assert.equal(await extensionPage.locator("#rings-extension-provider-chooser").count(), 0);

  await fixturePage.bringToFront();
  await fixturePage.waitForTimeout(250);
  await chooseEip191Wallet(fixturePage, "metamask");
  const eip191ConnectPromise = extensionPage.evaluate(() =>
    globalThis.RingsExtensionWalletBridge.connect("eip191"),
  );
  assert.equal(await fixturePage.locator("#rings-eip191-provider-chooser").count(), 0);
  const eip191Connect = await eip191ConnectPromise;

  const eip191Sign = await extensionPage.evaluate(() =>
    globalThis.RingsExtensionWalletBridge.sign(
      "eip191",
      "rings test proof",
      "0x1234567890abcdef1234567890abcdef12345678",
    ),
  );

  const ed25519Connect = await extensionPage.evaluate(() =>
    globalThis.RingsExtensionWalletBridge.connect("ed25519"),
  );
  const ed25519Sign = await extensionPage.evaluate(() =>
    globalThis.RingsExtensionWalletBridge.sign("ed25519", "rings test proof"),
  );

  await fixturePage.bringToFront();
  await fixturePage.waitForTimeout(250);
  await chooseEip191Wallet(fixturePage, "metamask");
  const legacyEip191ConnectPromise = extensionPage.evaluate(() =>
    globalThis.RingsExtensionWalletBridge.connect("metamask"),
  );
  const legacyEip191Connect = await legacyEip191ConnectPromise;

  assert.equal(
    eip191Connect.account,
    "0x1234567890abcdef1234567890abcdef12345678",
  );
  assert.equal(eip191Connect.accountType, "eip191");
  assert.equal(eip191Sign.signature, "0x00112233445566778899aabbccddeeff");
  assert.equal(ed25519Connect.account, "Bridge1111111111111111111111111111111111");
  assert.equal(ed25519Connect.accountType, "ed25519");
  assert.equal(
    legacyEip191Connect.account,
    "0x1234567890abcdef1234567890abcdef12345678",
  );
  assert.deepEqual(
    ed25519Sign.signature,
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
  );

  const calls = await fixturePage.locator("#calls").textContent();
  assert.match(calls ?? "", /phantom-evm/);
  assert.match(calls ?? "", /personal_sign/);
  assert.match(calls ?? "", /signMessage/);

  console.log("Extension wallet bridge fixture passed");
} finally {
  await context?.close();
  await rm(userDataDir, { force: true, recursive: true });
  await new Promise((resolve) => server.close(resolve));
}

async function chooseEip191Wallet(page, wallet) {
  await page.evaluate((nextWallet) => {
    window.__ringsFixtureChooseEip191Wallet(nextWallet);
  }, wallet);
}

async function waitForFixtureCall(page, wallet, method) {
  await page.waitForFunction(
    ({ wallet, method }) => {
      const text = document.querySelector("#calls")?.textContent ?? "[]";
      return JSON.parse(text).some((call) => {
        const payloadMethod = call.payload?.method ?? call.method;
        return call.wallet === wallet && payloadMethod === method;
      });
    },
    { wallet, method },
    { timeout: 10000 },
  );
}

async function clearFixtureCalls(page) {
  await page.evaluate(() => {
    const calls = document.querySelector("#calls");
    if (calls) {
      calls.textContent = "";
    }
  });
}

function serveFixture(root) {
  const mimeTypes = new Map([
    [".html", "text/html; charset=utf-8"],
    [".js", "text/javascript; charset=utf-8"],
    [".css", "text/css; charset=utf-8"],
  ]);
  const server = createServer(async (request, response) => {
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
    } catch (error) {
      response.writeHead(404).end(String(error));
    }
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      resolve({
        close: (callback) => server.close(callback),
        port: address.port,
      });
    });
  });
}
