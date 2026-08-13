;(function(config) {
  const { prefix, targetBase, marker } = config;
  const shellNavigation = config.delegateNavigation === true
    ? globalThis.__ringsWebviewShellNavigation
    : undefined;
  if (config.delegateNavigation === true) {
    try { delete globalThis.__ringsWebviewShellNavigation; } catch (_error) {}
  }
  try { delete globalThis.__ringsWebviewBootstrapConfig; } catch (_error) {}
  const runtimePrefix = `${prefix.replace(/\/$/, "")}-runtime/`;
  const runtimeTargetHeader = "X-Rings-Webview-Target";
  const controlledAssetPaths = new Set(["/assets/webview-overlay.js"]);
  const styleProxies = new WeakMap();
  const attributeMapOwners = new WeakMap();
  const nativeSetAttribute = globalThis.Element?.prototype?.setAttribute;
  const nativeSetAttributeNode = globalThis.Element?.prototype?.setAttributeNode;
  const nativeSetAttributeNodeNs = globalThis.Element?.prototype?.setAttributeNodeNS;
  const nativeNamedNodeMapSetNamedItem = globalThis.NamedNodeMap?.prototype?.setNamedItem;
  const nativeNamedNodeMapSetNamedItemNs = globalThis.NamedNodeMap?.prototype?.setNamedItemNS;
  const nativeElementAttributes = Object.getOwnPropertyDescriptor(
    globalThis.Element?.prototype || {},
    "attributes"
  );
  const nativeAttrValue = Object.getOwnPropertyDescriptor(globalThis.Attr?.prototype || {}, "value");
  const nativeNodeValue = Object.getOwnPropertyDescriptor(globalThis.Node?.prototype || {}, "nodeValue");
  const nativeNodeTextContent = Object.getOwnPropertyDescriptor(
    globalThis.Node?.prototype || {},
    "textContent"
  );
  const nativeNodeAppendChild = globalThis.Node?.prototype?.appendChild;
  if (globalThis[marker]) {
    globalThis[marker].targetBase = targetBase;
    return;
  }
  const gatewayState = { targetBase };
  let nextRuntimeRequestId = 1;
  globalThis[marker] = gatewayState;
  const transforms = globalThis.__ringsWebviewTransforms;
  if (!transforms) return;
  const urlTransformer = transforms.createUrlTransformer({
    prefix,
    targetBase,
    locationOrigin: globalThis.location?.origin,
    controlledAssetPaths,
  });
  const { decodeGatewayTarget, gatewayPath: gatewayPathFromText } = urlTransformer;
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
  function encodeTarget(input, base = resolveTargetBase()) {
    return urlTransformer.encodeTarget(input, base);
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
    if (typeof shellNavigation !== "function") return false;
    try {
      return shellNavigation(String(input)) === true;
    } catch (error) {
      reportFormNavigation(`Host navigation failed: ${String(error)}`);
      return false;
    }
  }
  function reportFormNavigation(message) {
    try {
      globalThis.__ringsWebviewDebugOverlay?.record?.("bootstrap", message);
    } catch (_error) {}
  }
  function encodeUrlList(input, base) {
    return transforms.encodeUrlList(input, base, encodeTarget);
  }
  function encodeSrcset(input, base) {
    return transforms.encodeSrcset(input, base, encodeTarget);
  }
  function encodeCssText(input) {
    return transforms.encodeCssText(input, encodeTarget);
  }
  function encodeRefreshText(input) {
    return transforms.encodeRefreshText(input, encodeTarget);
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
  function nativeInsertion(prototype, method) {
    const native = prototype?.[method];
    return typeof native === "function" ? native : undefined;
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
    switch (transforms.htmlAttributeKind(name)) {
      case "srcdoc": return encodeSrcdoc(value);
      case "style": return encodeCssText(value);
      case "url-list": return encodeUrlList(value, base);
      case "url": return encodeTarget(value, base);
      case "srcset": return encodeSrcset(value, base);
      default: break;
    }
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
  function setRawAttributeValue(attribute, descriptor, value) {
    if (descriptor?.set) return descriptor.set.call(attribute, value);
    attribute.value = value;
  }
  function prepareAttributeNode(element, attribute) {
    const value = encodeElementAttribute(element, attribute.name, attribute.value);
    setRawAttributeValue(attribute, nativeAttrValue, value);
    return attribute;
  }
  function blockUnsupportedConstructor(name) {
    const NativeConstructor = globalThis[name];
    if (!NativeConstructor) return;
    const BlockedConstructor = function() {
      throw new TypeError(`${name} is blocked by Rings WebView`);
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
  function patchNavigationApi() {
    const proto = globalThis.Navigation?.prototype;
    const nativeNavigate = proto?.navigate;
    if (typeof nativeNavigate !== "function") return;
    proto.navigate = function(url, options) {
      if (requestShellNavigation(url)) return;
      return nativeNavigate.call(this, encodeTarget(url), options);
    };
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
    const method = String(submitter?.formMethod || form?.method || "get").trim().toUpperCase();
    if (method !== "GET") {
      reportFormNavigation("Skipped non-GET form submission");
      return false;
    }
    const targetName = String(submitter?.formTarget || form?.target || "").trim().toLowerCase();
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
        if (!form) return false;
        const navigated = navigateGetForm(form, submitter);
        if (!navigated && config.delegateNavigation !== true) return false;
        event.preventDefault();
        event.stopImmediatePropagation?.();
        return navigated;
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
        if (navigateGetForm(this, undefined) || config.delegateNavigation === true) return;
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
  blockUnsupportedConstructor("RTCPeerConnection");
  blockUnsupportedConstructor("webkitRTCPeerConnection");
  blockUnsupportedConstructor("RTCDataChannel");
  blockUnsupportedConstructor("WebTransport");
  if (config.blockWorkers !== false) {
    blockUnsupportedConstructor("Worker");
    blockUnsupportedConstructor("SharedWorker");
  }
  if (globalThis.navigator?.sendBeacon) {
    globalThis.navigator.sendBeacon = function() {
      return false;
    };
  }
  }
  // Effect boundary: DOM and CSSOM mutations are installed separately from transforms.
  function installDomEffects() {
  function loadLocalScript(path, markerAttribute) {
    if (!controlledAssetPaths.has(String(path))) return false;
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
    const container = globalThis.document?.head || globalThis.document?.documentElement;
    if (container && nativeNodeAppendChild) nativeNodeAppendChild.call(container, script);
    else container?.append?.(script);
    return true;
  }
  function rewriteInsertedNode(value) {
    if (!globalThis.Node || !(value instanceof globalThis.Node)) return;
    rewriteHtmlTree(
      value,
      resolveTargetBase(),
      !globalThis.document?.querySelector?.("base[href]")
    );
  }
  function patchInsertionArguments(prototype, method, indexes) {
    const native = nativeInsertion(prototype, method);
    if (!native) return;
    prototype[method] = function(...args) {
      for (const index of indexes) rewriteInsertedNode(args[index]);
      return native.apply(this, args);
    };
  }
  function patchVariadicInsertions(prototype, method) {
    const native = nativeInsertion(prototype, method);
    if (!native) return;
    prototype[method] = function(...args) {
      for (const value of args) rewriteInsertedNode(value);
      return native.apply(this, args);
    };
  }
  patchInsertionArguments(globalThis.Node?.prototype, "appendChild", [0]);
  patchInsertionArguments(globalThis.Node?.prototype, "insertBefore", [0]);
  patchInsertionArguments(globalThis.Node?.prototype, "replaceChild", [0]);
  patchInsertionArguments(globalThis.Element?.prototype, "insertAdjacentElement", [1]);
  patchInsertionArguments(globalThis.Range?.prototype, "insertNode", [0]);
  for (const prototype of [
    globalThis.Element?.prototype,
    globalThis.Document?.prototype,
    globalThis.DocumentFragment?.prototype,
  ]) {
    for (const method of ["append", "prepend", "replaceChildren"]) {
      patchVariadicInsertions(prototype, method);
    }
  }
  for (const prototype of [
    globalThis.Element?.prototype,
    globalThis.CharacterData?.prototype,
    globalThis.DocumentType?.prototype,
  ]) {
    for (const method of ["after", "before", "replaceWith"]) {
      patchVariadicInsertions(prototype, method);
    }
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
  if (nativeSetAttributeNode) {
    globalThis.Element.prototype.setAttributeNode = function(attribute) {
      const result = nativeSetAttributeNode.call(this, prepareAttributeNode(this, attribute));
      rewriteMetaRefreshElement(this);
      return result;
    };
  }
  if (nativeSetAttributeNodeNs) {
    globalThis.Element.prototype.setAttributeNodeNS = function(attribute) {
      const result = nativeSetAttributeNodeNs.call(this, prepareAttributeNode(this, attribute));
      rewriteMetaRefreshElement(this);
      return result;
    };
  }
  function patchAttachedAttributeSetter(prototype, property, descriptor) {
    if (!prototype || !descriptor?.get || !descriptor?.set) return;
    Object.defineProperty(prototype, property, {
      ...descriptor,
      set(value) {
        const owner = globalThis.Attr && this instanceof globalThis.Attr ? this.ownerElement : null;
        const rewritten = owner
          ? encodeElementAttribute(owner, this.name, value == null ? "" : String(value))
          : value;
        const result = descriptor.set.call(this, rewritten);
        if (owner) rewriteMetaRefreshElement(owner);
        return result;
      }
    });
  }
  patchAttachedAttributeSetter(globalThis.Attr?.prototype, "value", nativeAttrValue);
  patchAttachedAttributeSetter(globalThis.Node?.prototype, "nodeValue", nativeNodeValue);
  patchAttachedAttributeSetter(globalThis.Node?.prototype, "textContent", nativeNodeTextContent);
  if (nativeElementAttributes?.get) {
    Object.defineProperty(globalThis.Element.prototype, "attributes", {
      ...nativeElementAttributes,
      get() {
        const attributes = nativeElementAttributes.get.call(this);
        attributeMapOwners.set(attributes, this);
        return attributes;
      }
    });
  }
  function setNamedAttribute(attributes, attribute, native) {
    const owner = attributeMapOwners.get(attributes);
    const prepared = owner ? prepareAttributeNode(owner, attribute) : attribute;
    const result = native.call(attributes, prepared);
    if (owner) rewriteMetaRefreshElement(owner);
    return result;
  }
  if (nativeNamedNodeMapSetNamedItem) {
    globalThis.NamedNodeMap.prototype.setNamedItem = function(attribute) {
      return setNamedAttribute(this, attribute, nativeNamedNodeMapSetNamedItem);
    };
  }
  if (nativeNamedNodeMapSetNamedItemNs) {
    globalThis.NamedNodeMap.prototype.setNamedItemNS = function(attribute) {
      return setNamedAttribute(this, attribute, nativeNamedNodeMapSetNamedItemNs);
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
  const nativeDocumentWriteln = globalThis.Document?.prototype?.writeln;
  if (nativeDocumentWriteln) {
    globalThis.Document.prototype.writeln = function(...parts) {
      return nativeDocumentWriteln.call(this, encodeHtmlFragment(parts.join("")));
    };
  }
  const nativeCreateContextualFragment = globalThis.Range?.prototype?.createContextualFragment;
  if (nativeCreateContextualFragment) {
    globalThis.Range.prototype.createContextualFragment = function(markup) {
      const fragment = nativeCreateContextualFragment.call(this, String(markup));
      rewriteHtmlTree(
        fragment,
        resolveTargetBase(),
        !globalThis.document?.querySelector?.("base[href]")
      );
      return fragment;
    };
  }
  patchCssStyleDeclaration();
  patchMetaRefreshProperty("content");
  patchMetaRefreshProperty("httpEquiv");
  patchWindowOpen();
  patchLocationNavigation();
  patchNavigationApi();
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
