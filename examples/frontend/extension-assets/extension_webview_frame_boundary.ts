/**
 * Synchronous DOM boundary that makes authored iframe sources inert before insertion.
 *
 * A MutationObserver is deliberately not a security boundary: a newly inserted
 * `srcdoc` realm may execute before the observer callback. These wrappers retain
 * native DOM ordering while capturing ordinary insertion and source-write
 * surfaces before the browser can start a child navigation.
 */

/** Source attributes that can create a child browsing realm. */
export type FrameSourceAttribute = "src" | "srcdoc";

/** Capability-shaped effects owned by the recursive renderer. */
type DynamicFrameBoundaryEffects = {
  readonly preserveFramePlan: (frame: HTMLIFrameElement) => void;
  readonly captureFrameSource: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute, value: string) => void;
  readonly discardFrameSource: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute) => void;
};

const nativeElementSetAttribute = globalThis.Element.prototype.setAttribute;
const nativeElementSetAttributeNs = globalThis.Element.prototype.setAttributeNS;
const nativeElementSetAttributeNode = globalThis.Element.prototype.setAttributeNode;
const nativeElementSetAttributeNodeNs = globalThis.Element.prototype.setAttributeNodeNS;
const nativeElementRemoveAttribute = globalThis.Element.prototype.removeAttribute;
const nativeElementRemoveAttributeNs = globalThis.Element.prototype.removeAttributeNS;
const nativeNamedNodeMapSetNamedItem = globalThis.NamedNodeMap.prototype.setNamedItem;
const nativeNamedNodeMapSetNamedItemNs = globalThis.NamedNodeMap.prototype.setNamedItemNS;
const nativeNamedNodeMapRemoveNamedItem = globalThis.NamedNodeMap.prototype.removeNamedItem;
const nativeNamedNodeMapRemoveNamedItemNs = globalThis.NamedNodeMap.prototype.removeNamedItemNS;
const nativeInsertAdjacentHtml = globalThis.Element.prototype.insertAdjacentHTML;
const nativeDocumentWrite = globalThis.Document.prototype.write;
const nativeDocumentWriteln = globalThis.Document.prototype.writeln;
const nativeNodeAppendChild = globalThis.Node.prototype.appendChild;
const nativeRangeSelectNodeContents = globalThis.Range.prototype.selectNodeContents;
const nativeRangeCreateContextualFragment = globalThis.Range.prototype.createContextualFragment;
const nativeElementInnerHtml = Object.getOwnPropertyDescriptor(globalThis.Element.prototype, "innerHTML");
const nativeAttrValue = Object.getOwnPropertyDescriptor(globalThis.Attr.prototype, "value");
const nativeNodeValue = Object.getOwnPropertyDescriptor(globalThis.Node.prototype, "nodeValue");
const nativeNodeTextContent = Object.getOwnPropertyDescriptor(globalThis.Node.prototype, "textContent");
const nativeElementAttributes = Object.getOwnPropertyDescriptor(globalThis.Element.prototype, "attributes");
const attributeMapOwners = new WeakMap<NamedNodeMap, Element>();

/**
 * Installs the finite synchronous iframe-mutation boundary before authored code runs.
 *
 * Invariant F1: an authored iframe crosses a live DOM insertion boundary only
 * after `preserveFramePlan` has removed its native `src` and `srcdoc` authority.
 * Invariant F2: an authored source write on a connected frame is captured instead
 * of reaching the browser navigation algorithm.
 */
