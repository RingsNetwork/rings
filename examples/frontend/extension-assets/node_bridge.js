"use strict";
/**
 * Exposes the extension node bridge used by the Rings Yew app.
 */
let startPromise;
let iconWatchPromise;
const NODE_START_ICON_POLL_ATTEMPTS = 240;
const NODE_START_ICON_POLL_DELAY_MS = 750;
/**
 * Sends one runtime message and unwraps the extension callback response.
 */
function sendNodeBridgeRuntimeMessage(message) {
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
            if (!response || response.ok === false) {
                reject(new Error(response?.error || "node bridge failed"));
                return;
            }
            resolve(response.result);
        });
    });
}
/**
 * Best-effort update for the extension action icon.
 */
async function setExtensionNodeBridgeIconState(state) {
    try {
        await sendNodeBridgeRuntimeMessage({
            type: "rings.icon.set",
            state,
        });
    }
    catch (error) {
        console.warn("Rings extension icon update failed", error);
    }
}
/**
 * Sleeps for a fixed number of milliseconds.
 */
function delay(ms) {
    return new Promise((resolve) => {
        setTimeout(resolve, ms);
    });
}
/**
 * Ensures the offscreen document hosting the retained browser node exists.
 */
async function ensureOffscreenNode() {
    await sendNodeBridgeRuntimeMessage({ type: "rings.node.ensureOffscreen" });
}
/**
 * Returns true for transient offscreen-document message delivery failures.
 */
function shouldRetryNodeMessage(error) {
    const message = error instanceof Error ? error.message : String(error);
    return (message.includes("Receiving end does not exist") ||
        message.includes("message port closed") ||
        message.includes("The message port closed"));
}
/**
 * Sends a node message through the retrying offscreen bridge path.
 */
async function sendNodeMessage(message) {
    return sendNodeMessageWithRetry(message);
}
/**
 * Adds the offscreen target marker before sending a runtime message.
 */
async function sendNodeMessageToOffscreen(message) {
    return sendNodeBridgeRuntimeMessage({
        ...message,
        target: "rings.node.offscreen",
    });
}
/**
 * Retries messages while Chrome is still creating or waking the offscreen document.
 */
async function sendNodeMessageWithRetry(message) {
    await ensureOffscreenNode();
    let lastError;
    for (let attempt = 0; attempt < 25; attempt += 1) {
        try {
            return await sendNodeMessageToOffscreen(message);
        }
        catch (error) {
            lastError = error;
            if (!shouldRetryNodeMessage(error)) {
                throw error;
            }
            await delay(120);
        }
    }
    throw lastError ?? new Error("node bridge did not respond");
}
/**
 * Converts a node snapshot into the corresponding action icon state.
 */
function iconStateFromSnapshot(snapshot) {
    if (snapshot?.online) {
        return "connected";
    }
    if (snapshot?.starting) {
        return "connecting";
    }
    return "disconnected";
}
/**
 * Refreshes the node snapshot and mirrors its state to the extension icon.
 */
async function refreshNodeIcon() {
    const snapshot = await sendNodeMessageWithRetry({
        type: "rings.node.status",
    });
    await setExtensionNodeBridgeIconState(iconStateFromSnapshot(snapshot));
    return snapshot;
}
/**
 * Polls node state while startup is still settling so the icon eventually converges.
 */
function watchNodeIconUntilSettled() {
    if (iconWatchPromise) {
        return;
    }
    iconWatchPromise = (async () => {
        for (let attempt = 0; attempt < NODE_START_ICON_POLL_ATTEMPTS; attempt += 1) {
            await delay(NODE_START_ICON_POLL_DELAY_MS);
            let snapshot;
            try {
                snapshot = await refreshNodeIcon();
            }
            catch (error) {
                await setExtensionNodeBridgeIconState("disconnected");
                return;
            }
            if (!snapshot.starting) {
                return;
            }
        }
        await setExtensionNodeBridgeIconState("disconnected");
    })().finally(() => {
        iconWatchPromise = undefined;
    });
}
/**
 * Starts the retained browser node in the offscreen document.
 */
async function startNode(settings) {
    await setExtensionNodeBridgeIconState("connecting");
    try {
        const walletBridge = globalThis.RingsExtensionWalletBridge;
        if ((settings.walletKind === "eip191" || settings.walletKind === "metamask") && walletBridge?.resetProvider) {
            await walletBridge.resetProvider(settings.walletKind);
        }
        const snapshot = await sendNodeMessageWithRetry({
            type: "rings.node.start",
            settings,
        });
        await setExtensionNodeBridgeIconState(iconStateFromSnapshot(snapshot));
        if (snapshot.starting) {
            watchNodeIconUntilSettled();
        }
        return snapshot;
    }
    catch (error) {
        await setExtensionNodeBridgeIconState("disconnected");
        throw error;
    }
}
/**
 * Stops the retained browser node and marks the extension as disconnected.
 */
async function stopNode() {
    try {
        const result = await sendNodeMessage({
            type: "rings.node.stop",
        });
        await setExtensionNodeBridgeIconState("disconnected");
        return result;
    }
    catch (error) {
        await setExtensionNodeBridgeIconState("disconnected");
        throw error;
    }
}
/**
 * Reads node status and updates the extension icon from the latest snapshot.
 */
async function nodeStatus() {
    const snapshot = await sendNodeMessage({
        type: "rings.node.status",
    });
    await setExtensionNodeBridgeIconState(iconStateFromSnapshot(snapshot));
    return snapshot;
}
const nodeBridge = {
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
Object.assign(globalThis, {
    RingsExtensionNodeBridge: nodeBridge,
});
