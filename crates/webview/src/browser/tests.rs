use super::*;
use crate::error::WebviewError;

#[test]
fn bootstrap_hooks_browser_network_entrypoints() -> Result<()> {
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
    assert!(script.contains("HTMLBaseElement"));
    assert!(script.contains("setAttribute"));
    assert!(script.contains("encodeURIComponent"));
    Ok(())
}

#[test]
fn runtime_urls_resolve_against_target_document_not_gateway_location() -> Result<()> {
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
fn bootstrap_executes_runtime_routing_in_javascript() -> Result<()> {
    let document_url = Url::parse("https://example.test/docs/index.html")?;
    let script = bootstrap_script("/webview/", &document_url);
    let program = format!(
        r#"
const calls = [];
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
xhr.open("POST", "forms/x", true);
assertThrows(() => new globalThis.WebSocket("/socket"), "WebSocket was not blocked");
assertThrows(() => new globalThis.EventSource("events"), "EventSource was not blocked");
const beaconResult = navigator.sendBeacon("/beacon", "payload");
assert(beaconResult === false, "sendBeacon was not blocked");
assertThrows(() => new globalThis.Worker("worker.js"), "Worker was not blocked");
assertThrows(() => new globalThis.SharedWorker("shared.js"), "SharedWorker was not blocked");
const element = new Element();
element.setAttribute("src", "image.png");
element.setAttribute("srcset", "small.png 1x, /big.png 2x");
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
const actual = JSON.stringify(calls);
assert(calls.some((call) => call[0] === "fetch" && call[1] === gateway("https://example.test/api/data")), "fetch URL was not rewritten: " + actual);
assert(calls.some((call) => call[0] === "fetch" && call[1] === gateway("https://example.test/docs/forms/submit")), "Request URL was not rewritten: " + actual);
assert(calls.some((call) => call[0] === "xhr" && call[2] === gateway("https://example.test/docs/forms/x")), "XHR URL was not rewritten: " + actual);
assert(!calls.some((call) => ["websocket", "eventsource", "beacon", "worker", "sharedworker"].includes(call[0])), "unsupported native entrypoint was called: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "src" && call[2] === gateway("https://example.test/docs/image.png")), "setAttribute src was not rewritten: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "srcset" && call[2].includes(gateway("https://example.test/docs/small.png")) && call[2].includes(gateway("https://example.test/big.png"))), "setAttribute srcset was not rewritten: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "aria-label" && call[2] === "unchanged"), "non-URL attribute changed: " + actual);
assert(calls.some((call) => call[0] === "property" && call[1] === "img.src" && call[2] === gateway("https://example.test/docs/property.png")), "img.src property was not rewritten: " + actual);
assert(calls.some((call) => call[0] === "property" && call[1] === "img.srcset" && call[2].includes(gateway("https://example.test/docs/property-small.png")) && call[2].includes(gateway("https://example.test/property-big.png"))), "img.srcset property was not rewritten: " + actual);
assert(submitEvent.prevented && submitEvent.stopped, "GET form submission was not intercepted");
assert(calls.some((call) => call[0] === "navigate" && call[1] === gateway("https://example.test/docs/search?q=test")), "GET form query did not use native gateway navigation: " + actual);
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
fn bootstrap_runtime_urls_follow_rewritten_base_href() -> Result<()> {
    let document_url = Url::parse("https://example.test/docs/index.html")?;
    let script = bootstrap_script("/webview/", &document_url);
    let program = format!(
        r#"
const calls = [];
const gateway = (url) => "/webview/" + encodeURIComponent(url);
function assert(condition, message) {{
  if (!condition) throw new Error(message);
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
assert(calls.some((call) => call[0] === "fetch" && call[1] === gateway("https://example.test/assets/api/data")), "fetch did not use base href: " + actual);
assert(calls.some((call) => call[0] === "xhr" && call[2] === gateway("https://example.test/assets/forms/submit")), "XHR did not use base href: " + actual);
assert(calls.some((call) => call[0] === "attribute" && call[1] === "src" && call[2] === gateway("https://example.test/assets/image.png")), "setAttribute did not use base href: " + actual);
assert(calls.some((call) => call[0] === "property" && call[1] === "img.src" && call[2] === gateway("https://example.test/assets/property.png")), "property setter did not use base href: " + actual);
assert(calls.some((call) => call[0] === "fetch" && call[1] === gateway("https://example.test/kept")), "same-origin gateway URL was encoded again: " + actual);
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
