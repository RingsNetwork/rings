use super::*;
use crate::error::WebviewError;

#[test]
fn test_bootstrap_hooks_browser_network_entrypoints() -> Result<()> {
    let document_url = Url::parse("https://example.test/docs/index.html")?;
    let script = bootstrap_script("/webview/", &document_url);

    assert!(script.contains(BOOTSTRAP_MARKER));
    assert!(script.contains("targetBase"));
    assert!(script.contains("https://example.test/docs/index.html"));
    assert!(script.contains("globalThis.fetch"));
    assert!(script.contains("X-Rings-Webview-Kind"));
    assert!(script.contains("patchLocationNavigation"));
    assert!(script.contains("resolveTargetBase"));
    assert!(script.contains("XMLHttpRequest"));
    assert!(script.contains("encodeSrcdoc"));
    assert!(script.contains("HTMLIFrameElement"));
    assert!(script.contains("blockUnsupportedConstructor"));
    assert!(script.contains("WebSocket"));
    assert!(script.contains("EventSource"));
    assert!(script.contains("sendBeacon"));
    assert!(script.contains("SharedWorker"));
    assert!(script.contains("\"blockWorkers\":true"));
    assert!(script.contains("HTMLBaseElement"));
    assert!(script.contains("setAttribute"));
    assert!(script.contains("encodeURIComponent"));
    Ok(())
}

#[test]
fn test_host_worker_bridge_bootstrap_preserves_host_worker_constructors() -> Result<()> {
    let document_url = Url::parse("https://example.test/docs/index.html")?;
    let script = bootstrap_script_with_extension_bridge("/webview/", &document_url);

    assert!(script.contains("\"blockWorkers\":false"));
    assert!(script.contains("\"delegateNavigation\":true"));
    assert!(script.contains("config.blockWorkers !== false"));
    Ok(())
}

#[test]
fn test_runtime_urls_resolve_against_target_document_not_gateway_location() -> Result<()> {
    let prefix = GatewayPrefix::new("/webview/")?;
    let document_url = Url::parse("https://example.test/docs/index.html")?;

    let fetch_url = runtime_gateway_url(&prefix, &document_url, "/api/data")
        .and_then(|url| required_gateway_url(url, "/api/data"))?;
    let xhr_url = runtime_gateway_url(&prefix, &document_url, "forms/submit")
        .and_then(|url| required_gateway_url(url, "forms/submit"))?;

    assert_eq!(
        prefix.decode_path(&fetch_url)?.as_url().as_str(),
        "https://example.test/api/data"
    );
    assert_eq!(
        prefix.decode_path(&xhr_url)?.as_url().as_str(),
        "https://example.test/docs/forms/submit"
    );
    Ok(())
}

fn required_gateway_url(url: Option<String>, input: &str) -> Result<String> {
    url.ok_or_else(|| WebviewError::InvalidGatewayUrl(input.to_string()))
}

