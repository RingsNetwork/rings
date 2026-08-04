/** Pure Service Worker request classification and response rendering contracts. */

import assert from "node:assert/strict";
import vm from "node:vm";

import { verifyWebviewHostAsset } from "./test-webview-host.mjs";
import { bytes, request, type ServiceWorkerTestApi, text } from "./webview-service-worker-fixtures.mjs";

/** Request-body helpers installed by the dedicated worker asset. */
export type WorkerRequestApi = {
  readonly gatewayRequestBodyLimitBytes: number;
  readonly isGatewayRequestBodyTooLarge: (error: unknown) => boolean;
  readonly readGatewayRequestBody: (
    request: {
      readonly method: string;
      readonly headers: Headers;
      readonly body: ReadableStream<Uint8Array> | null;
    },
    signal?: AbortSignal,
  ) => Promise<ArrayBuffer | undefined>;
};

/** Dependencies shared with the stateful Service Worker VM harness. */
type StaticServiceWorkerTestInputs = {
  readonly api: ServiceWorkerTestApi;
  readonly canonicalGatewayCsp: string;
  readonly globalThisKey: string;
  readonly hostAssetPath: string;
  readonly hostAssetSource: string;
  readonly workerRequestApi: WorkerRequestApi;
};

/** Run pure request, response rendering, CSP, and history-guard contracts. */
export async function runStaticServiceWorkerTests({
  api,
  canonicalGatewayCsp,
  globalThisKey,
  hostAssetPath,
  hostAssetSource,
  workerRequestApi,
}: StaticServiceWorkerTestInputs): Promise<void> {
  const { controlledNavigationBody, gatewayContentSecurityPolicy, gatewayFailureDocument, requestKind } = api;

  assert.equal(
    gatewayContentSecurityPolicy,
    canonicalGatewayCsp,
    "Rust and Service Worker gateway CSP must match the canonical policy",
  );

  {
    const limit = workerRequestApi.gatewayRequestBodyLimitBytes;
    const requestWithChunks = (chunks: Uint8Array[]) => ({
      method: "POST",
      headers: new Headers(),
      body: new ReadableStream<Uint8Array>({
        start(controller) {
          for (const chunk of chunks) controller.enqueue(chunk);
          controller.close();
        },
      }),
    });
    const exact = await workerRequestApi.readGatewayRequestBody(requestWithChunks([new Uint8Array(limit)]));
    assert.equal(exact?.byteLength, limit);
    await assert.rejects(
      workerRequestApi.readGatewayRequestBody(requestWithChunks([new Uint8Array(limit), new Uint8Array(1)])),
      workerRequestApi.isGatewayRequestBodyTooLarge,
    );

    let cancelled = false;
    const controller = new AbortController();
    const pending = workerRequestApi.readGatewayRequestBody(
      {
        method: "POST",
        headers: new Headers(),
        body: new ReadableStream<Uint8Array>({
          cancel() {
            cancelled = true;
          },
        }),
      },
      controller.signal,
    );
    controller.abort();
    assert.equal((await pending)?.byteLength, 0);
    assert.equal(cancelled, true);
  }

  /**
   * Runs the injected history guard in a small browser-like VM.
   */
  function runHistoryGuard(html: string, locationHref: string): unknown[][] {
    const script = html.match(/<script data-rings-webview-history-guard>([\s\S]*?)<\/script>/)?.[1];
    assert(script, "history guard script was not injected");
    const calls: unknown[][] = [];
    class HistoryFixture {
      pushState(...args: unknown[]): void {
        calls.push(["pushState", ...args]);
      }

      replaceState(...args: unknown[]): void {
        calls.push(["replaceState", ...args]);
      }
    }
    const historyContext: Record<string, unknown> = {
      calls,
      DOMException,
      History: HistoryFixture,
      history: new HistoryFixture(),
      location: new URL(locationHref),
      Object,
      Reflect,
      URL,
    };
    historyContext[globalThisKey] = historyContext;
    vm.runInNewContext(script, historyContext, {
      filename: "rings-webview-history-guard.js",
    });
    vm.runInNewContext(
      `
        history.pushState({ page: "search" }, "", "/search?q=test");
        history.replaceState({ page: "hash" }, "", "/#node");
      `,
      historyContext,
      {
        filename: "rings-webview-history-guard-fixture.js",
      },
    );
    return calls;
  }

  await verifyWebviewHostAsset(hostAssetSource, hostAssetPath);

  assert.equal(requestKind(request({ mode: "navigate", destination: "document" })), "navigation");
  assert.equal(requestKind(request({ destination: "style" })), "subresource");
  assert.equal(requestKind(request()), "fetch");
  assert.equal(requestKind(request({ headers: { "X-Rings-Webview-Kind": "fetch" } })), "fetch");
  assert.equal(requestKind(request({ headers: { "X-Rings-Webview-Kind": "xhr" } })), "xhr");
  assert.throws(
    () => requestKind(request({ headers: { "X-Rings-Webview-Kind": "xhr, subresource" } })),
    /invalid X-Rings-Webview-Kind/,
  );
  assert.throws(
    () => requestKind(request({ headers: { "X-Rings-Webview-Kind": "subresource" } })),
    /invalid X-Rings-Webview-Kind/,
  );
  assert.throws(
    () => requestKind(request({ headers: { "X-Rings-Webview-Kind": "xhr, xhr" } })),
    /invalid X-Rings-Webview-Kind/,
  );

  {
    const headers = new Headers({
      "content-encoding": "gzip",
      "content-length": "42",
      "content-security-policy": "default-src 'none'",
      "content-security-policy-report-only": "default-src 'none'",
      "content-type": "text/html; charset=utf-8",
      "x-frame-options": "DENY",
    });
    const body = controlledNavigationBody(
      { kind: "navigation" },
      200,
      headers,
      bytes("<!doctype html><html><head><title>Target</title></head><body>ok</body></html>"),
    );
    const html = text(body);
    assert.match(html, /data-rings-webview-history-guard/);
    assert.match(html, /<script src="\/assets\/webview-overlay\.js"><\/script><\/head>/);
    assert.equal(headers.has("content-length"), false);
    assert.equal(headers.has("content-encoding"), false);
    assert.equal(headers.has("content-security-policy-report-only"), false);
    assert.equal(headers.has("x-frame-options"), false);
    const contentSecurityPolicy = headers.get("content-security-policy") ?? "";
    assert.match(contentSecurityPolicy, /^sandbox /);
    assert.match(contentSecurityPolicy, /script-src 'self'/);
    assert.doesNotMatch(contentSecurityPolicy, /allow-same-origin/);
    assert.equal(headers.get("x-content-type-options"), "nosniff");
  }

  {
    const headers = new Headers({
      "content-length": "42",
      "content-security-policy": "default-src 'none'",
      "content-type": "image/svg+xml",
      "x-frame-options": "DENY",
    });
    const svg = bytes('<svg xmlns="http://www.w3.org/2000/svg"><script>globalThis.pwned = true</script></svg>');
    const body = controlledNavigationBody({ kind: "navigation" }, 200, headers, svg);
    assert.equal(body, svg);
    assert.equal(headers.has("content-length"), false);
    assert.equal(headers.has("x-frame-options"), false);
    assert.match(headers.get("content-security-policy") ?? "", /^sandbox /);
    assert.doesNotMatch(headers.get("content-security-policy") ?? "", /allow-same-origin/);
    assert.equal(headers.get("x-content-type-options"), "nosniff");
  }

  {
    const headers = new Headers({
      "content-length": "42",
      "content-type": "text/html",
    });
    const body = controlledNavigationBody(
      { kind: "navigation", topLevelNavigation: false },
      200,
      headers,
      bytes("<!doctype html><html><head><title>Frame</title></head><body>ok</body></html>"),
    );
    const html = text(body);
    assert.match(html, /data-rings-webview-history-guard/);
    assert.doesNotMatch(html, /\/assets\/webview-overlay\.js/);
    assert.equal(headers.has("content-length"), false);
    assert.match(headers.get("content-security-policy") ?? "", /script-src 'self'/);
  }

  {
    const html =
      '<!doctype html><html><head><script src="/assets/webview-overlay.js"></script></head><body>ok</body></html>';
    const headers = new Headers({
      "content-length": "42",
      "content-security-policy": "default-src 'none'",
      "content-type": "text/html",
    });
    const body = controlledNavigationBody({ kind: "navigation" }, 200, headers, bytes(html));
    const injected = text(body);
    assert.match(injected, /data-rings-webview-history-guard/);
    assert.match(injected, /<script src="\/assets\/webview-overlay\.js"><\/script><\/head>/);
    assert.equal(headers.has("content-length"), false);
    assert.match(headers.get("content-security-policy") ?? "", /script-src 'self'/);
  }

  {
    const headers = new Headers({
      "content-length": "42",
      "content-security-policy": "default-src 'none'",
      "content-type": "text/html",
    });
    const body = controlledNavigationBody(
      { kind: "navigation" },
      200,
      headers,
      bytes(
        "<!doctype html><!-- attacker marker: data-rings-webview-history-guard /assets/webview-overlay.js --><html><head><title>Target</title></head><body>ok</body></html>",
      ),
    );
    const html = text(body);
    const guardIndex = html.indexOf("<script data-rings-webview-history-guard>");
    const attackerMarkerIndex = html.indexOf("attacker marker");
    const overlayIndex = html.lastIndexOf('<script src="/assets/webview-overlay.js"></script>');
    assert.ok(guardIndex >= 0);
    assert.ok(attackerMarkerIndex >= 0);
    assert.ok(overlayIndex > attackerMarkerIndex);
    const historyCalls = runHistoryGuard(
      html,
      "http://127.0.0.1:8080/webview/https%3A%2F%2Ftrusted.example%2Fdocs%2Findex.html",
    );
    assert.equal(historyCalls[0]?.[3], "/webview/https%3A%2F%2Ftrusted.example%2Fsearch%3Fq%3Dtest");
    assert.equal(headers.has("content-length"), false);
  }

  {
    const headers = new Headers({
      "content-length": "42",
      "content-security-policy": "default-src 'none'",
      "content-type": "text/html",
    });
    const body = controlledNavigationBody(
      { kind: "navigation" },
      200,
      headers,
      bytes("\uFEFF<!-- leading comment --><html><head><title>Target</title></head><body>ok</body></html>"),
    );
    const html = text(body);
    assert.match(html, /<script src="\/assets\/webview-overlay\.js"><\/script><\/head>/);
    assert.equal(headers.has("content-length"), false);
    assert.match(headers.get("content-security-policy") ?? "", /script-src 'self'/);
  }

  {
    const headers = new Headers({
      "content-length": "42",
      "content-security-policy": "default-src 'none'",
      "content-type": "text/html",
    });
    const body = controlledNavigationBody({ kind: "navigation" }, 200, headers, bytes("<!-- comment-only fixture -->"));
    assert.equal(text(body), "<!-- comment-only fixture -->");
    assert.equal(headers.has("content-length"), false);
    assert.match(headers.get("content-security-policy") ?? "", /script-src 'self'/);
  }

  {
    const headers = new Headers({
      "content-type": "text/html",
    });
    const body = controlledNavigationBody(
      { kind: "navigation" },
      200,
      headers,
      bytes(
        '<!doctype html><html><head><script data-attacker>history.replaceState(null, "", "/#node")</script></head><body>ok</body></html>',
      ),
    );
    const html = text(body);
    const guardIndex = html.indexOf("data-rings-webview-history-guard");
    const attackerIndex = html.indexOf("data-attacker");
    assert.ok(guardIndex >= 0);
    assert.ok(attackerIndex >= 0);
    assert.ok(guardIndex < attackerIndex);
    const historyCalls = runHistoryGuard(
      html,
      "http://127.0.0.1:8080/webview/https%3A%2F%2Ftrusted.example%2Fdocs%2Findex.html",
    );
    assert.equal(historyCalls[0]?.[0], "pushState");
    assert.equal(historyCalls[0]?.[3], "/webview/https%3A%2F%2Ftrusted.example%2Fsearch%3Fq%3Dtest");
    assert.equal(historyCalls[1]?.[0], "replaceState");
    assert.equal(historyCalls[1]?.[3], "/webview/https%3A%2F%2Ftrusted.example%2F%23node");
  }

  {
    const css = bytes("body { color: red; }");
    const body = controlledNavigationBody(
      { kind: "subresource" },
      200,
      new Headers({ "content-type": "text/css" }),
      css,
    );
    assert.equal(body, css);
  }

  {
    const html = gatewayFailureDocument(
      503,
      'gateway transport failed: no live onion exit offers service "https"',
      "No live HTTPS onion exit is available.",
      "onion_exit_unavailable",
    );
    assert.match(html, /<template[\s\S]*data-rings-webview-failure="true"/);
    assert.match(html, /data-rings-webview-failure-code="onion_exit_unavailable"/);
    assert.doesNotMatch(html, /<h1\b/i);
    assert.doesNotMatch(html, /<main\b/i);
    assert.doesNotMatch(html, /<p\b/i);
  }
}
