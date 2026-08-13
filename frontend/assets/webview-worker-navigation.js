(() => {
  "use strict";

  const webviewHistoryGuardMarker = "data-rings-webview-history-guard";
  const webviewHistoryGuardSource = `(() => {
    "use strict";
    if (globalThis.__ringsWebviewHistoryGuard) return;
    Object.defineProperty(globalThis, "__ringsWebviewHistoryGuard", { value: true });
    const gatewayPrefix = "/webview/";
    function currentTargetUrl() {
      if (!location.pathname.startsWith(gatewayPrefix)) return undefined;
      const encoded = location.pathname.slice(gatewayPrefix.length);
      if (!encoded) return undefined;
      try {
        const target = new URL(decodeURIComponent(encoded));
        if (location.search) {
          const extra = location.search.slice(1);
          target.search = target.search ? target.search + "&" + extra : "?" + extra;
        }
        if (!target.hash && location.hash) target.hash = location.hash;
        return target;
      } catch (_error) {
        return undefined;
      }
    }
    function controlledGatewayUrl(target) {
      return gatewayPrefix + encodeURIComponent(target.href);
    }
    function rewrittenHistoryUrl(value) {
      const currentTarget = currentTargetUrl();
      if (!currentTarget) return value;
      const target = new URL(value, currentTarget.href);
      if (target.origin !== currentTarget.origin) {
        throw new DOMException("Rings WebView blocks history navigation outside the current target origin.", "SecurityError");
      }
      return controlledGatewayUrl(target);
    }
    function guardHistoryMethod(method) {
      const original = History.prototype[method];
      if (typeof original !== "function") return;
      Object.defineProperty(History.prototype, method, {
        value(state, title, url) {
          if (arguments.length >= 3 && url != null) {
            return Reflect.apply(original, this, [state, title, rewrittenHistoryUrl(url)]);
          }
          return Reflect.apply(original, this, arguments);
        },
        configurable: false,
        writable: false,
      });
    }
    guardHistoryMethod("pushState");
    guardHistoryMethod("replaceState");
  })();`;

  function webviewHistoryGuardScriptTag(xmlDocument) {
    const source = xmlDocument
      ? `<![CDATA[\n${webviewHistoryGuardSource.split("]]>").join("]]]]><![CDATA[>")}\n]]>`
      : webviewHistoryGuardSource;
    return `<script ${webviewHistoryGuardMarker}>${source}</script>`;
  }

  function injectWebviewOverlay(html, overlayScriptPath, xmlDocument) {
    const webviewOverlayScriptTag = `<script src="${overlayScriptPath}"></script>`;
    const guarded = injectWebviewHistoryGuard(html, xmlDocument);
    if (/<\/head\s*>/i.test(guarded)) {
      return guarded.replace(/<\/head\s*>/i, `${webviewOverlayScriptTag}</head>`);
    }
    if (xmlDocument && /<\/body\s*>/i.test(guarded)) {
      return guarded.replace(/<\/body\s*>/i, `${webviewOverlayScriptTag}</body>`);
    }
    if (/<body\b[^>]*>/i.test(guarded)) {
      return guarded.replace(/<body\b[^>]*>/i, (bodyTag) => `${bodyTag}${webviewOverlayScriptTag}`);
    }
    return xmlDocument ? guarded : `${guarded}\n${webviewOverlayScriptTag}`;
  }

  function injectControlledNavigationScripts(html, includeOverlay, overlayScriptPath, xmlDocument = false) {
    if (includeOverlay) {
      return injectWebviewOverlay(html, overlayScriptPath, xmlDocument);
    }
    return injectWebviewHistoryGuard(html, xmlDocument);
  }

  function injectWebviewHistoryGuard(html, xmlDocument) {
    const scriptTag = webviewHistoryGuardScriptTag(xmlDocument);
    if (xmlDocument) {
      if (/<head\b[^>]*>/i.test(html)) {
        return html.replace(/<head\b[^>]*>/i, (headTag) => `${headTag}${scriptTag}`);
      }
      if (/<body\b[^>]*>/i.test(html)) {
        return html.replace(/<body\b[^>]*>/i, (bodyTag) => `${bodyTag}${scriptTag}`);
      }
      return html;
    }
    const leading = /^\uFEFF?\s*(?:(?:<!--[\s\S]*?-->)\s*)*(?:<!doctype\s+html\b[^>]*>\s*)?/i.exec(html);
    const index = leading ? leading[0].length : 0;
    return `${html.slice(0, index)}${scriptTag}${html.slice(index)}`;
  }

  function sameTargetUrl(left, right) {
    try {
      return new URL(left).href === new URL(right).href;
    } catch (_error) {
      return false;
    }
  }

  function sameTargetOrigin(left, right) {
    try {
      return new URL(left).origin === new URL(right).origin;
    } catch (_error) {
      return false;
    }
  }

  function isTrustedShellEntrypoint(pathname) {
    return pathname === "/" || pathname === "/index.html";
  }

  self.RingsWebviewWorkerNavigation = Object.freeze({
    injectControlledNavigationScripts,
    isTrustedShellEntrypoint,
    sameTargetOrigin,
    sameTargetUrl,
  });
})();
