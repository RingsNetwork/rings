"use strict";
/**
 * MV3 service worker for Rings wallet injection, offscreen node setup, and icon state.
 */
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
};
/**
 * Enables side-panel opening from the extension action when the browser supports it.
 */
async function configureSidePanel() {
    if (!chrome.sidePanel?.setPanelBehavior) {
        return;
    }
    try {
        await chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true });
    }
    catch (error) {
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
    const optionalChrome = chrome;
    if (optionalChrome.sidePanel?.setPanelBehavior) {
        return;
    }
    chrome.tabs.create({ url: chrome.runtime.getURL("index.html") });
});
chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    if (isIconSetMessage(message)) {
        setNodeIconState(message.state)
            .then(() => sendResponse({ ok: true }))
            .catch((error) => {
            sendResponse({
                ok: false,
                error: errorMessage(error),
            });
        });
        return true;
    }
    if (isEnsureOffscreenMessage(message)) {
        ensureOffscreenDocument()
            .then(() => sendResponse({ ok: true }))
            .catch((error) => {
            sendResponse({
                ok: false,
                error: errorMessage(error),
            });
        });
        return true;
    }
    if (!isWalletMessage(message)) {
        return false;
    }
    handleWalletMessage(message)
        .then((result) => sendResponse({ ok: true, result }))
        .catch((error) => {
        sendResponse({
            ok: false,
            error: errorMessage(error),
        });
    });
    return true;
});
/**
 * Updates the extension action icon and tooltip from a caller-provided state.
 */
async function setNodeIconState(state) {
    if (!chrome.action?.setIcon) {
        return;
    }
    const safeState = isIconState(state) ? state : "disconnected";
    await chrome.action.setIcon({ path: iconPaths(safeState) });
    await chrome.action.setTitle({ title: ICON_TITLES[safeState] });
}
/**
 * Returns manifest icon paths for one node state.
 */
function iconPaths(state) {
    return {
        16: `icons/rings-${state}-16.png`,
        32: `icons/rings-${state}-32.png`,
        48: `icons/rings-${state}-48.png`,
        128: `icons/rings-${state}-128.png`,
    };
}
/**
 * Creates the single offscreen document that owns the retained WebRTC node.
 */
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
/**
 * Dispatches wallet connect, sign, and provider-selection runtime messages.
 */
async function handleWalletMessage(message) {
    if (message.type === WALLET_SELECT_PROVIDER) {
        selectedEip191Provider = providerSelection(message);
        return selectedEip191Provider;
    }
    const wallet = walletKind(message.wallet);
    if (!wallet) {
        throw new Error("unsupported wallet bridge");
    }
    if (message.type === WALLET_CONNECT) {
        return connectWallet(wallet);
    }
    if (typeof message.proof !== "string" || message.proof.length === 0) {
        throw new Error("wallet bridge proof is empty");
    }
    return signWithWallet(wallet, message.proof, typeof message.account === "string" ? message.account : "");
}
/**
 * Connects the requested wallet kind through the active injectable tab.
 */
async function connectWallet(wallet) {
    if (wallet === "eip191" || wallet === "metamask") {
        selectedEip191Provider = { providerId: "" };
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
        }
        catch (error) {
            selectedEip191Provider = { providerId: "" };
            throw error;
        }
    }
    return executeInActiveTab(connectEd25519InPage, []);
}
/**
 * Signs a Rings proof using the wallet path that matches the account type.
 */
async function signWithWallet(wallet, proof, account) {
    if (wallet === "eip191" || wallet === "metamask") {
        return executeInWalletTab(signEip191InPage, [proof, account], selectedEip191Provider.tabId);
    }
    return executeInActiveTab(signEd25519InPage, [proof]);
}
/**
 * Runs one wallet operation in the current active injectable tab.
 */
async function executeInActiveTab(func, args) {
    const tab = await activeInjectableTab();
    return executeInTab(tab, func, args);
}
/**
 * Runs one wallet operation in the remembered wallet tab when possible.
 */
async function executeInWalletTab(func, args, tabId) {
    if (typeof tabId === "number" && Number.isInteger(tabId)) {
        const tab = await getTab(tabId);
        if (isInjectableTab(tab)) {
            return executeInTab(tab, func, args);
        }
    }
    return executeInActiveTab(func, args);
}
/**
 * Injects a self-contained wallet operation into a page main world and unwraps its result.
 */
async function executeInTab(tab, func, args) {
    let results;
    try {
        results = await chrome.scripting.executeScript({
            target: { tabId: tab.id },
            world: "MAIN",
            func,
            args: [...args],
        });
    }
    catch (error) {
        throw new Error(walletInjectionError(tab, error));
    }
    const result = results[0]?.result;
    if (!result || result.ok !== true) {
        throw new Error(result?.error || "wallet bridge returned no result");
    }
    return result.value;
}
/**
 * Converts Chrome scripting errors into user-facing wallet bridge messages.
 */
function walletInjectionError(tab, error) {
    const message = errorMessage(error);
    const url = tab.url || "the active tab";
    if (message.includes("Cannot access a chrome:// URL")) {
        return "wallet bridge needs an ordinary http/https tab with the wallet provider active; Chrome internal pages cannot be used";
    }
    if (message.includes("Cannot access contents of the page") ||
        message.includes("Extension manifest must request permission")) {
        return `wallet bridge cannot access ${url}; reload the Rings extension so its http/https host permissions take effect`;
    }
    return `wallet bridge injection failed on ${url}: ${message}`;
}
/**
 * Returns the active http or https tab that can host wallet-provider injection.
 */
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
/**
 * Checks whether a Chrome tab is addressable by chrome.scripting.
 */