export function installDynamicFrameBoundary(effects: DynamicFrameBoundaryEffects): void {
  const guard = (value: unknown): void => {
    if (value instanceof Node) guardFrameTree(value, effects.preserveFramePlan);
  };
  wrapMethodArguments(globalThis.Node.prototype, "appendChild", [0], guard);
  wrapMethodArguments(globalThis.Node.prototype, "insertBefore", [0], guard);
  wrapMethodArguments(globalThis.Node.prototype, "replaceChild", [0], guard);
  wrapMethodArguments(globalThis.Element.prototype, "insertAdjacentElement", [1], guard);
  for (const prototype of parentNodePrototypes()) {
    for (const name of ["append", "prepend", "replaceChildren"] as const) wrapAllNodeArguments(prototype, name, guard);
  }
  for (const prototype of childNodePrototypes()) {
    for (const name of ["after", "before", "replaceWith"] as const) wrapAllNodeArguments(prototype, name, guard);
  }
  wrapMethodArguments(globalThis.Range.prototype, "insertNode", [0], guard);
  wrapMarkupSetters(effects.preserveFramePlan);
  wrapMarkupMethods(effects.preserveFramePlan);
  wrapFrameSourceAttributes(effects.captureFrameSource, effects.discardFrameSource);
  wrapAttachedSourceAttributes(effects.captureFrameSource);
  wrapNamedAttributeMaps(effects.captureFrameSource, effects.discardFrameSource);
}

/** Returns each distinct ParentNode implementation that owns insertion methods. */
function parentNodePrototypes(): readonly object[] {
  return [globalThis.Element.prototype, globalThis.Document.prototype, globalThis.DocumentFragment.prototype];
}

/** Returns each distinct ChildNode implementation that owns sibling methods. */
function childNodePrototypes(): readonly object[] {
  return [globalThis.Element.prototype, globalThis.CharacterData.prototype, globalThis.DocumentType.prototype];
}

/** Guards selected method arguments before delegating to the captured native method. */
function wrapMethodArguments(
  prototype: object,
  name: string,
  guardedIndexes: readonly number[],
  guard: (value: unknown) => void,
): void {
  const record = prototype as Record<string, unknown>;
  const native = record[name];
  if (typeof native !== "function") return;
  Object.defineProperty(prototype, name, {
    configurable: true,
    writable: true,
    value: function guardedMethod(this: unknown, ...arguments_: unknown[]): unknown {
      for (const index of guardedIndexes) guard(arguments_[index]);
      return Reflect.apply(native, this, arguments_);
    },
  });
}

/** Guards every Node argument accepted by a variadic DOM insertion method. */
function wrapAllNodeArguments(prototype: object, name: string, guard: (value: unknown) => void): void {
  const record = prototype as Record<string, unknown>;
  const native = record[name];
  if (typeof native !== "function") return;
  Object.defineProperty(prototype, name, {
    configurable: true,
    writable: true,
    value: function guardedVariadicMethod(this: unknown, ...arguments_: unknown[]): unknown {
      for (const argument of arguments_) guard(argument);
      return Reflect.apply(native, this, arguments_);
    },
  });
}

/** Captures iframe plans produced through string-parsing DOM setters. */
function wrapMarkupSetters(preserve: (frame: HTMLIFrameElement) => void): void {
  wrapMarkupSetter(
    globalThis.Element.prototype,
    "innerHTML",
    (owner: unknown): Node | undefined => (owner instanceof Node ? owner : undefined),
    preserve,
  );
  wrapMarkupSetter(
    globalThis.Element.prototype,
    "outerHTML",
    (owner: unknown): Node | undefined => (owner instanceof Node ? (owner.parentNode ?? undefined) : undefined),
    preserve,
  );
  wrapMarkupSetter(
    globalThis.ShadowRoot.prototype,
    "innerHTML",
    (owner: unknown): Node | undefined => (owner instanceof Node ? owner : undefined),
    preserve,
  );
}

