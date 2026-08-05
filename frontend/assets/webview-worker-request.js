(() => {
  "use strict";

  const gatewayRequestBodyLimitBytes = 8 * 1024 * 1024;

  class GatewayRequestBodyTooLarge extends Error {}

  function gatewayRequestMayHaveBody(request) {
    return request.method !== "GET" && request.method !== "HEAD";
  }

  function validateGatewayRequestBodyMetadata(request) {
    if (!gatewayRequestMayHaveBody(request)) {
      return;
    }
    rejectOversizedDeclaredBody(request.headers.get("content-length"));
  }

  async function readGatewayRequestBody(request, signal = undefined) {
    if (!gatewayRequestMayHaveBody(request)) {
      return undefined;
    }
    validateGatewayRequestBodyMetadata(request);
    // Consume the original stream. `Request::clone` tees the body; leaving the
    // other branch unread would let the platform buffer it without this
    // function's byte budget and defeat the resource bound.
    const stream = request.body;
    if (!stream) {
      return new ArrayBuffer(0);
    }

    const reader = stream.getReader();
    const chunks = [];
    let total = 0;
    const cancel = () => {
      void reader.cancel("Rings gateway request cancelled");
    };
    signal?.addEventListener("abort", cancel, { once: true });
    try {
      if (signal?.aborted) {
        cancel();
        return new ArrayBuffer(0);
      }
      while (true) {
        let result;
        try {
          result = await reader.read();
        } catch (error) {
          if (signal?.aborted) {
            return new ArrayBuffer(0);
          }
          throw error;
        }
        if (signal?.aborted) {
          return new ArrayBuffer(0);
        }
        if (result.done) {
          break;
        }
        const chunk = byteView(result.value);
        if (chunk.byteLength > gatewayRequestBodyLimitBytes - total) {
          void reader.cancel("Rings gateway request body limit exceeded");
          throw new GatewayRequestBodyTooLarge(
            `gateway request body exceeds ${gatewayRequestBodyLimitBytes} bytes`,
          );
        }
        chunks.push(chunk);
        total += chunk.byteLength;
      }
    } finally {
      signal?.removeEventListener("abort", cancel);
      reader.releaseLock();
    }

    const body = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      body.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return body.buffer;
  }

  function byteView(value) {
    if (value instanceof Uint8Array) {
      return value;
    }
    if (ArrayBuffer.isView(value)) {
      return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    }
    return new Uint8Array(value);
  }

  function rejectOversizedDeclaredBody(rawLength) {
    if (rawLength == null) {
      return;
    }
    const decimal = rawLength.trim();
    if (!/^\d+$/.test(decimal)) {
      throw new Error("invalid gateway request Content-Length");
    }
    const normalized = decimal.replace(/^0+(?=\d)/, "");
    const limit = String(gatewayRequestBodyLimitBytes);
    if (
      normalized.length > limit.length
      || (normalized.length === limit.length && normalized > limit)
    ) {
      throw new GatewayRequestBodyTooLarge(
        `gateway request body exceeds ${gatewayRequestBodyLimitBytes} bytes`,
      );
    }
  }

  function isGatewayRequestBodyTooLarge(error) {
    return error instanceof GatewayRequestBodyTooLarge;
  }

  self.RingsWebviewWorkerRequest = Object.freeze({
    gatewayRequestBodyLimitBytes,
    gatewayRequestMayHaveBody,
    isGatewayRequestBodyTooLarge,
    readGatewayRequestBody,
    validateGatewayRequestBodyMetadata,
  });
})();
