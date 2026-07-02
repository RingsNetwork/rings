#!/usr/bin/env node

import { execFile } from "node:child_process";
import { cp, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(scriptDir, "..");
const sourceDist = resolve(projectRoot, process.argv[2] ?? "dist");
const extensionDist = resolve(projectRoot, process.argv[3] ?? "dist-extension");
const execFileAsync = promisify(execFile);
const sourceIconSvg = join(projectRoot, "assets", "icons", "rings.svg");

const cargoToml = await readFile(join(projectRoot, "Cargo.toml"), "utf8");
const crateVersion = cargoToml.match(/^version\s*=\s*"([^"]+)"/m)?.[1] ?? "0.1.0";
const extensionVersion = chromeVersion(crateVersion);

const ICON_STATES = {
  neutral: {
    file: "rings",
    color: "#00e5ff",
  },
  disconnected: {
    file: "rings-disconnected",
    color: "#d8fbff",
  },
  connecting: {
    file: "rings-connecting",
    color: "#ffcb6b",
  },
  connected: {
    file: "rings-connected",
    color: "#54ffd0",
  },
};

const files = await readdir(sourceDist);
const sourceHtml = await readFile(join(sourceDist, "index.html"), "utf8").catch(() => "");
const jsFile =
  entryFileFromHtml(sourceHtml, files, /import\s+init[\s\S]*?from\s+['"]([^'"]+\.js)['"]/) ??
  entryFileFromHtml(sourceHtml, files, /<link[^>]+rel="modulepreload"[^>]+href="([^"]+\.js)"/) ??
  singleFile(files, (file) => file.endsWith(".js"), "generated JS bundle");
const wasmFile =
  entryFileFromHtml(sourceHtml, files, /module_or_path:\s*['"]([^'"]+_bg\.wasm)['"]/) ??
  entryFileFromHtml(sourceHtml, files, /<link[^>]+rel="preload"[^>]+href="([^"]+_bg\.wasm)"/) ??
  singleFile(files, (file) => file.endsWith("_bg.wasm"), "generated wasm bundle");

await rm(extensionDist, { force: true, recursive: true });
await mkdir(extensionDist, { recursive: true });
await cp(join(sourceDist, jsFile), join(extensionDist, jsFile));
await cp(join(sourceDist, wasmFile), join(extensionDist, wasmFile));

await writeFile(
  join(extensionDist, "index.html"),
  htmlShell(jsFile, wasmFile, { includeNodeBridge: true }),
  "utf8",
);
await writeFile(
  join(extensionDist, "offscreen.html"),
  htmlShell(jsFile, wasmFile, { includeNodeBridge: false }),
  "utf8",
);
await writeFile(
  join(extensionDist, "bootstrap.js"),
  bootstrapScript(jsFile, wasmFile),
  "utf8",
);
await writeFile(
  join(extensionDist, "wallet_bridge.js"),
  walletBridgeScript(),
  "utf8",
);
await writeFile(
  join(extensionDist, "node_bridge.js"),
  nodeBridgeScript(),
  "utf8",
);
await writeFile(
  join(extensionDist, "service_worker.js"),
  serviceWorkerScript(),
  "utf8",
);
await writeExtensionIcons();
await writeFile(
  join(extensionDist, "manifest.json"),
  `${JSON.stringify(manifest(extensionVersion), null, 2)}\n`,
  "utf8",
);

console.log(`Packaged Chrome MV3 extension at ${extensionDist}`);

function singleFile(files, predicate, label) {
  const matches = files.filter(predicate);
  if (matches.length !== 1) {
    throw new Error(`Expected one ${label} in ${sourceDist}, found ${matches.length}`);
  }
  return matches[0];
}