#[test]
fn test_bootstrap_executes_runtime_routing_in_javascript() -> Result<()> {
    let document_url = Url::parse("https://example.test/docs/index.html")?;
    let script = bootstrap_script("/webview/", &document_url);
    let program = format!(
        r#"
const calls = [];
const runtimePrefix = "/webview-runtime/";
function assert(condition, message) {{
  if (!condition) throw new Error(message);
}}
function assertThrows(operation, message) {{
  let threw = false;
  try {{
operation();
  }} catch (_error) {{
threw = true;
  }}
  if (!threw) throw new Error(message);
}}
function header(init, name) {{
  return init?.headers?.get?.(name) || init?.headers?.get?.(name.toLowerCase());
}}
class Request {{
  constructor(input, init) {{
this.url = String(input);
this.init = init;
  }}
}}
globalThis.Request = Request;
globalThis.fetch = function(input, init) {{
  calls.push(["fetch", input instanceof Request ? input.url : String(input), init]);
  return "fetch-result";
}};
class XMLHttpRequest {{
  static DONE = 4;
  setRequestHeader(name, value) {{
calls.push(["xhr-header", name, value]);
  }}
  open(method, url, async, user, password) {{
calls.push(["xhr", method, url, async, user, password]);
return "xhr-result";
  }}
}}
globalThis.XMLHttpRequest = XMLHttpRequest;
class WebSocket {{
  constructor(url, protocols) {{
calls.push(["websocket", url, protocols]);
  }}
}}
globalThis.WebSocket = WebSocket;
class EventSource {{
  constructor(url, init) {{
calls.push(["eventsource", url, init]);
  }}
}}
globalThis.EventSource = EventSource;
class Worker {{
  constructor(url, options) {{
calls.push(["worker", url, options]);
  }}
}}
globalThis.Worker = Worker;
class SharedWorker {{
  constructor(url, options) {{
calls.push(["sharedworker", url, options]);
  }}
}}
globalThis.SharedWorker = SharedWorker;
Object.defineProperty(globalThis, "navigator", {{
  value: {{
sendBeacon(url, data) {{
  calls.push(["beacon", url, data]);
  return true;
}}
  }},
  configurable: true
}});
class Element {{
  setAttribute(name, value) {{
calls.push(["attribute", name, value]);
  }}
}}
globalThis.Element = Element;
class HTMLImageElement extends Element {{
  set src(value) {{
calls.push(["property", "img.src", value]);
  }}
  set srcset(value) {{
calls.push(["property", "img.srcset", value]);
  }}
}}
globalThis.HTMLImageElement = HTMLImageElement;
const submitListeners = [];
Object.defineProperty(globalThis, "location", {{
  value: {{
origin: "http://127.0.0.1:3000",
assign(url) {{ calls.push(["navigate", url]); }}
  }},
  configurable: true
}});
Object.defineProperty(globalThis, "parent", {{
  value: {{
postMessage(message, targetOrigin) {{ calls.push(["shell-navigation", message, targetOrigin]); }}
  }},
  configurable: true
}});
Object.defineProperty(globalThis, "document", {{
  value: {{
querySelector() {{ return null; }},
addEventListener(type, listener, capture) {{
  if (type === "submit" && capture) submitListeners.push(listener);
}}
  }},
  configurable: true
}});
class HTMLFormElement extends Element {{
  constructor() {{
super();
this.method = "get";
this.target = "";
this.attributes = new Map();
this.fields = [];
  }}
  getAttribute(name) {{ return this.attributes.get(name) || null; }}
  setAttribute(name, value) {{ this.attributes.set(name, value); }}
  submit() {{ calls.push(["native-form-submit"]); }}
}}
globalThis.HTMLFormElement = HTMLFormElement;
globalThis.FormData = class FormData {{
  constructor(form) {{ this.fields = form.fields; }}
  entries() {{ return this.fields[Symbol.iterator](); }}
}};
{script}
const gateway = (url) => "/webview/" + encodeURIComponent(url);
await fetch("/api/data");
await fetch(new Request("forms/submit"), {{ method: "POST" }});
const xhr = new globalThis.XMLHttpRequest();
assert(xhr instanceof XMLHttpRequest, "XHR constructor identity was not preserved");
assert(globalThis.XMLHttpRequest === XMLHttpRequest, "XHR constructor was replaced");
assert(globalThis.XMLHttpRequest.DONE === 4, "XHR static constants were not preserved");
xhr.open("POST", "forms/x", true);
const syncXhr = new globalThis.XMLHttpRequest();
assertThrows(() => syncXhr.open("GET", "forms/sync", false), "synchronous gateway XHR was not blocked");
assertThrows(() => new globalThis.WebSocket("/socket"), "WebSocket was not blocked");
assertThrows(() => new globalThis.EventSource("events"), "EventSource was not blocked");
const beaconResult = navigator.sendBeacon("/beacon", "payload");
assert(beaconResult === false, "sendBeacon was not blocked");
assertThrows(() => new globalThis.Worker("worker.js"), "Worker was not blocked");
assertThrows(() => new globalThis.SharedWorker("shared.js"), "SharedWorker was not blocked");
const element = new Element();
element.setAttribute("src", "image.png");
element.setAttribute("srcset", "small.png 1x, /big.png 2x");
element.setAttribute("srcset", "data:image/svg+xml,%3Csvg%3E,%3C/svg%3E 1x, /data-fallback.png 2x");
element.setAttribute("srcset", "data:image/svg+xml,%3Csvg%3E,%3C/svg%3E, /next.png 2x");
element.setAttribute("srcset", "first.png 1x,,, /consecutive.png 2x");
element.setAttribute("aria-label", "unchanged");
const image = new globalThis.HTMLImageElement();
image.src = "property.png";
image.srcset = "property-small.png 1x, /property-big.png 2x";
const form = new globalThis.HTMLFormElement();
form.setAttribute("action", gateway("https://example.test/docs/search?existing=1"));
form.fields = [["q", "test"]];
const submitEvent = {{
  target: form,
  submitter: null,
  preventDefault() {{ this.prevented = true; }},
  stopImmediatePropagation() {{ this.stopped = true; }}
}};
for (const listener of submitListeners) listener(submitEvent);
const postOverrideEvent = {{
  target: form,
  submitter: {{ formMethod: "post", formTarget: "" }},
  preventDefault() {{ this.prevented = true; }},
  stopImmediatePropagation() {{ this.stopped = true; }}
}};
for (const listener of submitListeners) listener(postOverrideEvent);
const targetOverrideEvent = {{
  target: form,
  submitter: {{ formMethod: "get", formTarget: "named" }},
  preventDefault() {{ this.prevented = true; }},
  stopImmediatePropagation() {{ this.stopped = true; }}
}};
for (const listener of submitListeners) listener(targetOverrideEvent);
form.method = "post";
form.submit();
const actual = JSON.stringify(calls);
assert(calls.some((call) => call[0] === "fetch" && String(call[1]).startsWith(runtimePrefix) && header(call[2], "X-Rings-Webview-Target") === "https://example.test/api/data"), "fetch URL was not routed through runtime gateway: " + actual);
assert(calls.some((call) => call[0] === "fetch" && String(call[1]).startsWith(runtimePrefix) && header(call[2], "X-Rings-Webview-Target") === "https://example.test/docs/forms/submit"), "Request URL was not routed through runtime gateway: " + actual);
assert(calls.some((call) => call[0] === "xhr" && String(call[2]).startsWith(runtimePrefix)), "XHR URL was not routed through runtime gateway: " + actual);
assert(calls.some((call) => call[0] === "xhr-header" && call[1] === "X-Rings-Webview-Target" && call[2] === "https://example.test/docs/forms/x"), "XHR target header was not set: " + actual);
assert(!calls.some((call) => call[0] === "xhr" && String(call[2]).includes("forms%2Fsync")), "synchronous XHR reached native transport: " + actual);
assert(!calls.some((call) => ["websocket", "eventsource", "beacon", "worker", "sharedworker"].includes(call[0])), "unsupported native entrypoint was called: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "src" && call[2] === gateway("https://example.test/docs/image.png")), "setAttribute src was not rewritten: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "srcset" && call[2].includes(gateway("https://example.test/docs/small.png")) && call[2].includes(gateway("https://example.test/big.png"))), "setAttribute srcset was not rewritten: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "srcset" && call[2].includes("data:image/svg+xml,%3Csvg%3E,%3C/svg%3E 1x") && call[2].includes(gateway("https://example.test/data-fallback.png"))), "data URL srcset candidate was split at its payload comma: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "srcset" && call[2].includes("data:image/svg+xml,%3Csvg%3E,%3C/svg%3E") && call[2].includes(gateway("https://example.test/next.png"))), "descriptor-less data URL swallowed the next srcset candidate: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "srcset" && call[2].includes(gateway("https://example.test/docs/first.png")) && call[2].includes(gateway("https://example.test/consecutive.png"))), "consecutive srcset separators dropped a candidate: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "aria-label" && call[2] === "unchanged"), "non-URL attribute changed: " + actual);
assert(calls.some((call) => call[0] === "property" && call[1] === "img.src" && call[2] === gateway("https://example.test/docs/property.png")), "img.src property was not rewritten: " + actual);
assert(calls.some((call) => call[0] === "property" && call[1] === "img.srcset" && call[2].includes(gateway("https://example.test/docs/property-small.png")) && call[2].includes(gateway("https://example.test/property-big.png"))), "img.srcset property was not rewritten: " + actual);
assert(submitEvent.prevented && submitEvent.stopped, "GET form submission was not intercepted");
assert(calls.some((call) => call[0] === "navigate" && call[1] === gateway("https://example.test/docs/search?q=test")), "GET form query did not use native gateway navigation: " + actual);
assert(!postOverrideEvent.prevented && !postOverrideEvent.stopped, "web host blocked a native POST override");
assert(!targetOverrideEvent.prevented && !targetOverrideEvent.stopped, "web host blocked a named browsing-context target");
assert(calls.filter((call) => call[0] === "navigate").length === 1, "POST form override navigated through the GET path: " + actual);
assert(calls.filter((call) => call[0] === "native-form-submit").length === 1, "web host did not preserve native programmatic POST submission: " + actual);
"#,
        script = script
    );

    let output = std::process::Command::new("node")
        .arg("--input-type=module")
        .arg("-e")
        .arg(program)
        .output()
        .map_err(|error| WebviewError::Browser(error.to_string()))?;

    if !output.status.success() {
        return Err(WebviewError::Browser(format!(
            "node bootstrap test failed: stdout={} stderr={}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        )));
    }
    Ok(())
}

#[test]
fn test_bootstrap_runtime_urls_follow_rewritten_base_href() -> Result<()> {
    let document_url = Url::parse("https://example.test/docs/index.html")?;
    let script = bootstrap_script("/webview/", &document_url);
    let program = format!(
        r#"
const calls = [];
const runtimePrefix = "/webview-runtime/";
const gateway = (url) => "/webview/" + encodeURIComponent(url);
function assert(condition, message) {{
  if (!condition) throw new Error(message);
}}
function header(init, name) {{
  return init?.headers?.get?.(name) || init?.headers?.get?.(name.toLowerCase());
}}
Object.defineProperty(globalThis, "location", {{
  value: {{
href: "http://127.0.0.1:3000/webview/" + encodeURIComponent("https://example.test/docs/index.html"),
origin: "http://127.0.0.1:3000"
  }},
  configurable: true
}});
Object.defineProperty(globalThis, "document", {{
  value: {{
querySelector(selector) {{
  if (selector !== "base[href]") return null;
  return {{
    getAttribute(name) {{
      return name === "href" ? gateway("https://example.test/assets/") : null;
    }}
  }};
}}
  }},
  configurable: true
}});
class Request {{
  constructor(input) {{
this.url = String(input);
  }}
}}
globalThis.Request = Request;
globalThis.fetch = function(input, init) {{
  calls.push(["fetch", input instanceof Request ? input.url : String(input), init]);
  return "fetch-result";
}};
class XMLHttpRequest {{
  setRequestHeader(name, value) {{
calls.push(["xhr-header", name, value]);
  }}
  open(method, url, async, user, password) {{
calls.push(["xhr", method, url, async, user, password]);
  }}
}}
globalThis.XMLHttpRequest = XMLHttpRequest;
class Element {{
  setAttribute(name, value) {{
calls.push(["attribute", name, value]);
  }}
}}
globalThis.Element = Element;
class HTMLImageElement extends Element {{
  set src(value) {{
calls.push(["property", "img.src", value]);
  }}
}}
globalThis.HTMLImageElement = HTMLImageElement;
{script}
await fetch("api/data");
const xhr = new globalThis.XMLHttpRequest();
xhr.open("POST", "forms/submit", true);
const element = new Element();
element.setAttribute("src", "image.png");
const image = new globalThis.HTMLImageElement();
image.src = "property.png";
const alreadyGateway = await fetch("http://127.0.0.1:3000" + gateway("https://example.test/kept"));
const actual = JSON.stringify(calls);
assert(calls.some((call) => call[0] === "fetch" && String(call[1]).startsWith(runtimePrefix) && header(call[2], "X-Rings-Webview-Target") === "https://example.test/assets/api/data"), "fetch did not use base href: " + actual);
assert(calls.some((call) => call[0] === "xhr" && String(call[2]).startsWith(runtimePrefix)), "XHR did not use base href: " + actual);
assert(calls.some((call) => call[0] === "xhr-header" && call[1] === "X-Rings-Webview-Target" && call[2] === "https://example.test/assets/forms/submit"), "XHR target header did not use base href: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "src" && call[2] === gateway("https://example.test/assets/image.png")), "setAttribute did not use base href: " + actual);
assert(calls.some((call) => call[0] === "property" && call[1] === "img.src" && call[2] === gateway("https://example.test/assets/property.png")), "property setter did not use base href: " + actual);
assert(calls.some((call) => call[0] === "fetch" && String(call[1]).startsWith(runtimePrefix) && header(call[2], "X-Rings-Webview-Target") === "https://example.test/kept"), "same-origin gateway URL was encoded again: " + actual);
"#,
        script = script
    );

    let output = std::process::Command::new("node")
        .arg("--input-type=module")
        .arg("-e")
        .arg(program)
        .output()
        .map_err(|error| WebviewError::Browser(error.to_string()))?;

    if !output.status.success() {
        return Err(WebviewError::Browser(format!(
            "node base href bootstrap test failed: stdout={} stderr={}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        )));
    }
    Ok(())
}

#[test]
fn test_bootstrap_captures_srcdoc_source_at_init_and_guards_malformed_or_cross_origin_urls(
) -> Result<()> {
    let document_url = Url::parse("https://example.test/docs/index.html")?;
    let script = bootstrap_script("/webview/", &document_url);
    let program = format!(
        r#"
const calls = [];
const gateway = (url) => "/webview/" + encodeURIComponent(url);
function assert(condition, message) {{
  if (!condition) throw new Error(message);
}}
let bootstrapNode = {{
  textContent: "/*immutable-bootstrap-source*/",
  hasAttribute(name) {{ return name === "data-rings-webview-bootstrap"; }}
}};
class MockElement {{
  constructor(tagName) {{
    this.nodeType = 1;
    this.tagName = String(tagName || "").toUpperCase();
    this.attributes = [];
    this.children = [];
    this.textContent = "";
    this.parentNode = null;
    this.nextSibling = null;
  }}
  get firstChild() {{
    return this.children[0] || null;
  }}
  setAttribute(name, value) {{
    const existing = this.attributes.find((attr) => attr.name === name);
    if (existing) existing.value = String(value);
    else this.attributes.push({{ name, value: String(value) }});
    calls.push(["attribute", this.tagName, name, String(value)]);
  }}
  getAttribute(name) {{
    const existing = this.attributes.find((attr) => attr.name === name);
    return existing ? existing.value : null;
  }}
  insertBefore(node, anchor) {{
    node.parentNode = this;
    const index = anchor ? this.children.indexOf(anchor) : -1;
    if (index >= 0) this.children.splice(index, 0, node);
    else this.children.push(node);
    for (let i = 0; i < this.children.length; i += 1) {{
      this.children[i].nextSibling = this.children[i + 1] || null;
    }}
    return node;
  }}
  querySelector(selector) {{
    if (selector === "head") return this.children.find((child) => child.tagName === "HEAD") || null;
    if (selector === "base[href]") return this.children.find((child) => child.tagName === "BASE" && child.getAttribute("href")) || null;
    return null;
  }}
  querySelectorAll() {{
    return [...this.children];
  }}
  get innerHTML() {{
    return this.children.map((child) => {{
      const attrs = child.attributes.map((a) => `${{a.name}}="${{a.value}}"`).join(" ");
      return `<${{child.tagName.toLowerCase()}}${{attrs ? " " + attrs : ""}}>${{child.textContent}}</${{child.tagName.toLowerCase()}}>`;
    }}).join("");
  }}
  set innerHTML(value) {{
    this.children = [];
    if (String(value).includes("<p>frame</p>")) {{
      const p = new MockElement("p");
      p.textContent = "frame";
      this.insertBefore(p, null);
    }}
  }}
}}
globalThis.Element = MockElement;
Object.defineProperty(globalThis, "location", {{
  value: {{ origin: "http://127.0.0.1:3000" }},
  configurable: true
}});
Object.defineProperty(globalThis, "document", {{
  value: {{
    currentScript: bootstrapNode,
    querySelector(selector) {{
      if (selector === "script[data-rings-webview-bootstrap]") return bootstrapNode;
      return null;
    }},
    createElement(tagName) {{
      return new MockElement(tagName);
    }}
  }},
  configurable: true
}});
{script}
// Mutate and remove the DOM bootstrap script after initialization.
bootstrapNode.textContent = "window.__tampered = true;";
bootstrapNode = null;

const iframe = new MockElement("iframe");
iframe.setAttribute("srcdoc", "<p>frame</p>");
const rewrittenSrcdoc = iframe.getAttribute("srcdoc");
assert(rewrittenSrcdoc.includes("/*immutable-bootstrap-source*/"), "srcdoc did not use init-captured bootstrap source: " + rewrittenSrcdoc);
assert(!rewrittenSrcdoc.includes("__tampered"), "srcdoc read mutated DOM bootstrap source: " + rewrittenSrcdoc);

const scriptEl = new MockElement("script");
scriptEl.setAttribute("src", "https://evil.example/assets/webview-overlay.js");
assert(scriptEl.getAttribute("src") === gateway("https://evil.example/assets/webview-overlay.js"), "cross-origin controlled asset path bypassed gateway: " + scriptEl.getAttribute("src"));

const localScriptEl = new MockElement("script");
localScriptEl.setAttribute("src", "/assets/webview-overlay.js");
assert(localScriptEl.getAttribute("src") === "/assets/webview-overlay.js", "same-origin controlled asset path was broken: " + localScriptEl.getAttribute("src"));

const malformedLink = new MockElement("a");
malformedLink.setAttribute("href", "http://[invalid");
assert(malformedLink.getAttribute("href") === "http://[invalid", "malformed URL was not preserved safely: " + malformedLink.getAttribute("href"));
"#,
        script = script
    );

    let output = std::process::Command::new("node")
        .arg("--input-type=module")
        .arg("-e")
        .arg(program)
        .output()
        .map_err(|error| WebviewError::Browser(error.to_string()))?;

    if !output.status.success() {
        return Err(WebviewError::Browser(format!(
            "node srcdoc/bounds bootstrap test failed: stdout={} stderr={}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        )));
    }
    Ok(())
}
