const WALLET_CONNECT = "rings.wallet.connect";
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
    16: `icons/rings-${state}-16.png`,
    32: `icons/rings-${state}-32.png`,
    48: `icons/rings-${state}-48.png`,
    128: `icons/rings-${state}-128.png`,
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
    return `wallet bridge cannot access ${url}; reload the Rings extension so its http/https host permissions take effect`;
  }
  return `wallet bridge injection failed on ${url}: ${message}`;
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
