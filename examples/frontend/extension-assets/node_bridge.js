let startPromise;
let iconWatchPromise;
const NODE_START_ICON_POLL_ATTEMPTS = 240;
const NODE_START_ICON_POLL_DELAY_MS = 750;

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
    for (let attempt = 0; attempt < NODE_START_ICON_POLL_ATTEMPTS; attempt += 1) {
      await delay(NODE_START_ICON_POLL_DELAY_MS);
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