/** Wraps one native markup setter and guards its newly parsed subtree before returning. */
function wrapMarkupSetter(
  prototype: object,
  name: string,
  scope: (owner: unknown) => Node | undefined,
  preserve: (frame: HTMLIFrameElement) => void,
): void {
  const descriptor = Object.getOwnPropertyDescriptor(prototype, name);
  if (!descriptor?.get || !descriptor.set) return;
  Object.defineProperty(prototype, name, {
    configurable: descriptor.configurable ?? false,
    enumerable: descriptor.enumerable ?? false,
    get: descriptor.get,
    set: function guardedMarkupSetter(this: unknown, value: unknown): void {
      const priorScope = name === "outerHTML" ? scope(this) : undefined;
      const context =
        name === "outerHTML"
          ? this instanceof Element && this.parentElement
            ? this.parentElement
            : undefined
          : this instanceof Element
            ? this
            : undefined;
      descriptor.set?.call(this, sanitizedMarkup(String(value), preserve, context));
      const root = priorScope ?? scope(this);
      if (root) guardFrameTree(root, preserve);
    },
  });
}

/** Captures iframe plans produced by markup methods that bypass Node insertion wrappers. */
function wrapMarkupMethods(preserve: (frame: HTMLIFrameElement) => void): void {
  globalThis.Element.prototype.insertAdjacentHTML = function guardedInsertAdjacentHtml(
    position: InsertPosition,
    text: string,
  ): void {
    const context = position === "beforebegin" || position === "afterend" ? (this.parentElement ?? this) : this;
    nativeInsertAdjacentHtml.call(this, position, sanitizedMarkup(text, preserve, context));
    guardFrameTree(this.parentNode ?? this, preserve);
  };
  globalThis.Document.prototype.write = function guardedWrite(...text: string[]): void {
    nativeDocumentWrite.call(this, sanitizedMarkup(text.join(""), preserve));
    guardFrameTree(this, preserve);
  };
  globalThis.Document.prototype.writeln = function guardedWriteln(...text: string[]): void {
    nativeDocumentWriteln.call(this, `${sanitizedMarkup(text.join(""), preserve)}\n`);
    guardFrameTree(this, preserve);
  };
}

/** Parses authored markup in an inert template and serializes only guarded iframe plans. */
function sanitizedMarkup(value: string, preserve: (frame: HTMLIFrameElement) => void, context?: Element): string {
  if (!nativeElementInnerHtml?.get || !nativeElementInnerHtml.set) {
    throw new TypeError("native HTML parser is unavailable");
  }
  const template = document.createElement("template");
  if (context) {
    const range = document.createRange();
    nativeRangeSelectNodeContents.call(range, context);
    const parsed = nativeRangeCreateContextualFragment.call(range, value);
    guardFrameTree(parsed, preserve);
    nativeNodeAppendChild.call(template.content, parsed);
  } else {
    nativeElementInnerHtml.set.call(template, value);
    guardFrameTree(template.content, preserve);
  }
  const sanitized = nativeElementInnerHtml.get.call(template);
  if (typeof sanitized !== "string") throw new TypeError("native HTML serializer returned an invalid value");
  return sanitized;
}

