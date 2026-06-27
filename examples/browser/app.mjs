import init, { debug, Provider } from "../../crates/node/pkg/rings_node.js";
import { Wallet } from "https://cdn.jsdelivr.net/npm/ethers@6.13.5/+esm";

const EXAMPLE_NAMESPACE = "example";

const refs = {
  networkId: document.getElementById("network-id"),
  stabilizeInterval: document.getElementById("stabilize-interval"),
  iceServers: document.getElementById("ice-servers"),
  startProvider: document.getElementById("start-provider"),
  copyDid: document.getElementById("copy-did"),
  localDid: document.getElementById("local-did"),
  seedUrl: document.getElementById("seed-url"),
  remoteDid: document.getElementById("remote-did"),
  connectSeed: document.getElementById("connect-seed"),
  connectDid: document.getElementById("connect-did"),
  listPeers: document.getElementById("list-peers"),
  message: document.getElementById("message"),
  sendMessage: document.getElementById("send-message"),
  log: document.getElementById("log"),
};

const state = {
  provider: null,
  wallet: null,
  listen: null,
};

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function log(message, detail) {
  const time = new Date().toLocaleTimeString();
  const suffix = detail === undefined ? "" : `\n${stringify(detail)}`;
  refs.log.textContent += `[${time}] ${message}${suffix}\n`;
  refs.log.scrollTop = refs.log.scrollHeight;
}

function stringify(value) {
  if (value instanceof Error) {
    return value.stack || value.message;
  }
  if (typeof value === "string") {
    return value;
  }
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function hexToBytes(hex) {
  const clean = hex.startsWith("0x") ? hex.slice(2) : hex;
  if (!/^[0-9a-fA-F]*$/.test(clean) || clean.length % 2 !== 0) {
    throw new Error("signature is not even-length hex");
  }
  const bytes = new Uint8Array(clean.length / 2);
  for (let offset = 0; offset < clean.length; offset += 2) {
    bytes[offset / 2] = Number.parseInt(clean.slice(offset, offset + 2), 16);
  }
  return bytes;
}

function bytesToBase64(bytes) {
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary);
}

function requireProvider() {
  if (!state.provider) {
    throw new Error("start the provider first");
  }
  return state.provider;
}

function setStarted(started) {
  refs.startProvider.disabled = started;
  refs.copyDid.disabled = !started;
  refs.connectSeed.disabled = !started;
  refs.connectDid.disabled = !started;
  refs.listPeers.disabled = !started;
  refs.sendMessage.disabled = !started;
}

async function startProvider() {
  await init();
  debug(true);

  const wallet = Wallet.createRandom();
  const signer = async (proof) => hexToBytes(await wallet.signMessage(proof));
  const provider = await new Provider(
    Number(refs.networkId.value),
    refs.iceServers.value,
    BigInt(refs.stabilizeInterval.value),
    wallet.address,
    "eip191",
    signer,
  );

  provider.on(EXAMPLE_NAMESPACE, { received: 0 }, (ctx, event) => {
    const text = decoder.decode(event.payload);
    log(`received ${EXAMPLE_NAMESPACE} message from ${event.from}`, text);
    return {
      state: { received: Number(ctx.state?.received || 0) + 1 },
      effects: [],
    };
  });

  state.provider = provider;
  state.wallet = wallet;
  state.listen = provider.listen();
  state.listen.catch((error) => log("listen task stopped", error));

  refs.localDid.textContent = provider.address();
  setStarted(true);
  log("provider started", { did: provider.address(), account: wallet.address });
}

async function connectSeed() {
  const provider = requireProvider();
  const url = refs.seedUrl.value.trim();
  const response = await provider.request("connectPeerViaHttp", { url });
  log("connected to seed", response);
  await refreshPeers();
}

async function connectDid() {
  const provider = requireProvider();
  const did = refs.remoteDid.value.trim();
  const response = await provider.request("connectWithDid", { did });
  log("connectWithDid returned", response);
  await refreshPeers();
}

async function refreshPeers() {
  const provider = requireProvider();
  const response = await provider.request("listPeers", {});
  log("peers", response);
}

async function sendMessage() {
  const provider = requireProvider();
  const destinationDid = refs.remoteDid.value.trim();
  const payload = encoder.encode(refs.message.value);
  const response = await provider.request("sendBackendMessage", {
    destination_did: destinationDid,
    namespace: EXAMPLE_NAMESPACE,
    data: bytesToBase64(payload),
  });
  log("message sent", response);
}

async function copyDid() {
  const did = refs.localDid.textContent;
  await navigator.clipboard.writeText(did);
  log("copied local DID");
}

function bind(id, action) {
  refs[id].addEventListener("click", async () => {
    refs[id].disabled = true;
    try {
      await action();
    } catch (error) {
      log("error", error);
    } finally {
      if (id === "startProvider") {
        refs[id].disabled = Boolean(state.provider);
      } else {
        refs[id].disabled = !state.provider;
      }
    }
  });
}

bind("startProvider", startProvider);
bind("connectSeed", connectSeed);
bind("connectDid", connectDid);
bind("listPeers", refreshPeers);
bind("sendMessage", sendMessage);
bind("copyDid", copyDid);

setStarted(false);
log("build crates/node first, then serve the repository root over HTTP");
