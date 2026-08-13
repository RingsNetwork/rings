(() => {
  "use strict";

  function gatewayFailure(status, message, summary = undefined, code = undefined) {
    return new Response(gatewayFailureDocument(
      status,
      gatewayFailureSummary(message),
      gatewayFailureReason(status, message, summary),
      code || gatewayFailureCode(status),
    ), {
      status,
      headers: {
        "content-type": "text/html; charset=utf-8",
        "cache-control": "no-store",
        "referrer-policy": "no-referrer",
      },
    });
  }

  function gatewayFailureSummary(message) {
    let text = String(message || "gateway request failed").trim();
    text = text.replace(
      /^gateway transport:\s+gateway transport failed:/,
      "gateway transport failed:",
    );
    text = text.replace(
      /JsValue\(Error: ([^\n)]*)[\s\S]*\)/,
      "Error: $1",
    );
    const firstLine = text
      .split(/\r?\n/)
      .map((line) => line.trim())
      .find(Boolean);
    return firstLine || "gateway request failed";
  }

  function gatewayFailureReason(status, message, summary) {
    const cleanSummary = String(summary || "").trim();
    if (cleanSummary) return cleanSummary;
    const detail = gatewayFailureSummary(message);
    if (status === 503 && detail.includes("no live onion exit offers service \"https\"")) {
      return "No live HTTPS onion exit is available.";
    }
    if (status === 503 && detail.includes("no live onion exit")) {
      return "No live onion exit is available for this request.";
    }
    if (status === 503 && detail.includes("Start a local Rings node")) {
      return "Local Rings node gateway is unavailable.";
    }
    if (status === 502) return "Gateway transport failed.";
    if (status === 504) return "Gateway request timed out.";
    return "Gateway request failed.";
  }

  function gatewayFailureCode(status) {
    if (status === 400) return "invalid_webview_request";
    if (status === 403) return "webview_request_rejected";
    if (status === 404) return "controlled_asset_not_found";
    if (status === 502) return "gateway_transport_failed";
    if (status === 503) return "gateway_unavailable";
    if (status === 504) return "gateway_timeout";
    return "gateway_request_failed";
  }

  function gatewayFailureDocument(status, message, reason, code) {
    const detail = escapeHtml(message);
    const summary = escapeHtml(reason);
    const reasonCode = escapeHtml(code);
    const statusText = escapeHtml(status);
    return `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Rings gateway failure ${statusText}</title>
<style>
  body { margin: 0; min-height: 100vh; background: #fffaf0; }
</style>
<body>
<template
  data-rings-webview-failure="true"
  data-rings-webview-failure-status="${statusText}"
  data-rings-webview-failure-code="${reasonCode}"
  data-rings-webview-failure-summary="${summary}"
  data-rings-webview-failure-detail="${detail}"
></template>
<script src="/assets/webview-overlay.js"></script>
</body>
</html>`;
  }

  function escapeHtml(value) {
    return String(value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  Object.defineProperty(self, "RingsWebviewWorkerResponse", {
    value: Object.freeze({
      gatewayFailure,
      gatewayFailureDocument,
    }),
    configurable: false,
    writable: false,
  });
})();