/** Redirects direct iframe property and attribute writes into private renderer plans. */
function wrapFrameSourceAttributes(
  capture: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute, value: string) => void,
  discard: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute) => void,
): void {
  wrapFrameProperty("src", capture);
  wrapFrameProperty("srcdoc", capture);
  globalThis.Element.prototype.setAttribute = function guardedSetAttribute(name: string, value: string): void {
    const source = frameSourceAttribute(this, name);
    if (source) capture(this as HTMLIFrameElement, source, String(value));
    else nativeElementSetAttribute.call(this, name, value);
  };
  globalThis.Element.prototype.setAttributeNS = function guardedSetAttributeNs(
    namespace: string | null,
    qualifiedName: string,
    value: string,
  ): void {
    const source = namespace === null ? frameSourceAttribute(this, qualifiedName) : undefined;
    if (source) capture(this as HTMLIFrameElement, source, String(value));
    else nativeElementSetAttributeNs.call(this, namespace, qualifiedName, value);
  };
  globalThis.Element.prototype.setAttributeNode = function guardedSetAttributeNode(attribute: Attr): Attr | null {
    const source = frameSourceAttribute(this, attribute.name);
    if (!source) return nativeElementSetAttributeNode.call(this, attribute);
    const previous = this.getAttributeNode(attribute.name);
    capture(this as HTMLIFrameElement, source, attribute.value);
    return previous;
  };
  globalThis.Element.prototype.setAttributeNodeNS = function guardedSetAttributeNodeNs(attribute: Attr): Attr | null {
    const source = attribute.namespaceURI === null ? frameSourceAttribute(this, attribute.name) : undefined;
    if (!source) return nativeElementSetAttributeNodeNs.call(this, attribute);
    const previous = this.getAttributeNodeNS(null, attribute.localName);
    capture(this as HTMLIFrameElement, source, attribute.value);
    return previous;
  };
  globalThis.Element.prototype.removeAttribute = function guardedRemoveAttribute(name: string): void {
    const source = frameSourceAttribute(this, name);
    if (source) discard(this as HTMLIFrameElement, source);
    else nativeElementRemoveAttribute.call(this, name);
  };
  globalThis.Element.prototype.removeAttributeNS = function guardedRemoveAttributeNs(
    namespace: string | null,
    localName: string,
  ): void {
    const source = namespace === null ? frameSourceAttribute(this, localName) : undefined;
    if (source) discard(this as HTMLIFrameElement, source);
    else nativeElementRemoveAttributeNs.call(this, namespace, localName);
  };
}

/** Redirects mutations of an already attached iframe source Attr into a renderer plan. */
function wrapAttachedSourceAttributes(
  capture: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute, value: string) => void,
): void {
  wrapAttachedSourceAttributeSetter(globalThis.Attr.prototype, "value", nativeAttrValue, capture);
  wrapAttachedSourceAttributeSetter(globalThis.Node.prototype, "nodeValue", nativeNodeValue, capture);
  wrapAttachedSourceAttributeSetter(globalThis.Node.prototype, "textContent", nativeNodeTextContent, capture);
}

/** Wraps one Attr mutation surface while retaining the native getter and non-source behavior. */
function wrapAttachedSourceAttributeSetter(
  prototype: object,
  name: string,
  descriptor: PropertyDescriptor | undefined,
  capture: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute, value: string) => void,
): void {
  if (!descriptor?.get || !descriptor.set) return;
  Object.defineProperty(prototype, name, {
    configurable: descriptor.configurable ?? false,
    enumerable: descriptor.enumerable ?? false,
    get: descriptor.get,
    set: function guardedAttachedAttribute(this: unknown, value: unknown): void {
      const owner = this instanceof Attr ? this.ownerElement : null;
      const source = owner ? frameSourceAttribute(owner, this instanceof Attr ? this.name : "") : undefined;
      if (source) capture(owner as HTMLIFrameElement, source, value == null ? "" : String(value));
      else descriptor.set?.call(this, value);
    },
  });
}

/** Retains the owning element for authored NamedNodeMap source mutations. */
function wrapNamedAttributeMaps(
  capture: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute, value: string) => void,
  discard: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute) => void,
): void {
  if (!nativeElementAttributes?.get) return;
  Object.defineProperty(globalThis.Element.prototype, "attributes", {
    configurable: nativeElementAttributes.configurable ?? false,
    enumerable: nativeElementAttributes.enumerable ?? false,
    get: function guardedAttributes(this: Element): NamedNodeMap {
      const attributes = nativeElementAttributes.get?.call(this) as NamedNodeMap;
      attributeMapOwners.set(attributes, this);
      return attributes;
    },
  });
  globalThis.NamedNodeMap.prototype.setNamedItem = function guardedSetNamedItem(attribute: Attr): Attr | null {
    return setNamedSourceAttribute(this, attribute, nativeNamedNodeMapSetNamedItem, capture);
  };
  globalThis.NamedNodeMap.prototype.setNamedItemNS = function guardedSetNamedItemNs(attribute: Attr): Attr | null {
    return setNamedSourceAttribute(this, attribute, nativeNamedNodeMapSetNamedItemNs, capture);
  };
  globalThis.NamedNodeMap.prototype.removeNamedItem = function guardedRemoveNamedItem(name: string): Attr {
    return removeNamedSourceAttribute(this, null, name, discard);
  };
  globalThis.NamedNodeMap.prototype.removeNamedItemNS = function guardedRemoveNamedItemNs(
    namespace: string | null,
    localName: string,
  ): Attr {
    return removeNamedSourceAttribute(this, namespace, localName, discard);
  };
}

