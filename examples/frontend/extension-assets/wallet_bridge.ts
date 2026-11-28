/**
 * Exposes the extension wallet bridge used by the Rings Yew app.
 */

/**
 * Wallet kinds accepted by the browser-extension wallet bridge.
 */
type WalletBridgeWalletKind = "eip191" | "metamask" | "ed25519" | "phantom";

/**
 * Message envelope sent from the extension page to the MV3 service worker.
 */
type WalletBridgeRuntimeMessage = {
  readonly type: string;
  readonly [key: string]: unknown;
};

/**
 * Standard callback response shape returned by the service worker bridge.
 */
type WalletBridgeRuntimeResponse<T> =
  | {
      readonly ok: true;
      readonly result: T;
    }
  | {
      readonly ok: false;
      readonly error?: string;
    };

/**
 * Global wallet bridge surface consumed from Rust through wasm-bindgen.
 */
type WalletBridge = {
  resetProvider(wallet: WalletBridgeWalletKind): Promise<unknown>;
  connect(wallet: WalletBridgeWalletKind): Promise<unknown>;
  sign(wallet: WalletBridgeWalletKind, proof: string, account?: string): Promise<unknown>;
};

/**
 * Sends one wallet bridge request through chrome.runtime.sendMessage.
 */
function sendWalletMessage<T>(message: WalletBridgeRuntimeMessage): Promise<T> {
  if (!globalThis.chrome?.runtime?.sendMessage) {
    return Promise.reject(new Error("Rings extension wallet bridge is unavailable"));
  }
  return new Promise<T>((resolve, reject): void => {
    chrome.runtime.sendMessage(message, (response: WalletBridgeRuntimeResponse<T> | undefined): void => {
      const runtimeError = chrome.runtime.lastError;
      if (runtimeError) {
        reject(new Error(runtimeError.message));
        return;
      }
      if (!response || response.ok === false) {
        reject(new Error(response?.error || "wallet bridge failed"));
        return;
      }
      resolve(response.result);
    });
  });
}

/**
 * Returns true when the wallet kind uses the EIP-191 Ethereum request path.
 */
function isEip191Wallet(wallet: WalletBridgeWalletKind): boolean {
  return wallet === "eip191" || wallet === "metamask";
}

const walletBridge: WalletBridge = {
  async resetProvider(wallet: WalletBridgeWalletKind): Promise<unknown> {
    if (!isEip191Wallet(wallet)) {
      return null;
    }
    return sendWalletMessage({
      type: "rings.wallet.selectProvider",
      providerId: "",
    });
  },
  async connect(wallet: WalletBridgeWalletKind): Promise<unknown> {
    if (isEip191Wallet(wallet)) {
      await this.resetProvider(wallet).catch((): void => {});
    }
    try {
      return await sendWalletMessage({
        type: "rings.wallet.connect",
        wallet,
      });
    } catch (error: unknown) {
      if (isEip191Wallet(wallet)) {
        await this.resetProvider(wallet).catch((): void => {});
      }
      throw error;
    }
  },
  async sign(wallet: WalletBridgeWalletKind, proof: string, account = ""): Promise<unknown> {
    return sendWalletMessage({
      type: "rings.wallet.sign",
      wallet,
      proof,
      account,
    });
  },
};

Object.assign(globalThis, {
  RingsExtensionWalletBridge: walletBridge,
});