function isInjectableTab(tab) {
    return Boolean(tab?.id && tab.url && (tab.url.startsWith("http://") || tab.url.startsWith("https://")));
}
/**
 * Self-contained injected function that connects an EIP-1193 provider.
 */
async function connectEip191InPage() {
    try {
        const provider = globalThis.ethereum;
        if (!provider?.request) {
            throw new Error("EIP-1193 Ethereum provider not found on current tab");
        }
        const accounts = await provider.request({ method: "eth_requestAccounts" });
        const account = Array.isArray(accounts) && typeof accounts[0] === "string" ? accounts[0] : undefined;
        if (!account) {
            throw new Error("Ethereum provider returned no account");
        }
        return {
            ok: true,
            value: {
                wallet: "eip191",
                account,
                accountType: "eip191",
                providerId: "",
                providerName: "Injected Ethereum provider",
                providerRdns: "",
                origin: location.origin,
            },
        };
    }
    catch (error) {
        return {
            ok: false,
            error: error instanceof Error ? error.message : String(error),
        };
    }
}
/**
 * Self-contained injected function that signs a proof with personal_sign.
 */
async function signEip191InPage(proof, account) {
    try {
        const provider = globalThis.ethereum;
        if (!provider?.request) {
            throw new Error("EIP-1193 Ethereum provider not found on current tab");
        }
        const accounts = await provider.request({ method: "eth_requestAccounts" });
        const selectedAccount = account || (Array.isArray(accounts) && typeof accounts[0] === "string" ? accounts[0] : undefined);
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
                account: selectedAccount,
                accountType: "eip191",
                signature,
                providerId: "",
                providerName: "Injected Ethereum provider",
                providerRdns: "",
                origin: location.origin,
            },
        };
    }
    catch (error) {
        return {
            ok: false,
            error: error instanceof Error ? error.message : String(error),
        };
    }
}
/**
 * Self-contained injected function that connects a Solana-compatible Ed25519 provider.
 */
async function connectEd25519InPage() {
    try {
        const pageGlobal = globalThis;
        const provider = pageGlobal.phantom?.solana ?? pageGlobal.solana;
        if (!provider) {
            throw new Error("Solana provider not found on current tab");
        }
        const response = provider.connect ? await provider.connect() : undefined;
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
    }
    catch (error) {
        return {
            ok: false,
            error: error instanceof Error ? error.message : String(error),
        };
    }
}
/**
 * Self-contained injected function that signs a proof with a Solana-compatible provider.
 */
async function signEd25519InPage(proof) {
    try {
        const pageGlobal = globalThis;
        const provider = pageGlobal.phantom?.solana ?? pageGlobal.solana;
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
        const isNumberList = (value) => Array.isArray(value) && value.every((item) => typeof item === "number");
        let rawSignature;
        let publicKey;
        if (signed instanceof Uint8Array || isNumberList(signed)) {
            rawSignature = signed;
            publicKey = provider.publicKey;
        }
        else {
            rawSignature = signed.signature ?? [];
            publicKey = signed.publicKey ?? provider.publicKey;
        }
        const account = publicKey?.toBase58 ? publicKey.toBase58() : String(publicKey ?? "");
        return {
            ok: true,
            value: {
                wallet: "ed25519",
                account,
                accountType: "ed25519",
                signature: Array.from(rawSignature),
                origin: location.origin,
            },
        };
    }
    catch (error) {
        return {
            ok: false,
            error: error instanceof Error ? error.message : String(error),
        };
    }
}
/**
 * Normalizes a provider-selection message into retained service-worker state.
 */
function providerSelection(message) {
    const providerId = typeof message.providerId === "string" ? message.providerId : "";
    if (typeof message.tabId === "number" && Number.isInteger(message.tabId)) {
        return { providerId, tabId: message.tabId };
    }
    return { providerId };
}
/**
 * Narrows an unknown runtime message to an icon-state update.
 */
function isIconSetMessage(message) {
    return isRecord(message) && message["type"] === ICON_SET;
}
/**
 * Narrows an unknown runtime message to an offscreen creation request.
 */
function isEnsureOffscreenMessage(message) {
    return isRecord(message) && message["type"] === NODE_ENSURE_OFFSCREEN;
}
/**
 * Narrows an unknown runtime message to the wallet bridge message union.
 */
function isWalletMessage(message) {
    return (isRecord(message) &&
        (message["type"] === WALLET_CONNECT ||
            message["type"] === WALLET_SIGN ||
            message["type"] === WALLET_SELECT_PROVIDER));
}
/**
 * Narrows an unknown value to a supported extension icon state.
 */
function isIconState(value) {
    return typeof value === "string" && ICON_STATES.has(value);
}
/**
 * Narrows an unknown value to a supported wallet kind.
 */
function walletKind(value) {
    if (value === "eip191" || value === "metamask" || value === "ed25519" || value === "phantom") {
        return value;
    }
    return undefined;
}
/**
 * Promise wrapper around chrome.tabs.get for callback-style extension APIs.
 */
function getTab(tabId) {
    return new Promise((resolve, reject) => {
        chrome.tabs.get(tabId, (tab) => {
            const runtimeError = chrome.runtime.lastError;
            if (runtimeError) {
                reject(new Error(runtimeError.message));
                return;
            }
            resolve(tab);
        });
    });
}
/**
 * Narrows an unknown value to a non-null object record.
 */
function isRecord(value) {
    return typeof value === "object" && value !== null;
}
/**
 * Converts an unknown thrown value into a message string.
 */
function errorMessage(error) {
    return error instanceof Error ? error.message : String(error);
}
