;(function(config) {
  const { prefix, targetBase, marker } = config;
  try { delete globalThis.__ringsWebviewBootstrapConfig; } catch (_error) {}
  const runtimePrefix = `${prefix.replace(/\/$/, "")}-runtime/`;
  const runtimeTargetHeader = "X-Rings-Webview-Target";
  const controlledAssetPaths = new Set(["/assets/webview-overlay.js"]);
  const urlAttributes = new Set(["href", "src", "action", "poster", "data", "cite", "formaction", "manifest", "xlink:href"]);
  const srcsetAttributes = new Set(["srcset", "imagesrcset"]);
  const styleProxies = new WeakMap();
  const nativeSetAttribute = globalThis.Element?.prototype?.setAttribute;
  if (globalThis[marker]) {
    globalThis[marker].targetBase = targetBase;
    return;
  }
  const gatewayState = { targetBase };
  let nextRuntimeRequestId = 1;
  globalThis[marker] = gatewayState;
  function gatewayPathFromText(input) {
    const text = String(input);
    if (text.startsWith(prefix)) return text;
    try {
      const url = new URL(text);
      if (globalThis.location?.origin && url.origin === globalThis.location.origin && url.pathname.startsWith(prefix)) {
        return `${url.pathname}${url.search}${url.hash}`;
      }
    } catch (_error) {}
    return undefined;
  }
  function decodeGatewayTarget(input) {
    const path = gatewayPathFromText(input);
    if (!path?.startsWith(prefix)) return undefined;
    const encoded = path.slice(prefix.length);
    if (!encoded) return undefined;
    try {
      return decodeURIComponent(encoded);
    } catch (_error) {
      return undefined;
    }
  }
  function resolveTargetBase() {
    const rawBase = globalThis.document?.querySelector?.("base[href]")?.getAttribute?.("href");
    if (!rawBase) return gatewayState.targetBase;
    const decoded = decodeGatewayTarget(rawBase);
    if (decoded) return decoded;
    try {
      return new URL(String(rawBase), gatewayState.targetBase).href;
    } catch (_error) {
      return gatewayState.targetBase;
    }
  }
  function isUnrewritableTarget(value) {
    const lower = String(value).trim().toLowerCase();
    return !lower || ["javascript:", "mailto:", "tel:", "data:", "blob:", "about:"].some((scheme) => lower.startsWith(scheme));
  }
  function controlledAssetPath(input) {
    const text = String(input);
    const origin = globalThis.location?.origin;
    if (!origin) return undefined;
    try {
      const url = new URL(text, origin);
      if (controlledAssetPaths.has(url.pathname)) {
        return `${url.pathname}${url.search}${url.hash}`;
      }
    } catch (_error) {}
    return undefined;
  }
  function encodeTarget(input, base = resolveTargetBase()) {
    const text = String(input);
    if (isUnrewritableTarget(text)) return text;
    const controlled = controlledAssetPath(text);
    if (controlled) return controlled;
    const gatewayPath = gatewayPathFromText(text);
    if (gatewayPath) return gatewayPath;
    const url = new URL(text, base);
    return prefix + encodeURIComponent(url.href);
  }
  function runtimeGatewayPath() {
    const id = nextRuntimeRequestId++;
    return `${runtimePrefix}${Date.now().toString(36)}-${id.toString(36)}`;
  }
  function runtimeGatewayRequest(input) {
    const rewritten = encodeTarget(input);
    const target = decodeGatewayTarget(rewritten);
    if (!target || !gatewayPathFromText(rewritten)?.startsWith(prefix)) {
      return { url: rewritten, target: undefined };
    }
    return { url: runtimeGatewayPath(), target };
  }
  function requestShellNavigation(input) {
    void input;
    return false;
  }
  function reportFormNavigation(message) {
    try {
      globalThis.__ringsWebviewDebugOverlay?.record?.("bootstrap", message);
    } catch (_error) {}
  }
  function encodeUrlList(input, base) {
    return String(input).trim().split(/\s+/).filter(Boolean).map((value) => encodeTarget(value, base)).join(" ");
  }
  // Pure transform: tokenize candidates before rewriting their URLs.
  function parseSrcsetCandidates(input) {
    const text = String(input);
    const candidates = [];
    let cursor = 0;
    while (cursor < text.length) {
      while (cursor < text.length && /[\t\n\f\r ,]/.test(text[cursor])) cursor += 1;
      const urlStart = cursor;
      while (cursor < text.length && !/[\t\n\f\r ]/.test(text[cursor])) cursor += 1;
      let url = text.slice(urlStart, cursor);
      if (url.endsWith(",")) {
        url = url.replace(/,+$/, "");
        if (url) candidates.push({ url, descriptor: "" });
        continue;
      }
      while (cursor < text.length && /[\t\n\f\r ]/.test(text[cursor])) cursor += 1;
      const descriptorStart = cursor;
      while (cursor < text.length && text[cursor] !== ",") cursor += 1;
      const descriptor = text.slice(descriptorStart, cursor).trim();
      if (url) candidates.push({ url, descriptor });
      if (text[cursor] === ",") cursor += 1;
    }
    return candidates;
  }
  function encodeSrcset(input, base) {
    return parseSrcsetCandidates(input).map(({ url, descriptor }) => {
      const rewritten = encodeTarget(url, base);
      return descriptor ? `${rewritten} ${descriptor}` : rewritten;
    }).join(", ");
  }
  function encodeCssText(input) {
    const imports = String(input).replace(/(@import\s+)(['"])([^'"]+)\2/gi, function(match, importPrefix, quote, value) {
      try {
        return `${importPrefix}${quote}${encodeTarget(value)}${quote}`;
      } catch (_error) {
        return match;
      }
    });
    return imports.replace(/url\(\s*(['"]?)(.*?)\1\s*\)/gi, function(match, quote, value) {
      try {
        const rewritten = encodeTarget(value);
        const delimiter = quote || "";
        return `url(${delimiter}${rewritten}${delimiter})`;
      } catch (_error) {
        return match;
      }
    });
  }
  function encodeRefreshText(input) {
    return String(input).replace(/(\burl\s*=\s*)(['"]?)([^'"]+)\2/i, function(match, prefixText, quote, value) {
      try {
        const delimiter = quote || "";
        return `${prefixText}${delimiter}${encodeTarget(value.trim())}${delimiter}`;
      } catch (_error) {
        return match;
      }
    });
  }
  function setRawAttribute(element, name, value) {
    if (nativeSetAttribute) return nativeSetAttribute.call(element, name, value);
    return element.setAttribute(name, value);
  }
  function rewriteHtmlElement(element, base) {
    const tagName = String(element.tagName || "").toLowerCase();
    for (const attribute of Array.from(element.attributes || [])) {
      setRawAttribute(element, attribute.name, encodeElementAttribute(element, attribute.name, attribute.value, base));
    }
    if (tagName === "style") {
      element.textContent = encodeCssText(element.textContent || "");
    }
    rewriteMetaRefreshElement(element);
  }
  function rewriteHtmlTree(root, initialBase, maySetBase) {
    let base = initialBase;
    const elements = [];
    if (root?.nodeType === 1) elements.push(root);
    elements.push(...Array.from(root.querySelectorAll?.("*") || []));
    for (const element of elements) {
      const tagName = String(element.tagName || "").toLowerCase();
      const baseHref = tagName === "base" ? element.getAttribute?.("href") : null;
      rewriteHtmlElement(element, base);
      if (maySetBase && baseHref != null) {
        try {
          base = new URL(baseHref, base).href;
          maySetBase = false;
        } catch (_error) {}
      }
    }
  }
  function injectSrcdocRuntime(root) {
    const doc = globalThis.document;
    if (!doc?.createElement) return;
    const head = root.querySelector?.("head");
    let container = head || root;
    let anchor = container.firstChild || null;
    const existingBase = root.querySelector?.("base[href]");
    if (existingBase?.parentNode) {
      container = existingBase.parentNode;
      anchor = existingBase.nextSibling || null;
    } else {
      const base = doc.createElement("base");
      setRawAttribute(base, "href", encodeTarget(resolveTargetBase()));
      container.insertBefore?.(base, anchor);
      anchor = base.nextSibling || null;
    }
    const source = doc.querySelector?.("script[data-rings-webview-bootstrap]")?.textContent;
    if (source) {
      const script = doc.createElement("script");
      setRawAttribute(script, "data-rings-webview-bootstrap", "");
      script.textContent = source;
      container.insertBefore?.(script, anchor);
    }
  }
  function encodeSrcdoc(input) {
    const doc = globalThis.document;
    if (!doc?.createElement) return String(input);
    const template = doc.createElement("template");
    setNativeInnerHtml(template, String(input));
    const root = template.content || template;
    rewriteHtmlTree(root, resolveTargetBase(), true);
    injectSrcdocRuntime(root);
    return template.innerHTML;
  }
  function encodeHtmlFragment(input) {
    const doc = globalThis.document;
    if (!doc?.createElement) return String(input);
    const template = doc.createElement("template");
    setNativeInnerHtml(template, String(input));
    const root = template.content || template;
    rewriteHtmlTree(root, resolveTargetBase(), !doc.querySelector?.("base[href]"));
    return template.innerHTML;
  }
  function encodeAttribute(name, value, base) {
    const lower = String(name).toLowerCase();
    if (lower === "srcdoc") return encodeSrcdoc(value);
    if (lower === "style") return encodeCssText(value);
    if (lower === "ping") return encodeUrlList(value, base);
    if (urlAttributes.has(lower)) return encodeTarget(value, base);
    if (srcsetAttributes.has(lower)) return encodeSrcset(value, base);
    return value;
  }
  function isMetaRefreshElement(element) {
    return String(element?.tagName || "").toLowerCase() === "meta"
      && String(element.getAttribute?.("http-equiv") || "").trim().toLowerCase() === "refresh";
  }
  function encodeElementAttribute(element, name, value, base) {
    if (String(name).toLowerCase() === "content" && isMetaRefreshElement(element)) {
      return encodeRefreshText(value);
    }
    return encodeAttribute(name, value, base);
  }
  function rewriteMetaRefreshElement(element) {
    if (!isMetaRefreshElement(element)) return;
    const content = element.getAttribute?.("content");
    if (content != null) setRawAttribute(element, "content", encodeRefreshText(content));
  }
  function blockUnsupportedConstructor(name) {
    const NativeConstructor = globalThis[name];
    if (!NativeConstructor) return;
    const BlockedConstructor = function() {
      throw new TypeError(`${name} is blocked by Rings WebView until gateway transport supports it`);
    };
    BlockedConstructor.prototype = NativeConstructor.prototype;
    Object.setPrototypeOf?.(BlockedConstructor, NativeConstructor);
    globalThis[name] = BlockedConstructor;
  }
  function findPropertyDescriptor(proto, property) {
    let current = proto;
    while (current) {
      const descriptor = Object.getOwnPropertyDescriptor(current, property);
      if (descriptor) return descriptor;
      current = Object.getPrototypeOf(current);
    }
    return undefined;
  }
  function patchUrlProperty(constructorName, property, attributeName) {
    const proto = globalThis[constructorName]?.prototype;
    if (!proto) return;
    const descriptor = findPropertyDescriptor(proto, property);
    if (!descriptor?.set) return;
    Object.defineProperty(proto, property, {
      ...descriptor,
      set(value) {
        return descriptor.set.call(this, encodeAttribute(attributeName || property, value));
      }
    });
  }
  function patchUrlListProperty(constructorName, property) {
    const proto = globalThis[constructorName]?.prototype;
    if (!proto) return;
    const descriptor = findPropertyDescriptor(proto, property);
    if (!descriptor?.set) return;
    Object.defineProperty(proto, property, {
      ...descriptor,
      set(value) {
        return descriptor.set.call(this, encodeUrlList(value, resolveTargetBase()));
      }
    });
  }
  function patchHtmlProperty(property) {
    const proto = globalThis.Element?.prototype;
    if (!proto) return;
    const descriptor = findPropertyDescriptor(proto, property);
    if (!descriptor?.set) return;
    Object.defineProperty(proto, property, {
      ...descriptor,
      set(value) {
        const tagName = String(this.tagName || "").toLowerCase();
        const rewritten = property === "innerHTML" && tagName === "style" ? encodeCssText(value) : encodeHtmlFragment(value);
        return descriptor.set.call(this, rewritten);
      }
    });
  }
  function isStyleNode(node) {
    return String(node?.tagName || node?.parentElement?.tagName || "").toLowerCase() === "style";
  }
  function patchStyleTextContent() {
    const proto = globalThis.Node?.prototype;
    if (!proto) return;
    const descriptor = findPropertyDescriptor(proto, "textContent");
    if (!descriptor?.set) return;
    Object.defineProperty(proto, "textContent", {
      ...descriptor,
      set(value) {
        return descriptor.set.call(this, isStyleNode(this) ? encodeCssText(value) : value);
      }
    });
  }
  function proxyStyle(style) {
    if (!style || typeof Proxy === "undefined") return style;
    const existing = styleProxies.get(style);
    if (existing) return existing;
    const proxy = new Proxy(style, {
      get(target, property) {
        const value = Reflect.get(target, property, target);
        return typeof value === "function" ? value.bind(target) : value;
      },
      set(target, property, value) {
        return Reflect.set(target, property, typeof value === "string" ? encodeCssText(value) : value, target);
      }
    });
    styleProxies.set(style, proxy);
    return proxy;
  }
  function patchStyleGetter(constructorName) {
    const proto = globalThis[constructorName]?.prototype;
    if (!proto) return;
    const descriptor = findPropertyDescriptor(proto, "style");
    if (!descriptor?.get) return;
    Object.defineProperty(proto, "style", {
      ...descriptor,
      get() {
        return proxyStyle(descriptor.get.call(this));
      }
    });
  }
  function patchCssTextMethod(constructorName, method) {
    const proto = globalThis[constructorName]?.prototype;
    const native = proto?.[method];
    if (typeof native !== "function") return;
    proto[method] = function(css, ...rest) {
      return native.call(this, encodeCssText(css), ...rest);
    };
  }
  function patchCssStyleDeclaration() {
    const proto = globalThis.CSSStyleDeclaration?.prototype;
    if (!proto) return;
    const nativeSetProperty = proto.setProperty;
    if (typeof nativeSetProperty === "function") {
      proto.setProperty = function(name, value, priority) {
        return nativeSetProperty.call(this, name, encodeCssText(value), priority);
      };
    }
    const descriptor = findPropertyDescriptor(proto, "cssText");
    if (descriptor?.set) {
      Object.defineProperty(proto, "cssText", {
        ...descriptor,
        set(value) {
          return descriptor.set.call(this, encodeCssText(value));
        }
      });
    }
  }
  function patchMetaRefreshProperty(property) {
    const proto = globalThis.HTMLMetaElement?.prototype;
    if (!proto) return;
    const descriptor = findPropertyDescriptor(proto, property);
    if (!descriptor?.set) return;
    Object.defineProperty(proto, property, {
      ...descriptor,
      set(value) {
        if (property === "httpEquiv" && String(value).trim().toLowerCase() === "refresh") {
          const content = this.getAttribute?.("content");
          if (content != null) setRawAttribute(this, "content", encodeRefreshText(content));
        }
        const rewritten = property === "content" && isMetaRefreshElement(this) ? encodeRefreshText(value) : value;
        return descriptor.set.call(this, rewritten);
      }
    });
  }
  function patchWindowOpen() {
    const nativeOpen = globalThis.open?.bind(globalThis);
    if (!nativeOpen) return;
    globalThis.open = function(url, target, features) {
      const rewritten = url == null || url === "" ? url : encodeTarget(url);
      return nativeOpen(rewritten, target, features);
    };
  }
  function patchLocationNavigation() {
    const proto = globalThis.Location?.prototype;
    if (!proto) return;
    for (const method of ["assign", "replace"]) {
      const native = proto[method];
      if (typeof native !== "function") continue;
      proto[method] = function(url) {
        if (requestShellNavigation(url)) return;
        return native.call(this, encodeTarget(url));
      };
    }
    const descriptor = findPropertyDescriptor(proto, "href");
    if (!descriptor?.set) return;
    Object.defineProperty(proto, "href", {
      ...descriptor,
      set(value) {
        if (requestShellNavigation(value)) return;
        return descriptor.set.call(this, encodeTarget(value));
      }
    });
  }
  function formTargetUrl(form, submitter) {
    const action = submitter?.getAttribute?.("formaction")
      || form.getAttribute?.("action")
      || form.action;
    const decoded = decodeGatewayTarget(action);
    const target = decoded || new URL(String(action || ""), resolveTargetBase()).href;
    const url = new URL(target);
    if (url.protocol !== "http:" && url.protocol !== "https:") return undefined;
    return url;
  }
  function navigateGetForm(form, submitter) {
    if (String(form?.method || "get").toUpperCase() !== "GET") {
      reportFormNavigation("Skipped non-GET form submission");
      return false;
    }
    const targetName = String(form?.target || "").trim().toLowerCase();
    if (targetName && targetName !== "_self") {
      reportFormNavigation(`Skipped form submission targeting ${targetName}`);
      return false;
    }
    if (typeof globalThis.FormData !== "function") {
      reportFormNavigation("FormData is unavailable for GET form submission");
      return false;
    }
    try {
      const target = formTargetUrl(form, submitter);
      if (!target) {
        reportFormNavigation("GET form target is not HTTP(S)");
        return false;
      }
      const fields = new URLSearchParams();
      const data = submitter ? new FormData(form, submitter) : new FormData(form);
      for (const [name, value] of data.entries()) {
        fields.append(String(name), typeof value === "string" ? value : String(value?.name || ""));
      }
      target.search = fields.toString();
      if (requestShellNavigation(target.href)) return true;
      const location = globalThis.location;
      if (typeof location?.assign !== "function") return false;
      location.assign(encodeTarget(target.href));
      return true;
    } catch (error) {
      reportFormNavigation(`GET form interception failed: ${String(error)}`);
      return false;
    }
  }
  function patchGetFormSubmission() {
    const document = globalThis.document;
    if (document?.addEventListener) {
      const intercept = function(event, form, submitter) {
        if (!navigateGetForm(form, submitter)) return false;
        event.preventDefault();
        event.stopImmediatePropagation?.();
        return true;
      };
      document.addEventListener("keydown", function(event) {
        if (event.defaultPrevented || event.isComposing || event.key !== "Enter") return;
        const form = event.target?.closest?.("form");
        reportFormNavigation("Captured Enter for GET form submission");
        intercept(event, form, undefined);
      }, true);
      document.addEventListener("click", function(event) {
        if (event.defaultPrevented || (event.button != null && event.button !== 0)) return;
        const submitter = event.target?.closest?.("button, input");
        if (!submitter) return;
        const tagName = String(submitter.tagName || "").toLowerCase();
        const type = String(submitter.type || "").toLowerCase();
        const isSubmit = (tagName === "button" && (!type || type === "submit"))
          || (tagName === "input" && (type === "submit" || type === "image"));
        if (!isSubmit) return;
        reportFormNavigation("Captured submit control click");
        intercept(event, submitter.form || submitter.closest?.("form"), submitter);
      }, true);
      document.addEventListener("submit", function(event) {
        const form = event.target;
        if (!(form instanceof globalThis.HTMLFormElement)) return;
        intercept(event, form, event.submitter);
      }, true);
    }
    const proto = globalThis.HTMLFormElement?.prototype;
    const nativeSubmit = proto?.submit;
    if (typeof nativeSubmit === "function") {
      proto.submit = function() {
        if (navigateGetForm(this, undefined)) return;
        return nativeSubmit.call(this);
      };
    }
  }
  const nativeInnerHtmlSetter = findPropertyDescriptor(globalThis.Element?.prototype, "innerHTML")?.set;
  function setNativeInnerHtml(element, value) {
    if (nativeInnerHtmlSetter) return nativeInnerHtmlSetter.call(element, value);
    return element.innerHTML = value;
  }
  // Effect boundary: browser network entry points only call pure URL transforms.
  function installNetworkEffects() {
  const nativeFetch = globalThis.fetch?.bind(globalThis);
  if (nativeFetch) {
    globalThis.fetch = function(input, init) {
      const runtime = runtimeGatewayRequest(input instanceof Request ? input.url : input);
      const next = input instanceof Request ? new Request(runtime.url, input) : runtime.url;
      try {
        const headers = new Headers(init?.headers || (input instanceof Request ? next.headers : undefined));
        if (runtime.target) {
          headers.set("X-Rings-Webview-Kind", "fetch");
          headers.set(runtimeTargetHeader, runtime.target);
        }
        return nativeFetch(next, { ...init, headers });
      } catch (_error) {
        return nativeFetch(next, init);
      }
    };
  }
  const NativeXHR = globalThis.XMLHttpRequest;
  const nativeXhrOpen = NativeXHR?.prototype?.open;
  if (typeof nativeXhrOpen === "function") {
    NativeXHR.prototype.open = function(method, url, async, user, password) {
        const runtime = runtimeGatewayRequest(url);
        if (async === false && runtime.target) {
          reportFormNavigation(`Blocked synchronous XMLHttpRequest to ${runtime.target}`);
          throw new TypeError("Synchronous XMLHttpRequest is blocked by Rings WebView");
        }
        const result = nativeXhrOpen.call(this, method, runtime.url, async, user, password);
        try {
          if (runtime.target) {
            this.setRequestHeader("X-Rings-Webview-Kind", "xhr");
            this.setRequestHeader(runtimeTargetHeader, runtime.target);
          }
        } catch (_error) {}
        return result;
    };
  }
  blockUnsupportedConstructor("WebSocket");
  blockUnsupportedConstructor("EventSource");
  blockUnsupportedConstructor("Worker");
  blockUnsupportedConstructor("SharedWorker");
  if (globalThis.navigator?.sendBeacon) {
    globalThis.navigator.sendBeacon = function() {
      return false;
    };
  }
  }
  // Effect boundary: DOM and CSSOM mutations are installed separately from transforms.
  function installDomEffects() {
  function loadLocalScript(path, markerAttribute) {
    const script = globalThis.document?.createElement?.("script");
    if (!script) return false;
    script.async = false;
    if (nativeSetAttribute) {
      nativeSetAttribute.call(script, "src", path);
      if (markerAttribute) nativeSetAttribute.call(script, markerAttribute, "");
    } else {
      script.src = path;
      if (markerAttribute) script.setAttribute?.(markerAttribute, "");
    }
    (globalThis.document?.head || globalThis.document?.documentElement)?.append?.(script);
    return true;
  }
  if (nativeSetAttribute) {
    globalThis.Element.prototype.setAttribute = function(name, value) {
      const lower = String(name).toLowerCase();
      if (lower === "http-equiv" && String(value).trim().toLowerCase() === "refresh") {
        const content = this.getAttribute?.("content");
        if (content != null) setRawAttribute(this, "content", encodeRefreshText(content));
      }
      return nativeSetAttribute.call(this, name, encodeElementAttribute(this, name, value));
    };
  }
  const nativeSetAttributeNs = globalThis.Element?.prototype?.setAttributeNS;
  if (nativeSetAttributeNs) {
    globalThis.Element.prototype.setAttributeNS = function(namespace, name, value) {
      return nativeSetAttributeNs.call(this, namespace, name, encodeElementAttribute(this, name, value));
    };
  }
  patchHtmlProperty("innerHTML");
  patchHtmlProperty("outerHTML");
  const nativeInsertAdjacentHtml = globalThis.Element?.prototype?.insertAdjacentHTML;
  if (nativeInsertAdjacentHtml) {
    globalThis.Element.prototype.insertAdjacentHTML = function(position, html) {
      return nativeInsertAdjacentHtml.call(this, position, encodeHtmlFragment(html));
    };
  }
  patchStyleTextContent();
  const nativeDocumentWrite = globalThis.Document?.prototype?.write;
  if (nativeDocumentWrite) {
    globalThis.Document.prototype.write = function(...parts) {
      return nativeDocumentWrite.call(this, encodeHtmlFragment(parts.join("")));
    };
  }
  patchCssStyleDeclaration();
  patchMetaRefreshProperty("content");
  patchMetaRefreshProperty("httpEquiv");
  patchWindowOpen();
  patchLocationNavigation();
  patchGetFormSubmission();
  for (const constructorName of ["HTMLElement", "SVGElement", "CSSStyleRule", "CSSFontFaceRule", "CSSPageRule", "CSSKeyframeRule"]) {
    patchStyleGetter(constructorName);
  }
  for (const [constructorName, method] of [
    ["CSSStyleSheet", "insertRule"],
    ["CSSStyleSheet", "replace"],
    ["CSSStyleSheet", "replaceSync"],
    ["CSSGroupingRule", "insertRule"],
    ["CSSKeyframesRule", "appendRule"]
  ]) {
    patchCssTextMethod(constructorName, method);
  }
  patchUrlProperty("HTMLAnchorElement", "href");
  patchUrlListProperty("HTMLAnchorElement", "ping");
  patchUrlProperty("HTMLAreaElement", "href");
  patchUrlListProperty("HTMLAreaElement", "ping");
  patchUrlProperty("HTMLBaseElement", "href");
  patchUrlProperty("HTMLImageElement", "src");
  patchUrlProperty("HTMLImageElement", "srcset");
  patchUrlProperty("HTMLScriptElement", "src");
  patchUrlProperty("HTMLIFrameElement", "src");
  patchUrlProperty("HTMLIFrameElement", "srcdoc");
  patchUrlProperty("HTMLLinkElement", "href");
  patchUrlProperty("HTMLFormElement", "action");
  patchUrlProperty("HTMLInputElement", "src");
  patchUrlProperty("HTMLInputElement", "formAction", "formaction");
  patchUrlProperty("HTMLButtonElement", "formAction", "formaction");
  patchUrlProperty("HTMLSourceElement", "src");
  patchUrlProperty("HTMLSourceElement", "srcset");
  patchUrlProperty("HTMLVideoElement", "poster");
  patchUrlProperty("HTMLVideoElement", "src");
  patchUrlProperty("HTMLAudioElement", "src");
  patchUrlProperty("HTMLEmbedElement", "src");
  patchUrlProperty("HTMLObjectElement", "data");
  gatewayState.loadLocalScript = loadLocalScript;
  }
  installNetworkEffects();
  installDomEffects();
  gatewayState.prefix = prefix;
})(globalThis.__ringsWebviewBootstrapConfig);