/** Captures one NamedNodeMap source insertion before the native navigation algorithm. */
function setNamedSourceAttribute(
  attributes: NamedNodeMap,
  attribute: Attr,
  native: (this: NamedNodeMap, attribute: Attr) => Attr | null,
  capture: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute, value: string) => void,
): Attr | null {
  const owner = attributeMapOwners.get(attributes);
  const source = owner && attribute.namespaceURI === null ? frameSourceAttribute(owner, attribute.name) : undefined;
  if (!source) return native.call(attributes, attribute);
  const frame = owner as HTMLIFrameElement;
  const previous = frame.getAttributeNode(attribute.name);
  capture(frame, source, attribute.value);
  return previous;
}

/** Captures one NamedNodeMap source removal while preserving its native return law. */
function removeNamedSourceAttribute(
  attributes: NamedNodeMap,
  namespace: string | null,
  name: string,
  discard: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute) => void,
): Attr {
  const owner = attributeMapOwners.get(attributes);
  const source = owner && namespace === null ? frameSourceAttribute(owner, name) : undefined;
  if (!source) {
    return namespace === null
      ? nativeNamedNodeMapRemoveNamedItem.call(attributes, name)
      : nativeNamedNodeMapRemoveNamedItemNs.call(attributes, namespace, name);
  }
  const frame = owner as HTMLIFrameElement;
  const previous = frame.getAttributeNode(name);
  if (!previous) throw new DOMException(`Failed to remove attribute ${name}`, "NotFoundError");
  discard(frame, source);
  return previous;
}

/** Replaces one iframe source-property setter while retaining its native getter. */
function wrapFrameProperty(
  attribute: FrameSourceAttribute,
  capture: (frame: HTMLIFrameElement, attribute: FrameSourceAttribute, value: string) => void,
): void {
  const descriptor = Object.getOwnPropertyDescriptor(globalThis.HTMLIFrameElement.prototype, attribute);
  if (!descriptor?.get || !descriptor.set) return;
  Object.defineProperty(globalThis.HTMLIFrameElement.prototype, attribute, {
    configurable: descriptor.configurable ?? false,
    enumerable: descriptor.enumerable ?? false,
    get: descriptor.get,
    set: function guardedFrameSource(this: HTMLIFrameElement, value: string): void {
      capture(this, attribute, String(value));
    },
  });
}

/** Recognizes only source attributes on an authored iframe. */
function frameSourceAttribute(element: Element, name: string): FrameSourceAttribute | undefined {
  if (!(element instanceof HTMLIFrameElement)) return undefined;
  const lower = name.toLowerCase();
  return lower === "src" || lower === "srcdoc" ? lower : undefined;
}

/** Applies the private-frame plan transform to a node and all iframe descendants. */
function guardFrameTree(node: Node, preserve: (frame: HTMLIFrameElement) => void): void {
  if (node instanceof HTMLIFrameElement) preserve(node);
  if (
    node instanceof Element ||
    node instanceof Document ||
    node instanceof DocumentFragment ||
    node instanceof ShadowRoot
  ) {
    for (const frame of Array.from(node.querySelectorAll<HTMLIFrameElement>("iframe"))) preserve(frame);
  }
}
