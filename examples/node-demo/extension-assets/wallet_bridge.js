function sendWalletMessage(message) {
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