function entryFileFromHtml(html, files, pattern) {
  const match = html.match(pattern);
  if (!match) {
    return undefined;
  }
  const file = match[1].replace(/^\.\//, "").replace(/^\//, "");
  return files.includes(file) ? file : undefined;
}

function chromeVersion(version) {
  const parts = version.split(".").map((part) => part.replace(/\D.*$/, "") || "0");
  while (parts.length < 3) {
    parts.push("0");
  }
  return parts.slice(0, 4).join(".");
}

async function writeExtensionIcons() {
  const iconsDir = join(extensionDist, "icons");
  await mkdir(iconsDir, { recursive: true });
  const iconSvg = await readFile(sourceIconSvg, "utf8");
  await writeFile(join(iconsDir, "rings.svg"), iconSvg, "utf8");
  for (const state of Object.values(ICON_STATES)) {
    const tempDir = await mkdtemp(join(tmpdir(), "rings-icon-"));
    const svgPath = join(tempDir, `${state.file}.svg`);
    try {
      await writeFile(svgPath, tintIconSvg(iconSvg, state.color), "utf8");
      for (const size of [16, 32, 48, 128]) {
        await renderSvgToPng(svgPath, join(iconsDir, `${state.file}-${size}.png`), size);
      }
    } finally {
      await rm(tempDir, { force: true, recursive: true });
    }
  }
}

function tintIconSvg(svg, color) {
  const tinted = svg.replace(/\sfill="(?!none\b)[^"]*"/gi, ` fill="${color}"`);
  if (tinted !== svg) {
    return tinted;
  }
  return svg.replace(
    /<svg\b([^>]*)>/i,
    `<svg$1>\n<style>path,circle,rect,polygon,polyline,ellipse{fill:${color};}</style>`,
  );
}

async function renderSvgToPng(svgPath, pngPath, size) {
  const renderers = [
    ["sips", ["-s", "format", "png", "-z", String(size), String(size), svgPath, "--out", pngPath]],
    ["rsvg-convert", ["-w", String(size), "-h", String(size), "-o", pngPath, svgPath]],
    ["magick", [svgPath, "-resize", `${size}x${size}`, pngPath]],
  ];
  const errors = [];
  for (const [command, args] of renderers) {
    try {
      await execFileAsync(command, args);
      return;
    } catch (error) {
      errors.push(`${command}: ${error.message}`);
    }
  }
  throw new Error(
    `Unable to render ${svgPath} to ${pngPath}. Tried sips, rsvg-convert, and magick.\n${errors.join("\n")}`,
  );
}

function htmlShell(jsFile, wasmFile, { includeNodeBridge }) {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Rings</title>
    <link rel="icon" type="image/svg+xml" href="./icons/rings.svg" />
    <link rel="icon" type="image/png" sizes="16x16" href="./icons/rings-16.png" />
    <link rel="icon" type="image/png" sizes="32x32" href="./icons/rings-32.png" />
    <link rel="apple-touch-icon" href="./icons/rings-128.png" />
    <meta name="theme-color" content="#020c10" />
    <link rel="modulepreload" href="./${jsFile}" />
    <link rel="preload" href="./${wasmFile}" as="fetch" type="application/wasm" />
  </head>
  <body>
    <script type="module" src="./wallet_bridge.js"></script>
    ${includeNodeBridge ? '<script type="module" src="./node_bridge.js"></script>' : ''}
    <script type="module" src="./bootstrap.js"></script>
  </body>
</html>
`;
}

function bootstrapScript(jsFile, wasmFile) {
  return `import init, * as bindings from "./${jsFile}";

const wasm = await init({
  module_or_path: new URL("./${wasmFile}", import.meta.url),
});

globalThis.wasmBindings = bindings;
globalThis.dispatchEvent(new CustomEvent("TrunkApplicationStarted", { detail: { wasm } }));
`;
}

function serviceWorkerScript() {
  return `const WALLET_CONNECT = "rings.wallet.connect";
const WALLET_SIGN = "rings.wallet.sign";
const WALLET_SELECT_PROVIDER = "rings.wallet.selectProvider";
const NODE_ENSURE_OFFSCREEN = "rings.node.ensureOffscreen";
const ICON_SET = "rings.icon.set";
const OFFSCREEN_DOCUMENT = "offscreen.html";
const ICON_STATES = new Set(["disconnected", "connecting", "connected"]);
const ICON_TITLES = {
  disconnected: "Rings: node offline",
  connecting: "Rings: connecting",
  connected: "Rings: node connected",
};
let creatingOffscreenDocument;
let selectedEip191Provider = {
  providerId: "",
  tabId: undefined,
};

async function configureSidePanel() {
  if (!chrome.sidePanel?.setPanelBehavior) {
    return;
  }
  try {
    await chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true });
  } catch (error) {
    console.warn("Failed to configure Rings side panel", error);
  }
}

