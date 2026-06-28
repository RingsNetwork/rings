export const EXAMPLE_NAMESPACE = "example";

export function stringify(value) {
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

export function hexToBytes(hex) {
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

export function bytesToBase64(bytes) {
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary);
}

export function connectSeedRequest(url) {
  return {
    method: "connectPeerViaHttp",
    params: { url: url.trim() },
  };
}

export function connectDidRequest(did) {
  return {
    method: "connectWithDid",
    params: { did: did.trim() },
  };
}

export function sendExampleMessageRequest(destinationDid, payload) {
  return {
    method: "sendBackendMessage",
    params: {
      destination_did: destinationDid.trim(),
      namespace: EXAMPLE_NAMESPACE,
      data: bytesToBase64(payload),
    },
  };
}

export function createExampleHandler(log, decoder = new TextDecoder()) {
  return (ctx, event) => {
    const text = decoder.decode(event.payload);
    log(`received ${EXAMPLE_NAMESPACE} message from ${event.from}`, text);
    return {
      state: { received: Number(ctx.state?.received || 0) + 1 },
      effects: [],
    };
  };
}
