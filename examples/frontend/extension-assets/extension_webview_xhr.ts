/**
 * XMLHttpRequest facade backed exclusively by the Extension WebView fetch bridge.
 */

type WebviewFetch = (input: RequestInfo | URL, init?: RequestInit, kind?: "fetch" | "xhr") => Promise<Response>;

/** Explicit effect dependency owned by one renderer-bound XHR constructor. */
type XhrEffects = {
  readonly fetcher: WebviewFetch;
  readonly reportError: (message: string) => void;
};

/** Finite terminal outcomes emitted after the mandatory DONE transition. */
type XhrFailureKind = "abort" | "error" | "timeout";

/** Pure terminal transition consumed by the imperative DOM-event shell. */
type XhrFailureTransition = {
  readonly event: XhrFailureKind;
  readonly finalReadyState: typeof OnionXMLHttpRequest.DONE | typeof OnionXMLHttpRequest.UNSENT;
};

/** Stateful browser facade whose external effects are injected through `XhrEffects`. */
class OnionXMLHttpRequest extends EventTarget {
  static readonly UNSENT = 0;
  static readonly OPENED = 1;
  static readonly HEADERS_RECEIVED = 2;
  static readonly LOADING = 3;
  static readonly DONE = 4;
  readonly UNSENT = 0;
  readonly OPENED = 1;
  readonly HEADERS_RECEIVED = 2;
  readonly LOADING = 3;
  readonly DONE = 4;
  readyState = OnionXMLHttpRequest.UNSENT;
  response: unknown = null;
  responseText = "";
  responseType: XMLHttpRequestResponseType = "";
  responseURL = "";
  responseXML: Document | null = null;
  status = 0;
  statusText = "";
  timeout = 0;
  upload = new EventTarget() as XMLHttpRequestUpload;
  withCredentials = false;
  onabort: ((this: OnionXMLHttpRequest, event: ProgressEvent<EventTarget>) => unknown) | null = null;
  onerror: ((this: OnionXMLHttpRequest, event: ProgressEvent<EventTarget>) => unknown) | null = null;
  onload: ((this: OnionXMLHttpRequest, event: ProgressEvent<EventTarget>) => unknown) | null = null;
  onloadend: ((this: OnionXMLHttpRequest, event: ProgressEvent<EventTarget>) => unknown) | null = null;
  onloadstart: ((this: OnionXMLHttpRequest, event: ProgressEvent<EventTarget>) => unknown) | null = null;
  onprogress: ((this: OnionXMLHttpRequest, event: ProgressEvent<EventTarget>) => unknown) | null = null;
  onreadystatechange: ((this: OnionXMLHttpRequest, event: Event) => unknown) | null = null;
  ontimeout: ((this: OnionXMLHttpRequest, event: ProgressEvent<EventTarget>) => unknown) | null = null;
  private method = "GET";
  private url = "";
  private headers = new Headers();
  private responseHeaders = new Headers();
  private controller: AbortController | undefined;
  private requestGeneration = 0;
  private sendInProgress = false;

  constructor(private readonly effects: XhrEffects) {
    super();
  }

  open(method: string, url: string | URL, async = true, _username?: string | null, _password?: string | null): void {
    if (!async) {
      throw new DOMException("Synchronous XMLHttpRequest is blocked by Rings WebView", "InvalidAccessError");
    }
    const previousState = this.readyState;
    const previous = this.controller;
    this.requestGeneration += 1;
    this.controller = undefined;
    this.sendInProgress = false;
    previous?.abort("superseded");
    this.method = method.toUpperCase();
    this.url = String(url);
    this.headers = new Headers();
    this.resetResponse();
    this.readyState = OnionXMLHttpRequest.OPENED;
    if (previousState !== OnionXMLHttpRequest.OPENED) this.emitReadyStateChange();
  }

  setRequestHeader(name: string, value: string): void {
    if (this.readyState !== OnionXMLHttpRequest.OPENED || this.sendInProgress) {
      throw new DOMException("XMLHttpRequest headers are not mutable", "InvalidStateError");
    }
    this.headers.append(name, value);
  }

  getAllResponseHeaders(): string {
    return Array.from(
      this.responseHeaders.entries(),
      ([name, value]: [string, string]): string => `${name}: ${value}\r\n`,
    ).join("");
  }

  getResponseHeader(name: string): string | null {
    return this.responseHeaders.get(name);
  }

  overrideMimeType(_mime: string): void {}

  abort(): void {
    const controller = this.controller;
    if (!controller || !this.sendInProgress) return;
    this.requestGeneration += 1;
    controller.abort("abort");
    this.finishFailure("abort");
  }

