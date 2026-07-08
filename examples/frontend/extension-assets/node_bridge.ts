/**
 * Exposes the extension node bridge used by the Rings Yew app.
 */

/**
 * Message envelope sent from the extension page to the MV3 service worker or offscreen node.
 */
type NodeBridgeRuntimeMessage = {
  readonly type: string;
  readonly [key: string]: unknown;
};

/**
 * Standard callback response shape returned by extension runtime messages.
 */
type NodeBridgeRuntimeResponse<T> =
  | {
      readonly ok: true;
      readonly result: T;
    }
  | {
      readonly ok: false;
      readonly error?: string;
    };

/**
 * Extension action icon states derived from the retained node snapshot.
 */
type NodeBridgeIconState = "disconnected" | "connecting" | "connected";

/**
 * Minimal node snapshot fields needed by the bridge and icon state machine.
 */
type NodeSnapshot = {
  readonly online?: boolean;
  readonly starting?: boolean;
  readonly [key: string]: unknown;
};

/**
 * Node start settings forwarded from Rust/Yew to the offscreen node host.
 */
type NodeSettings = {
  readonly walletKind?: string;
  readonly [key: string]: unknown;
};

/**
 * Optional global wallet bridge used to reset EIP-191 provider selection before node start.
 */
type WalletBridgeGlobal = {
  readonly RingsExtensionWalletBridge?: {
    resetProvider(wallet: string): Promise<unknown>;
  };
};

/**
 * Global node bridge surface consumed from Rust through wasm-bindgen.
 */
type NodeBridge = {
  start(settings: NodeSettings): Promise<NodeSnapshot>;
  stop(): Promise<unknown>;
  status(): Promise<NodeSnapshot>;
  connectHttp(endpoint: string): Promise<unknown>;
  createOffer(did: string): Promise<unknown>;
  answerOffer(offer: string): Promise<unknown>;
  acceptAnswer(answer: string): Promise<unknown>;
};

let startPromise: Promise<NodeSnapshot> | undefined;
let iconWatchPromise: Promise<void> | undefined;
const NODE_START_ICON_POLL_ATTEMPTS = 240;
const NODE_START_ICON_POLL_DELAY_MS = 750;

/**
 * Sends one runtime message and unwraps the extension callback response.
 */
