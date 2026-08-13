#!/usr/bin/env node

/** Focused Chromium regressions for recursive frames and module Worker state. */
import assert from "node:assert/strict";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  browserBootstrapFixture,
  type FixtureCredentialRecord,
  type FixtureExtensionGlobal,
  type FixtureRoute,
  fixtureRequestCount,
  frontendProjectRoot,
  installExtensionGatewayFixture,
  launchExtensionWebview,
  openExtensionWebview,
} from "./extension_webview_test_support.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const extensionPath = resolve(projectRoot, process.argv[2] ?? "dist-extension");
const fixtureBootstrap = (await browserBootstrapFixture(projectRoot)).replaceAll("</script", "<\\/script");
const dynamicSrcdoc = `<!doctype html><html><body><p id="dynamic-child">recursive renderer</p><script>
let result;
try {
  const peer = new RTCPeerConnection();
  peer.close();
  result = "native RTCPeerConnection constructed";
} catch (error) {
  result = error instanceof Error ? error.message : String(error);
}
parent.postMessage({ type: "rings.review.dynamic-frame", result }, "*");
</script></body></html>`;
const encodedSrcdoc = Buffer.from(dynamicSrcdoc, "utf8").toString("base64");
const rootHtml = `<!doctype html><html><head><title>Regression fixture</title></head><body>
  <h1>Extension WebView regressions</h1>
  <p id="dynamic-frame-result">pending</p>
  <p id="blank-frame-result">pending</p>
  <p id="worker-tla-result">pending</p>
  <p id="worker-omit-result">pending</p>
  <p id="worker-include-result">pending</p>
  <p id="worker-meta-result">pending</p>
  <p id="worker-cycle-result">pending</p>
  <p id="worker-forged-ready-result">pending</p>
  <p id="worker-body-limit-result">pending</p>
  <p id="page-body-limit-result">pending</p>
  <form id="post-override-form" method="get" action="./form-target">
    <input name="q" value="must-not-navigate">
    <button id="post-override-submit" type="submit" formmethod="post">Submit as POST</button>
  </form>
  <form id="get-navigation-form" method="get" action="./replacement">
    <input name="via" value="form">
    <button type="submit">Navigate once</button>
  </form>
  <div id="dynamic-frame-host"></div>
  <script data-rings-webview-bootstrap>${fixtureBootstrap}</script>
  <script>
  addEventListener("message", (event) => {
    if (event.data?.type === "rings.review.dynamic-frame") {
      document.getElementById("dynamic-frame-result").textContent = event.data.result;
    }
  });
  setTimeout(() => {
    const source = atob(${JSON.stringify(encodedSrcdoc)});
    const escaped = source.replaceAll("&", "&amp;").replaceAll('"', "&quot;");
    document.getElementById("dynamic-frame-host").innerHTML =
      '<iframe id="dynamic-frame" srcdoc="' + escaped + '"></iframe>';
    const adjacentDocument = new DOMParser().parseFromString(
      '<iframe id="adjacent-frame" srcdoc="' + escaped + '"></iframe>',
      "text/html",
    );
    document.getElementById("dynamic-frame-host").insertAdjacentElement(
      "afterend",
      adjacentDocument.querySelector("iframe"),
    );
    const blank = document.createElement("iframe");
    blank.id = "blank-frame";
    document.body.append(blank);
    try {
      void blank.contentWindow.RTCPeerConnection;
      document.getElementById("blank-frame-result").textContent = "native blank realm";
    } catch (_error) {
      document.getElementById("blank-frame-result").textContent = "isolated blank realm";
    }
    const attributeFrame = document.createElement("iframe");
    attributeFrame.id = "attribute-frame";
    const sourceObserver = new MutationObserver(() => {
      const source = attributeFrame.getAttributeNode("src");
      if (!source?.value.includes("webview_frame.html")) return;
      sourceObserver.disconnect();
      source.value = "https://fixture.example/attribute-frame";
    });
    sourceObserver.observe(attributeFrame, { attributes: true, attributeFilter: ["src"] });
    document.body.append(attributeFrame);
    const namedFrame = document.createElement("iframe");
    namedFrame.id = "named-frame";
    const namedSource = document.createAttribute("srcdoc");
    namedSource.value = '<p id="named-frame-result">NamedNodeMap stayed recursive</p>';
    namedFrame.attributes.setNamedItem(namedSource);
    document.body.append(namedFrame);
    const removedFrame = document.createElement("iframe");
    removedFrame.id = "removed-frame";
    removedFrame.srcdoc = '<p id="removed-source-leak">removed source executed</p>';
    removedFrame.removeAttribute("srcdoc");
    document.body.append(removedFrame);
  }, 25);

  const tla = new Worker("./worker-tla.js", { type: "module" });
  tla.onmessage = (event) => { document.getElementById("worker-tla-result").textContent = event.data; };
  tla.postMessage("queued through top-level await");

  for (const credentials of ["omit", "include"]) {
    const worker = new Worker("./worker-" + credentials + ".js", { type: "module", credentials });
    worker.onmessage = (event) => {
      document.getElementById("worker-" + credentials + "-result").textContent = event.data;
    };
  }

  const metadata = new Worker("./worker-meta.js", { type: "module" });
  metadata.onmessage = (event) => { document.getElementById("worker-meta-result").textContent = event.data; };

  const cycle = new Worker("./worker-cycle.js", { type: "module" });
  cycle.onmessage = (event) => {
    document.getElementById("worker-cycle-result").textContent = event.data;
  };
  cycle.onerror = (event) => {
    event.preventDefault();
    document.getElementById("worker-cycle-result").textContent = "error:" + event.message;
  };

  const guarded = new Worker("./worker-forged-ready.js", { type: "module" });
  const guardedMessages = [];
  guarded.onmessage = (event) => {
    guardedMessages.push(event.data?.type || String(event.data));
    document.getElementById("worker-forged-ready-result").textContent = guardedMessages.join("|");
  };
  guarded.postMessage("guarded queue");
  const bodyLimit = new Worker("./worker-body-limit.js");
  bodyLimit.onmessage = (event) => {
    document.getElementById("worker-body-limit-result").textContent = event.data;
    bodyLimit.terminate();
  };
  bodyLimit.postMessage("test bound");
  fetch("./page-upload", { method: "POST", body: new Uint8Array(8 * 1024 * 1024 + 1) })
    .then(() => { document.getElementById("page-body-limit-result").textContent = "oversized request reached gateway"; })
    .catch((error) => {
      document.getElementById("page-body-limit-result").textContent =
        error instanceof Error ? error.message : String(error);
    });
  new Worker("./worker-lifetime.js", { type: "module" });
  </script>
</body></html>`;
const routes: Readonly<Record<string, FixtureRoute>> = {
  "https://fixture.example/review": { body: rootHtml, contentType: "text/html; charset=utf-8" },
  "https://fixture.example/replacement": {
    body: "<!doctype html><html><body><h1>Replacement document</h1></body></html>",
    contentType: "text/html; charset=utf-8",
  },
  "https://fixture.example/replacement?via=form": {
    body: "<!doctype html><html><body><h1>Replacement document</h1></body></html>",
    contentType: "text/html; charset=utf-8",
  },
  "https://fixture.example/attribute-frame": {
    body: '<!doctype html><html><body><p id="attribute-frame-result">Attr mutation stayed recursive</p></body></html>',
    contentType: "text/html; charset=utf-8",
  },
  "https://fixture.example/worker-tla.js": {
    body: `await new Promise((resolve) => setTimeout(resolve, 250));
      self.onmessage = (event) => postMessage(event.data);`,
  },
  "https://fixture.example/worker-omit.js": {
    body: 'import { token } from "./worker-omit-dependency.js"; postMessage(token);',
  },
  "https://fixture.example/worker-omit-dependency.js": { body: 'export const token = "omit worker ready";' },
  "https://fixture.example/worker-include.js": {
    body: 'import { token } from "./worker-include-dependency.js"; postMessage(token);',
  },
  "https://fixture.example/worker-include-dependency.js": { body: 'export const token = "include worker ready";' },
  "https://fixture.example/worker-meta.js": {
    body: 'import.meta.reviewWitness = 7; postMessage(import.meta.url + "|" + import.meta.resolve("./asset.txt") + "|" + import.meta.reviewWitness + "|" + Object.isExtensible(import.meta) + "|" + (Object.getPrototypeOf(import.meta) === null));',
  },
  "https://fixture.example/worker-cycle.js": {
    body: 'import { left, readRight, setLeft } from "./worker-cycle-left.js"; import { readLeft } from "./worker-cycle-right.js"; setLeft(3); postMessage([left, readRight(), readLeft()].join("|"));',
  },
  "https://fixture.example/worker-cycle-left.js": {
    body: 'import { right } from "./worker-cycle-right.js"; export let left = 1; export const setLeft = (value) => { left = value; }; export const readRight = () => right;',
  },
  "https://fixture.example/worker-cycle-right.js": {
    body: 'import { left } from "./worker-cycle-left.js"; export const right = 2; export const readLeft = () => left;',
  },
  "https://fixture.example/worker-forged-ready.js": {
    body: `postMessage({ type: "rings.worker.ready" });
      await new Promise((resolve) => setTimeout(resolve, 250));
      self.onmessage = (event) => postMessage("echo:" + event.data);`,
  },
  "https://fixture.example/worker-body-limit.js": {
    body: `self.onmessage = async () => {
      try {
        await fetch("./worker-upload", { method: "POST", body: new Uint8Array(8 * 1024 * 1024 + 1) });
        postMessage("oversized request unexpectedly reached the gateway");
      } catch (error) {
        postMessage(error instanceof Error ? error.message : String(error));
      }
    };`,
  },
  "https://fixture.example/worker-lifetime.js": {
    body: 'setInterval(() => { void fetch("./worker-ping"); }, 25);',
  },
  "https://fixture.example/worker-ping": { body: "pong", contentType: "text/plain; charset=utf-8" },
};