chrome.runtime.onInstalled.addListener(() => {
  configureSidePanel();
  setNodeIconState("disconnected").catch((error) => {
    console.warn("Failed to set Rings extension icon", error);
  });
});
chrome.runtime.onStartup.addListener(() => {
  configureSidePanel();
  setNodeIconState("disconnected").catch((error) => {
    console.warn("Failed to set Rings extension icon", error);
  });
});
configureSidePanel();
setNodeIconState("disconnected").catch((error) => {
  console.warn("Failed to set Rings extension icon", error);
});

chrome.action.onClicked.addListener(() => {
  if (chrome.sidePanel?.setPanelBehavior) {
    return;
  }
  chrome.tabs.create({ url: chrome.runtime.getURL("index.html") });
});

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (message?.type === ICON_SET) {
    setNodeIconState(message.state)
      .then(() => sendResponse({ ok: true }))
      .catch((error) => {
        sendResponse({
          ok: false,
          error: error instanceof Error ? error.message : String(error),
        });
      });
    return true;
  }

  if (message?.type === NODE_ENSURE_OFFSCREEN) {
    ensureOffscreenDocument()
      .then(() => sendResponse({ ok: true }))
      .catch((error) => {
        sendResponse({
          ok: false,
          error: error instanceof Error ? error.message : String(error),
        });
      });
    return true;
  }

  if (
    !message ||
    ![WALLET_CONNECT, WALLET_SIGN, WALLET_SELECT_PROVIDER].includes(message.type)
  ) {
    return false;
  }

  handleWalletMessage(message)
    .then((result) => sendResponse({ ok: true, result }))
    .catch((error) => {
      sendResponse({
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      });
    });
  return true;
});

async function setNodeIconState(state) {
  if (!chrome.action?.setIcon) {
    return;
  }
  const safeState = ICON_STATES.has(state) ? state : "disconnected";
  await chrome.action.setIcon({ path: iconPaths(safeState) });
  if (chrome.action.setTitle) {
    await chrome.action.setTitle({ title: ICON_TITLES[safeState] });
  }
}