function sendNodeBridgeRuntimeMessage<T>(message: NodeBridgeRuntimeMessage): Promise<T> {
  if (!globalThis.chrome?.runtime?.sendMessage) {
    return Promise.reject(new Error("Rings extension node bridge is unavailable"));
  }
  return new Promise<T>((resolve, reject): void => {
    chrome.runtime.sendMessage(message, (response: NodeBridgeRuntimeResponse<T> | undefined): void => {
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
async function setExtensionNodeBridgeIconState(state: NodeBridgeIconState): Promise<void> {
  try {
    await sendNodeBridgeRuntimeMessage({
      type: "rings.icon.set",
      state,
    });
  } catch (error: unknown) {
    console.warn("Rings extension icon update failed", error);
  }
}

/**
 * Sleeps for a fixed number of milliseconds.
 */
function delay(ms: number): Promise<void> {
  return new Promise((resolve): void => {
    setTimeout(resolve, ms);
  });
}

/**
 * Ensures the offscreen document hosting the retained browser node exists.
 */
async function ensureOffscreenNode(): Promise<void> {
  await sendNodeBridgeRuntimeMessage({ type: "rings.node.ensureOffscreen" });
}

/**
 * Returns true for transient offscreen-document message delivery failures.
 */
function shouldRetryNodeMessage(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return (
    message.includes("Receiving end does not exist") ||
    message.includes("message port closed") ||
    message.includes("The message port closed")
  );
}

/**
 * Sends a node message through the retrying offscreen bridge path.
 */
async function sendNodeMessage<T>(message: NodeBridgeRuntimeMessage): Promise<T> {
  return sendNodeMessageWithRetry(message);
}

/**
 * Adds the offscreen target marker before sending a runtime message.
 */
async function sendNodeMessageToOffscreen<T>(message: NodeBridgeRuntimeMessage): Promise<T> {
  return sendNodeBridgeRuntimeMessage({
    ...message,
    target: "rings.node.offscreen",
  });
}

/**
 * Retries messages while Chrome is still creating or waking the offscreen document.
 */
async function sendNodeMessageWithRetry<T>(message: NodeBridgeRuntimeMessage): Promise<T> {
  await ensureOffscreenNode();
  let lastError: unknown;
  for (let attempt = 0; attempt < 25; attempt += 1) {
    try {
      return await sendNodeMessageToOffscreen<T>(message);
    } catch (error: unknown) {
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
function iconStateFromSnapshot(snapshot: NodeSnapshot | undefined): NodeBridgeIconState {
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
async function refreshNodeIcon(): Promise<NodeSnapshot> {
  const snapshot = await sendNodeMessageWithRetry<NodeSnapshot>({
    type: "rings.node.status",
  });
  await setExtensionNodeBridgeIconState(iconStateFromSnapshot(snapshot));
  return snapshot;
}

/**
 * Polls node state while startup is still settling so the icon eventually converges.
 */
function watchNodeIconUntilSettled(): void {
  if (iconWatchPromise) {
    return;
  }
  iconWatchPromise = (async (): Promise<void> => {
    for (let attempt = 0; attempt < NODE_START_ICON_POLL_ATTEMPTS; attempt += 1) {
      await delay(NODE_START_ICON_POLL_DELAY_MS);
      let snapshot: NodeSnapshot;
      try {
        snapshot = await refreshNodeIcon();
      } catch (error: unknown) {
        await setExtensionNodeBridgeIconState("disconnected");
        return;
      }
      if (!snapshot.starting) {
        return;
      }
    }
    await setExtensionNodeBridgeIconState("disconnected");
  })().finally((): void => {
    iconWatchPromise = undefined;
  });
}

/**
 * Starts the retained browser node in the offscreen document.
 */
async function startNode(settings: NodeSettings): Promise<NodeSnapshot> {
  await setExtensionNodeBridgeIconState("connecting");
  try {
    const walletBridge = (globalThis as typeof globalThis & WalletBridgeGlobal).RingsExtensionWalletBridge;
    if ((settings.walletKind === "eip191" || settings.walletKind === "metamask") && walletBridge?.resetProvider) {
      await walletBridge.resetProvider(settings.walletKind);
    }
    const snapshot = await sendNodeMessageWithRetry<NodeSnapshot>({
      type: "rings.node.start",
      settings,
    });
    await setExtensionNodeBridgeIconState(iconStateFromSnapshot(snapshot));
    if (snapshot.starting) {
      watchNodeIconUntilSettled();
    }
    return snapshot;
  } catch (error: unknown) {
    await setExtensionNodeBridgeIconState("disconnected");
    throw error;
  }
}

/**
 * Stops the retained browser node and marks the extension as disconnected.
 */
async function stopNode(): Promise<unknown> {
  try {
    const result = await sendNodeMessage({
      type: "rings.node.stop",
    });
    await setExtensionNodeBridgeIconState("disconnected");
    return result;
  } catch (error: unknown) {
    await setExtensionNodeBridgeIconState("disconnected");
    throw error;
  }
}

/**
 * Reads node status and updates the extension icon from the latest snapshot.
 */
async function nodeStatus(): Promise<NodeSnapshot> {
  const snapshot = await sendNodeMessage<NodeSnapshot>({
    type: "rings.node.status",
  });
  await setExtensionNodeBridgeIconState(iconStateFromSnapshot(snapshot));
  return snapshot;
}

const nodeBridge: NodeBridge = {
  start(settings: NodeSettings): Promise<NodeSnapshot> {
    if (!startPromise) {
      startPromise = startNode(settings).finally((): void => {
        startPromise = undefined;
      });
    }
    return startPromise;
  },
  stop(): Promise<unknown> {
    return stopNode();
  },
  status(): Promise<NodeSnapshot> {
    return nodeStatus();
  },
  connectHttp(endpoint: string): Promise<unknown> {
    return sendNodeMessage({
      type: "rings.node.connectHttp",
      endpoint,
    });
  },
  createOffer(did: string): Promise<unknown> {
    return sendNodeMessage({
      type: "rings.node.createOffer",
      did,
    });
  },
  answerOffer(offer: string): Promise<unknown> {
    return sendNodeMessage({
      type: "rings.node.answerOffer",
      offer,
    });
  },
  acceptAnswer(answer: string): Promise<unknown> {
    return sendNodeMessage({
      type: "rings.node.acceptAnswer",
      answer,
    });
  },
};

Object.assign(globalThis, {
  RingsExtensionNodeBridge: nodeBridge,
});