const harness = await launchExtensionWebview(extensionPath);
try {
  const popup = await openExtensionWebview(harness);
  const pageErrors: string[] = [];
  const consoleMessages: string[] = [];
  popup.on("pageerror", (error: Error): void => {
    pageErrors.push(error.message);
  });
  popup.on("console", (message): void => {
    consoleMessages.push(`${message.type()}: ${message.text()}`);
  });
  await installExtensionGatewayFixture(popup, routes);
  await popup.locator("#webview-address").fill("https://fixture.example/review");
  await popup.locator("#webview-address-form").evaluate((form: HTMLFormElement): void => form.requestSubmit());
  const renderer = popup.frameLocator("#webview-frame");
  await renderer.locator("h1").waitFor({ timeout: 10_000 });
  await renderer.locator("#post-override-form").evaluate((form: HTMLFormElement): void => {
    const submitter = form.querySelector("button");
    if (!(submitter instanceof HTMLButtonElement)) throw new Error("POST override submitter is unavailable");
    form.requestSubmit(submitter);
  });
  await popup.waitForTimeout(100);
  assert.equal(await fixtureRequestCount(popup, "https://fixture.example/form-target"), 0);
  assert.equal(await renderer.locator("h1").textContent(), "Extension WebView regressions");

  await renderer.locator("#dynamic-frame-result").filter({ hasText: "blocked by Rings WebView" }).waitFor();
  assert.equal(
    await renderer.locator("#dynamic-frame-result").textContent(),
    "RTCPeerConnection is blocked by Rings WebView",
  );
  assert.equal(
    await renderer.frameLocator("#dynamic-frame").locator("#dynamic-child").textContent(),
    "recursive renderer",
  );
  assert.equal(
    await renderer.frameLocator("#adjacent-frame").locator("#dynamic-child").textContent(),
    "recursive renderer",
  );
  await renderer.locator("#blank-frame-result").filter({ hasText: "isolated blank realm" }).waitFor();
  assert.equal(
    await renderer.frameLocator("#attribute-frame").locator("#attribute-frame-result").textContent(),
    "Attr mutation stayed recursive",
  );
  assert.equal(
    await renderer.frameLocator("#named-frame").locator("#named-frame-result").textContent(),
    "NamedNodeMap stayed recursive",
  );
  await renderer.frameLocator("#removed-frame").locator("body").waitFor({ state: "attached" });
  assert.equal(await renderer.frameLocator("#removed-frame").locator("#removed-source-leak").count(), 0);

  await renderer
    .locator("#worker-tla-result")
    .filter({ hasText: "queued through top-level await" })
    .waitFor()
    .catch(async (error: unknown): Promise<never> => {
      const failedRecords = await popup.evaluate(
        (): readonly FixtureCredentialRecord[] =>
          (globalThis as FixtureExtensionGlobal).__fixtureCredentialRecords ?? [],
      );
      throw new Error(
        `top-level-await Worker did not settle; records=${JSON.stringify(failedRecords)}; pageErrors=${pageErrors.join(" | ")}; console=${consoleMessages.join(" | ")}; ${String(error)}`,
      );
    });
  await renderer
    .locator("#worker-omit-result")
    .filter({ hasText: "omit worker ready" })
    .waitFor()
    .catch(async (error: unknown): Promise<never> => {
      const failedRecords = await popup.evaluate(
        (): readonly FixtureCredentialRecord[] =>
          (globalThis as FixtureExtensionGlobal).__fixtureCredentialRecords ?? [],
      );
      throw new Error(
        `credential Worker did not settle; recordCount=${failedRecords.length}; firstRecords=${JSON.stringify(failedRecords.slice(0, 20))}; pageErrors=${pageErrors.join(" | ")}; ${String(error)}`,
      );
    });
  await renderer.locator("#worker-include-result").filter({ hasText: "include worker ready" }).waitFor();
  await renderer.locator("#worker-meta-result").filter({ hasText: "https://fixture.example/worker-meta.js" }).waitFor();
  assert.equal(
    await renderer.locator("#worker-meta-result").textContent(),
    "https://fixture.example/worker-meta.js|https://fixture.example/asset.txt|7|true|true",
  );
  await renderer.locator("#worker-cycle-result").filter({ hasText: "3|2|3" }).waitFor();
  assert.equal(await renderer.locator("#worker-cycle-result").textContent(), "3|2|3");
  await renderer
    .locator("#worker-forged-ready-result")
    .filter({ hasText: "rings.worker.ready|echo:guarded queue" })
    .waitFor();
  assert.equal(
    await renderer.locator("#worker-forged-ready-result").textContent(),
    "rings.worker.ready|echo:guarded queue",
  );
  await renderer.locator("#worker-body-limit-result").filter({ hasText: "exceeds 8388608 bytes" }).waitFor();
  assert.equal(await fixtureRequestCount(popup, "https://fixture.example/worker-upload"), 0);
  await renderer.locator("#page-body-limit-result").filter({ hasText: "exceeds 8388608 bytes" }).waitFor();
  assert.equal(await fixtureRequestCount(popup, "https://fixture.example/page-upload"), 0);

  const records = await popup.evaluate(
    (): readonly FixtureCredentialRecord[] => (globalThis as FixtureExtensionGlobal).__fixtureCredentialRecords ?? [],
  );
  assertWorkerCredentials(records, "worker-omit", "omit");
  assertWorkerCredentials(records, "worker-include", "include");
  await popup.waitForFunction(
    (): boolean =>
      ((globalThis as FixtureExtensionGlobal).__fixtureCredentialRecords ?? []).filter(
        (record: FixtureCredentialRecord): boolean => record.target === "https://fixture.example/worker-ping",
      ).length >= 3,
  );
  const formNavigationTarget = "https://fixture.example/replacement?via=form";
  const formNavigationsBefore = await fixtureRequestCount(popup, formNavigationTarget);
  await renderer.locator("#get-navigation-form").evaluate((form: HTMLFormElement): void => form.requestSubmit());
  await renderer.locator("h1").filter({ hasText: "Replacement document" }).waitFor();
  assert.equal(
    await fixtureRequestCount(popup, formNavigationTarget),
    formNavigationsBefore + 1,
    "one GET form submission must produce exactly one shell navigation",
  );
  await popup.waitForTimeout(100);
  const pingsAfterRelease = await fixtureRequestCount(popup, "https://fixture.example/worker-ping");
  await popup.waitForTimeout(200);
  assert.equal(
    await fixtureRequestCount(popup, "https://fixture.example/worker-ping"),
    pingsAfterRelease,
    "a Worker from the replaced document retained gateway authority",
  );
  assert.deepEqual(pageErrors, []);
  console.log("Extension WebView focused regressions passed");
} finally {
  await harness.close();
}

/** Requires both a module entry and its dependency to retain one credentials mode. */
function assertWorkerCredentials(
  records: readonly FixtureCredentialRecord[],
  workerPrefix: string,
  expected: "omit" | "include",
): void {
  const matching = records.filter((record: FixtureCredentialRecord): boolean => record.target.includes(workerPrefix));
  assert.equal(matching.length, 2, `${workerPrefix} did not fetch exactly its entry and dependency`);
  assert(matching.every((record: FixtureCredentialRecord): boolean => record.credentials === expected));
}