function iconPaths(state) {
  return {
    16: \`icons/rings-\${state}-16.png\`,
    32: \`icons/rings-\${state}-32.png\`,
    48: \`icons/rings-\${state}-48.png\`,
    128: \`icons/rings-\${state}-128.png\`,
  };
}

async function ensureOffscreenDocument() {
  if (!chrome.offscreen?.createDocument) {
    throw new Error("Chrome offscreen documents are unavailable");
  }
  const offscreenUrl = chrome.runtime.getURL(OFFSCREEN_DOCUMENT);
  const contexts = await chrome.runtime.getContexts({
    contextTypes: ["OFFSCREEN_DOCUMENT"],
    documentUrls: [offscreenUrl],
  });
  if (contexts.length > 0) {
    return;
  }
  if (!creatingOffscreenDocument) {
    creatingOffscreenDocument = chrome.offscreen.createDocument({
      url: OFFSCREEN_DOCUMENT,
      reasons: ["WEB_RTC"],
      justification: "Keep the Rings browser node WebRTC transport alive while the side panel is closed.",
    }).finally(() => {
      creatingOffscreenDocument = undefined;
    });
  }
  await creatingOffscreenDocument;
}

async function handleWalletMessage(message) {
  if (message.type === WALLET_SELECT_PROVIDER) {
    selectedEip191Provider = {
      providerId: String(message.providerId || ""),
      tabId: Number.isInteger(message.tabId) ? message.tabId : undefined,
    };
    return selectedEip191Provider;
  }
  if (!["eip191", "metamask", "ed25519", "phantom"].includes(message.wallet)) {
    throw new Error("unsupported wallet bridge");
  }
  if (message.type === WALLET_CONNECT) {
    return connectWallet(message.wallet);
  }
  if (typeof message.proof !== "string" || message.proof.length === 0) {
    throw new Error("wallet bridge proof is empty");
  }
  return signWithWallet(message.wallet, message.proof, message.account || "");
}

async function connectWallet(wallet) {
  if (wallet === "eip191" || wallet === "metamask") {
    selectedEip191Provider = { providerId: "", tabId: undefined };
    try {
      const tab = await activeInjectableTab();
      const result = await executeInTab(tab, connectEip191InPage, []);
      selectedEip191Provider = {
        providerId: "",
        tabId: tab.id,
      };
      return {
        ...result,
        tabId: tab.id,
      };
    } catch (error) {
      selectedEip191Provider = { providerId: "", tabId: undefined };
      throw error;
    }
  }
  return executeInActiveTab(connectEd25519InPage, []);
}

async function signWithWallet(wallet, proof, account) {
  if (wallet === "eip191" || wallet === "metamask") {
    return executeInWalletTab(
      signEip191InPage,
      [proof, account],
      selectedEip191Provider.tabId,
    );
  }
  return executeInActiveTab(signEd25519InPage, [proof]);
}

async function executeInActiveTab(func, args) {
  const tab = await activeInjectableTab();
  return executeInTab(tab, func, args);
}

async function executeInWalletTab(func, args, tabId) {
  if (Number.isInteger(tabId)) {
    const tab = await chrome.tabs.get(tabId);
    if (isInjectableTab(tab)) {
      return executeInTab(tab, func, args);
    }
  }
  return executeInActiveTab(func, args);
}

async function executeInTab(tab, func, args) {
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: { tabId: tab.id },
      world: "MAIN",
      func,
      args,
    });
  } catch (error) {
    throw new Error(walletInjectionError(tab, error));
  }
  const result = results?.[0]?.result;
  if (!result || result.ok !== true) {
    throw new Error(result?.error || "wallet bridge returned no result");
  }
  return result.value;
}

function walletInjectionError(tab, error) {
  const message = error instanceof Error ? error.message : String(error);
  const url = tab.url || "the active tab";
  if (message.includes("Cannot access a chrome:// URL")) {
    return "wallet bridge needs an ordinary http/https tab with the wallet provider active; Chrome internal pages cannot be used";
  }
  if (
    message.includes("Cannot access contents of the page") ||
    message.includes("Extension manifest must request permission")
  ) {
    return \`wallet bridge cannot access \${url}; reload the Rings extension so its http/https host permissions take effect\`;
  }
  return \`wallet bridge injection failed on \${url}: \${message}\`;
}

async function activeInjectableTab() {
  const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
  if (!tab?.id) {
    throw new Error("open an ordinary web page tab before connecting a wallet");
  }
  if (!isInjectableTab(tab)) {
    throw new Error("wallet bridge can only run on ordinary http/https tabs");
  }
  return tab;
}

function isInjectableTab(tab) {
  return Boolean(tab?.id && tab.url && (tab.url.startsWith("http://") || tab.url.startsWith("https://")));
}

async function connectEip191InPage() {
  try {
    const provider = globalThis.ethereum;
    if (!provider?.request) {
      throw new Error("EIP-1193 Ethereum provider not found on current tab");
    }
    const accounts = await provider.request({ method: "eth_requestAccounts" });
    const account = accounts?.[0];
    if (!account) {
      throw new Error("Ethereum provider returned no account");
    }
    return {
      ok: true,
      value: {
        wallet: "eip191",
        account: String(account),
        accountType: "eip191",
        providerId: "",
        providerName: "Injected Ethereum provider",
        providerRdns: "",
        origin: location.origin,
      },
    };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    };
  }
}

async function signEip191InPage(proof, account) {
  try {
    const provider = globalThis.ethereum;
    if (!provider?.request) {
      throw new Error("EIP-1193 Ethereum provider not found on current tab");
    }
    const selectedAccount = account || (await provider.request({ method: "eth_requestAccounts" }))?.[0];
    if (!selectedAccount) {
      throw new Error("Ethereum provider returned no account");
    }
    const signature = await provider.request({
      method: "personal_sign",
      params: [proof, selectedAccount],
    });
    if (typeof signature !== "string") {
      throw new Error("Ethereum provider returned a non-string signature");
    }
    return {
      ok: true,
      value: {
        wallet: "eip191",
        account: String(selectedAccount),
        accountType: "eip191",
        signature,
        providerId: "",
        providerName: "Injected Ethereum provider",
        providerRdns: "",
        origin: location.origin,
      },
    };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    };
  }
}

async function connectEd25519InPage() {
  try {
    const provider = globalThis.phantom?.solana ?? globalThis.solana;
    if (!provider) {
      throw new Error("Solana provider not found on current tab");
    }
    const response = await provider.connect();
    const publicKey = response?.publicKey ?? provider.publicKey;
    const account = publicKey?.toBase58 ? publicKey.toBase58() : String(publicKey ?? "");
    if (!account) {
      throw new Error("Solana provider returned no public key");
    }
    return {
      ok: true,
      value: {
        wallet: "ed25519",
        account,
        accountType: "ed25519",
        origin: location.origin,
      },
    };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    };
  }
}

async function signEd25519InPage(proof) {
  try {
    const provider = globalThis.phantom?.solana ?? globalThis.solana;
    if (!provider) {
      throw new Error("Solana provider not found on current tab");
    }
    if (!provider.isConnected && provider.connect) {
      await provider.connect();
    }
    if (!provider.signMessage) {
      throw new Error("Solana provider signMessage is unavailable");
    }
    const encoded = new TextEncoder().encode(proof);
    const signed = await provider.signMessage(encoded, "utf8");
    const rawSignature = signed?.signature ?? signed;
    const signature = Array.from(rawSignature instanceof Uint8Array ? rawSignature : new Uint8Array(rawSignature));
    const publicKey = signed?.publicKey ?? provider.publicKey;
    const account = publicKey?.toBase58 ? publicKey.toBase58() : String(publicKey ?? "");
    return {
      ok: true,
      value: {
        wallet: "ed25519",
        account,
        accountType: "ed25519",
        signature,
        origin: location.origin,
      },
    };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    };
  }
}
`;
}

function walletBridgeScript() {
return `function sendWalletMessage(message) {
  if (!globalThis.chrome?.runtime?.sendMessage) {
    return Promise.reject(new Error("Rings extension wallet bridge is unavailable"));
  }
  return new Promise((resolve, reject) => {
    chrome.runtime.sendMessage(message, (response) => {
      const runtimeError = chrome.runtime.lastError;
      if (runtimeError) {
        reject(new Error(runtimeError.message));
        return;
      }
      if (!response?.ok) {
        reject(new Error(response?.error || "wallet bridge failed"));
        return;
      }
      resolve(response.result);
    });
  });
}

function isEip191Wallet(wallet) {
  return wallet === "eip191" || wallet === "metamask";
}

globalThis.RingsExtensionWalletBridge = {
  async resetProvider(wallet) {
    if (!isEip191Wallet(wallet)) {
      return null;
    }
    return sendWalletMessage({
      type: "rings.wallet.selectProvider",
      providerId: "",
    });
  },
  async connect(wallet) {
    if (isEip191Wallet(wallet)) {
      await this.resetProvider(wallet).catch(() => {});
    }
    try {
      return await sendWalletMessage({
        type: "rings.wallet.connect",
        wallet,
      });
    } catch (error) {
      if (isEip191Wallet(wallet)) {
        await this.resetProvider(wallet).catch(() => {});
      }
      throw error;
    }
  },
  async sign(wallet, proof, account = "") {
    return sendWalletMessage({
      type: "rings.wallet.sign",
      wallet,
      proof,
      account,
    });
  },
};
`;
}

function nodeBridgeScript() {
  return `let startPromise;
let iconWatchPromise;

function sendRuntimeMessage(message) {
  if (!globalThis.chrome?.runtime?.sendMessage) {
    return Promise.reject(new Error("Rings extension node bridge is unavailable"));
  }
  return new Promise((resolve, reject) => {
    chrome.runtime.sendMessage(message, (response) => {
      const runtimeError = chrome.runtime.lastError;
      if (runtimeError) {
        reject(new Error(runtimeError.message));
        return;
      }
      if (!response?.ok) {
        reject(new Error(response?.error || "node bridge failed"));
        return;
      }
      resolve(response.result);
    });
  });
}

async function setExtensionIconState(state) {
  try {
    await sendRuntimeMessage({
      type: "rings.icon.set",
      state,
    });
  } catch (error) {
    console.warn("Rings extension icon update failed", error);
  }
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function ensureOffscreenNode() {
  await sendRuntimeMessage({ type: "rings.node.ensureOffscreen" });
}

function shouldRetryNodeMessage(error) {
  const message = error instanceof Error ? error.message : String(error);
  return (
    message.includes("Receiving end does not exist") ||
    message.includes("message port closed") ||
    message.includes("The message port closed")
  );
}

async function sendNodeMessage(message) {
  return sendNodeMessageWithRetry(message);
}

async function sendNodeMessageToOffscreen(message) {
  return sendRuntimeMessage({
    ...message,
    target: "rings.node.offscreen",
  });
}

async function sendNodeMessageWithRetry(message) {
  await ensureOffscreenNode();
  let lastError;
  for (let attempt = 0; attempt < 25; attempt += 1) {
    try {
      return await sendNodeMessageToOffscreen(message);
    } catch (error) {
      lastError = error;
      if (!shouldRetryNodeMessage(error)) {
        throw error;
      }
      await delay(120);
    }
  }
  throw lastError ?? new Error("node bridge did not respond");
}

function iconStateFromSnapshot(snapshot) {
  if (snapshot?.online) {
    return "connected";
  }
  if (snapshot?.starting) {
    return "connecting";
  }
  return "disconnected";
}

async function refreshNodeIcon() {
  const snapshot = await sendNodeMessageWithRetry({
    type: "rings.node.status",
  });
  await setExtensionIconState(iconStateFromSnapshot(snapshot));
  return snapshot;
}

function watchNodeIconUntilSettled() {
  if (iconWatchPromise) {
    return;
  }
  iconWatchPromise = (async () => {
    for (let attempt = 0; attempt < 240; attempt += 1) {
      await delay(750);
      let snapshot;
      try {
        snapshot = await refreshNodeIcon();
      } catch (error) {
        await setExtensionIconState("disconnected");
        return;
      }
      if (!snapshot?.starting) {
        return;
      }
    }
    await setExtensionIconState("disconnected");
  })().finally(() => {
    iconWatchPromise = undefined;
  });
}

async function startNode(settings) {
  await setExtensionIconState("connecting");
  try {
    if ((settings?.walletKind === "eip191" || settings?.walletKind === "metamask") && globalThis.RingsExtensionWalletBridge?.resetProvider) {
      await globalThis.RingsExtensionWalletBridge.resetProvider(settings.walletKind);
    }
    const snapshot = await sendNodeMessageWithRetry({
      type: "rings.node.start",
      settings,
    });
    await setExtensionIconState(iconStateFromSnapshot(snapshot));
    if (snapshot?.starting) {
      watchNodeIconUntilSettled();
    }
    return snapshot;
  } catch (error) {
    await setExtensionIconState("disconnected");
    throw error;
  }
}

async function stopNode() {
  try {
    const result = await sendNodeMessage({
      type: "rings.node.stop",
    });
    await setExtensionIconState("disconnected");
    return result;
  } catch (error) {
    await setExtensionIconState("disconnected");
    throw error;
  }
}

async function nodeStatus() {
  const snapshot = await sendNodeMessage({
    type: "rings.node.status",
  });
  await setExtensionIconState(iconStateFromSnapshot(snapshot));
  return snapshot;
}

globalThis.RingsExtensionNodeBridge = {
  start(settings) {
    if (!startPromise) {
      startPromise = startNode(settings).finally(() => {
        startPromise = undefined;
      });
    }
    return startPromise;
  },
  stop() {
    return stopNode();
  },
  status() {
    return nodeStatus();
  },
  connectHttp(endpoint) {
    return sendNodeMessage({
      type: "rings.node.connectHttp",
      endpoint,
    });
  },
  createOffer(did) {
    return sendNodeMessage({
      type: "rings.node.createOffer",
      did,
    });
  },
  answerOffer(offer) {
    return sendNodeMessage({
      type: "rings.node.answerOffer",
      offer,
    });
  },
  acceptAnswer(answer) {
    return sendNodeMessage({
      type: "rings.node.acceptAnswer",
      answer,
    });
  },
};
`;
}

function manifest(version) {
  return {
    manifest_version: 3,
    name: "Rings Node",
    short_name: "Rings",
    version,
    description: "Rings browser node console for WebRTC peer connectivity and Chord topology.",
    minimum_chrome_version: "116",
    icons: {
      16: "icons/rings-16.png",
      32: "icons/rings-32.png",
      48: "icons/rings-48.png",
      128: "icons/rings-128.png",
    },
    action: {
      default_title: "Rings",
      default_icon: {
        16: "icons/rings-disconnected-16.png",
        32: "icons/rings-disconnected-32.png",
        48: "icons/rings-disconnected-48.png",
        128: "icons/rings-disconnected-128.png",
      },
    },
    background: {
      service_worker: "service_worker.js",
    },
    side_panel: {
      default_path: "index.html",
    },
    options_page: "index.html",
    permissions: ["activeTab", "offscreen", "scripting", "sidePanel", "storage"],
    host_permissions: ["http://*/*", "https://*/*"],
    content_security_policy: {
      extension_pages: "script-src 'self' 'wasm-unsafe-eval'; object-src 'self';",
    },
  };
}