  send(body?: Document | XMLHttpRequestBodyInit | null): void {
    if (this.readyState !== OnionXMLHttpRequest.OPENED || this.sendInProgress) {
      throw new DOMException("XMLHttpRequest is not open", "InvalidStateError");
    }
    const controller = new AbortController();
    const generation = this.requestGeneration;
    this.controller = controller;
    this.sendInProgress = true;
    this.emitProgress("loadstart");
    const timeoutId =
      this.timeout > 0 ? globalThis.setTimeout((): void => controller.abort("timeout"), this.timeout) : undefined;
    const requestInit: RequestInit = {
      method: this.method,
      headers: this.headers,
      ...(body == null ? {} : { body: body as BodyInit }),
      credentials: this.withCredentials ? "include" : "same-origin",
      redirect: "follow",
      signal: controller.signal,
    };
    void this.effects
      .fetcher(this.url, requestInit, "xhr")
      .then(async (response: Response): Promise<void> => {
        if (!this.ownsRequest(generation, controller)) return;
        this.status = response.status;
        this.statusText = response.statusText;
        this.responseURL = response.url;
        this.responseHeaders = response.headers;
        this.readyState = OnionXMLHttpRequest.HEADERS_RECEIVED;
        this.emitReadyStateChange();
        this.readyState = OnionXMLHttpRequest.LOADING;
        this.emitReadyStateChange();
        const bytes = await response.arrayBuffer();
        if (!this.ownsRequest(generation, controller)) return;
        this.assignResponse(bytes, response.headers.get("content-type") ?? "text/plain");
        this.readyState = OnionXMLHttpRequest.DONE;
        this.sendInProgress = false;
        this.controller = undefined;
        this.emitReadyStateChange();
        this.emitProgress("load", bytes.byteLength);
        this.emitProgress("loadend", bytes.byteLength);
      })
      .catch((error: unknown): void => {
        if (!this.ownsRequest(generation, controller)) return;
        const kind = failureKind(controller);
        if (kind === "error") this.effects.reportError(error instanceof Error ? error.message : String(error));
        this.finishFailure(kind);
      })
      .finally((): void => {
        if (timeoutId !== undefined) globalThis.clearTimeout(timeoutId);
      });
  }

  private ownsRequest(generation: number, controller: AbortController): boolean {
    return this.requestGeneration === generation && this.controller === controller;
  }

  private finishFailure(kind: XhrFailureKind): void {
    const transition = failureTransition(kind);
    this.sendInProgress = false;
    this.controller = undefined;
    this.resetResponse();
    this.readyState = OnionXMLHttpRequest.DONE;
    this.emitReadyStateChange();
    this.emitProgress(transition.event);
    this.emitProgress("loadend");
    this.readyState = transition.finalReadyState;
  }

  private assignResponse(buffer: ArrayBuffer, contentType: string): void {
    const text = new TextDecoder().decode(buffer);
    if (this.responseType === "arraybuffer") {
      this.response = buffer;
    } else if (this.responseType === "blob") {
      this.response = new Blob([buffer], { type: contentType });
    } else if (this.responseType === "json") {
      try {
        this.response = JSON.parse(text) as unknown;
      } catch (_error: unknown) {
        this.response = null;
      }
    } else if (this.responseType === "document") {
      this.responseXML = new DOMParser().parseFromString(text, "text/html");
      this.response = this.responseXML;
    } else {
      this.responseText = text;
      this.response = text;
    }
  }

  private resetResponse(): void {
    this.response = null;
    this.responseText = "";
    this.responseURL = "";
    this.responseXML = null;
    this.status = 0;
    this.statusText = "";
    this.responseHeaders = new Headers();
  }

  private emitReadyStateChange(): void {
    const event = new Event("readystatechange");
    this.dispatchEvent(event);
    this.onreadystatechange?.call(this, event);
  }

  private emitProgress(type: string, loaded = 0): void {
    const event = new ProgressEvent(type, { lengthComputable: true, loaded, total: loaded });
    this.dispatchEvent(event);
    const handler = this[`on${type}` as keyof OnionXMLHttpRequest];
    if (typeof handler === "function") {
      (handler as (this: OnionXMLHttpRequest, event: ProgressEvent<EventTarget>) => unknown).call(this, event);
    }
  }
}

/** Classifies an AbortController reason without coupling it to DOM effects. */
function failureKind(controller: AbortController): XhrFailureKind {
  if (controller.signal.reason === "timeout") return "timeout";
  return controller.signal.aborted ? "abort" : "error";
}

/**
 * Formal terminal invariant: every failed send is observable in DONE while its
 * terminal event and loadend fire; only explicit abort resets silently to
 * UNSENT after those events, matching the browser XHR state machine.
 */
function failureTransition(kind: XhrFailureKind): XhrFailureTransition {
  return {
    event: kind,
    finalReadyState: kind === "abort" ? OnionXMLHttpRequest.UNSENT : OnionXMLHttpRequest.DONE,
  };
}

/** Installs the XMLHttpRequest facade at the renderer effect boundary. */
export function installWebviewXmlHttpRequest(fetcher: WebviewFetch, reportError: (message: string) => void): void {
  const effects: XhrEffects = { fetcher, reportError };
  class BoundOnionXMLHttpRequest extends OnionXMLHttpRequest {
    constructor() {
      super(effects);
    }
  }
  Object.defineProperty(globalThis, "XMLHttpRequest", {
    configurable: true,
    value: BoundOnionXMLHttpRequest,
    writable: true,
  });
}
