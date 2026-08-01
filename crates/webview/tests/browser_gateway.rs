//! Browser fixture coverage for the webview gateway.
#![cfg(feature = "browser")]

use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use rings_webview::browser::bootstrap_script;
use rings_webview::GatewayHeader;
use rings_webview::GatewayPrefix;
use rings_webview::GatewayRequest;
use rings_webview::GatewayRequestKind;
use rings_webview::GatewayResponse;
use rings_webview::GatewayTransport;
use rings_webview::Result;
use rings_webview::TargetUrl;
use rings_webview::WebviewError;
use rings_webview::WebviewGateway;

#[path = "browser_gateway/fixture_server.rs"]
mod fixture_server;

use fixture_server::BrowserFixtureServer;

#[derive(Clone, Default)]
struct FixtureLog {
    requests: Arc<Mutex<Vec<GatewayRequest>>>,
}

impl FixtureLog {
    fn push(&self, request: GatewayRequest) {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request);
        }
    }

    fn requests(&self) -> Result<Vec<GatewayRequest>> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .map_err(|_| WebviewError::transport("fixture log lock poisoned".to_string()))
    }
}

struct BrowserFixtureTransport {
    log: FixtureLog,
}

impl BrowserFixtureTransport {
    fn new(log: FixtureLog) -> Self {
        Self { log }
    }
}

#[async_trait(?Send)]
impl GatewayTransport for BrowserFixtureTransport {
    async fn send(&self, request: GatewayRequest) -> Result<GatewayResponse> {
        let target = request.target.clone();
        self.log.push(request);

        match target.as_str() {
            "https://example.test/docs/index.html" => response(
                200,
                "text/html; charset=utf-8",
                vec![
                    GatewayHeader::new("Set-Cookie", "sid=browser; Path=/; Secure")?,
                    GatewayHeader::new("Content-Security-Policy", "default-src 'none'")?,
                    GatewayHeader::new("X-Frame-Options", "DENY")?,
                ],
                br##"<!doctype html>
<html>
  <head>
    <title>Rings WebView Fixture</title>
    <base href="/assets/">
    <style>
      #style-bg { width: 1px; height: 1px; background-image: url("inline-bg.png"); }
    </style>
    <link rel="stylesheet" href="site.css">
  </head>
  <body>
    <h1 id="title">Rings WebView Fixture</h1>
    <img id="static-image" src="/hero.png" alt="hero">
    <img id="base-image" src="inline-image.png" alt="base">
    <div id="style-bg"></div>
    <div id="imported-bg"></div>
    <p id="fetch-result"></p>
    <p id="base-fetch-result"></p>
    <p id="xhr-result"></p>
    <form id="gateway-search-form" action="form-result.html" method="get">
      <input name="q" value="test">
      <button id="gateway-search-submit" type="submit">Search</button>
    </form>
    <script>
      async function runFixture() {
        const fetchResponse = await fetch("/api/data", {
          headers: { "X-Rings-Webview-Kind": "fetch" }
        });
        const fetchJson = await fetchResponse.json();
        document.querySelector("#fetch-result").textContent = fetchJson.message;

        const baseFetchResponse = await fetch("runtime-base.json", {
          headers: { "X-Rings-Webview-Kind": "fetch" }
        });
        const baseFetchJson = await baseFetchResponse.json();
        document.querySelector("#base-fetch-result").textContent = baseFetchJson.message;

        const xhr = new XMLHttpRequest();
        xhr.open("POST", "forms/submit");
        xhr.setRequestHeader("X-Requested-With", "XMLHttpRequest");
        xhr.onload = () => {
          document.querySelector("#xhr-result").textContent = xhr.responseText;
        };
        xhr.send("name=value");

        const dynamicImage = document.createElement("img");
        dynamicImage.id = "dynamic-image";
        dynamicImage.src = "dynamic.png";
        document.body.appendChild(dynamicImage);

        const dynamicAnchor = document.createElement("a");
        dynamicAnchor.id = "dynamic-link";
        dynamicAnchor.href = "../next";
        dynamicAnchor.textContent = "next";
        document.body.appendChild(dynamicAnchor);

        const dynamicFrame = document.createElement("iframe");
        dynamicFrame.id = "dynamic-srcdoc";
        const closeScript = "</scr" + "ipt>";
        dynamicFrame.srcdoc = `
          <img id="srcdoc-image" src="srcdoc-image.png" alt="srcdoc">
          <script>
            fetch("srcdoc-fetch.json", {
              headers: { "X-Rings-Webview-Kind": "fetch" }
            })
              .then((response) => response.json())
              .then((json) => { document.body.dataset.srcdocFetch = json.message; })
              .catch((error) => { document.body.dataset.srcdocError = String(error); });
          ${closeScript}`;
        document.body.appendChild(dynamicFrame);

        const dynamicFrameAttr = document.createElement("iframe");
        dynamicFrameAttr.id = "dynamic-srcdoc-attr";
        dynamicFrameAttr.setAttribute(
          "srcdoc",
          '<img id="srcdoc-attr-image" src="srcdoc-attr-image.png" alt="srcdoc attr">'
        );
        document.body.appendChild(dynamicFrameAttr);

        const dynamicHtml = document.createElement("div");
        dynamicHtml.id = "dynamic-html";
        dynamicHtml.innerHTML = '<img id="dynamic-html-image" src="dynamic-html-image.png" alt="dynamic html"><style>#dynamic-html-bg { width: 1px; height: 1px; background-image: url("dynamic-html-bg.png"); }</style><div id="dynamic-html-bg"></div>';
        document.body.appendChild(dynamicHtml);

        const adjacentHtml = document.createElement("div");
        adjacentHtml.id = "adjacent-html";
        document.body.appendChild(adjacentHtml);
        adjacentHtml.insertAdjacentHTML("beforeend", '<img id="adjacent-html-image" src="adjacent-html-image.png" alt="adjacent html">');

        const outerHtml = document.createElement("div");
        outerHtml.id = "outer-html-placeholder";
        document.body.appendChild(outerHtml);
        outerHtml.outerHTML = '<img id="outer-html-image" src="outer-html-image.png" alt="outer html">';

        const dynamicStyle = document.createElement("style");
        dynamicStyle.textContent = '@import "dynamic-import.css"; #dynamic-css-bg { width: 1px; height: 1px; background-image: url("dynamic-css-bg.png"); }';
        document.head.appendChild(dynamicStyle);
        const dynamicCssBg = document.createElement("div");
        dynamicCssBg.id = "dynamic-css-bg";
        document.body.appendChild(dynamicCssBg);
        const dynamicImportBg = document.createElement("div");
        dynamicImportBg.id = "dynamic-import-bg";
        document.body.appendChild(dynamicImportBg);

        const inlineStyle = document.createElement("div");
        inlineStyle.id = "dynamic-inline-style-bg";
        inlineStyle.style.backgroundImage = 'url("dynamic-inline-style-bg.png")';
        document.body.appendChild(inlineStyle);

        const cssomStyle = document.createElement("style");
        document.head.appendChild(cssomStyle);
        cssomStyle.sheet.insertRule('#cssom-insert-bg { width: 1px; height: 1px; background-image: url("cssom-insert-bg.png"); }', 0);
        const cssomInsertBg = document.createElement("div");
        cssomInsertBg.id = "cssom-insert-bg";
        document.body.appendChild(cssomInsertBg);

        const constructedSheet = new CSSStyleSheet();
        constructedSheet.replaceSync('#cssom-replace-bg { width: 1px; height: 1px; background-image: url("cssom-replace-bg.png"); }');
        document.adoptedStyleSheets = [...document.adoptedStyleSheets, constructedSheet];
        const cssomReplaceBg = document.createElement("div");
        cssomReplaceBg.id = "cssom-replace-bg";
        document.body.appendChild(cssomReplaceBg);

        const writeFrame = document.createElement("iframe");
        writeFrame.id = "document-write-srcdoc";
        writeFrame.srcdoc = '<script>document.write(\'<img id="document-write-image" src="document-write-image.png" alt="document write">\');' + closeScript;
        document.body.appendChild(writeFrame);

        const dynamicPing = document.createElement("a");
        dynamicPing.id = "dynamic-ping-link";
        dynamicPing.href = "ping-navigation.html";
        dynamicPing.ping = "ping-one ping-two";
        dynamicPing.target = "_blank";
        dynamicPing.textContent = "ping";
        document.body.appendChild(dynamicPing);

        const namespaceImage = document.createElement("img");
        namespaceImage.id = "namespace-image";
        namespaceImage.setAttributeNS(null, "src", "namespace-image.png");
        document.body.appendChild(namespaceImage);

        const refreshFrame = document.createElement("iframe");
        refreshFrame.id = "refresh-navigation-srcdoc";
        refreshFrame.srcdoc = '<script>const meta = document.createElement("meta");meta.content = "0; url=refresh-navigation.html";meta.httpEquiv = "refresh";document.head.appendChild(meta);' + closeScript;
        document.body.appendChild(refreshFrame);

        const runtimeOpen = document.createElement("button");
        runtimeOpen.id = "runtime-open-button";
        runtimeOpen.addEventListener("click", () => window.open("window-open.html", "_blank"));
        document.body.appendChild(runtimeOpen);
      }
      runFixture().catch((error) => {
        document.body.dataset.fixtureError = String(error);
      });
    </script>
  </body>
</html>"##
                    .to_vec(),
            ),
            "https://example.test/assets/site.css" => response(
                200,
                "text/css; charset=utf-8",
                Vec::new(),
                b"@import \"inline-import.css\"; #title { color: rgb(1, 2, 3); }".to_vec(),
            ),
            "https://example.test/assets/inline-import.css" => response(
                200,
                "text/css; charset=utf-8",
                Vec::new(),
                b"#imported-bg { width: 1px; height: 1px; background-image: url('inline-import-bg.png'); }"
                    .to_vec(),
            ),
            "https://example.test/assets/dynamic-import.css" => response(
                200,
                "text/css; charset=utf-8",
                Vec::new(),
                b"#dynamic-import-bg { width: 1px; height: 1px; background-image: url('dynamic-import-bg.png'); }"
                    .to_vec(),
            ),
            "https://example.test/hero.png"
            | "https://example.test/assets/dynamic.png"
            | "https://example.test/assets/inline-bg.png"
            | "https://example.test/assets/inline-image.png"
            | "https://example.test/assets/inline-import-bg.png"
            | "https://example.test/assets/srcdoc-image.png"
            | "https://example.test/assets/srcdoc-attr-image.png"
            | "https://example.test/assets/dynamic-html-image.png"
            | "https://example.test/assets/dynamic-html-bg.png"
            | "https://example.test/assets/adjacent-html-image.png"
            | "https://example.test/assets/outer-html-image.png"
            | "https://example.test/assets/dynamic-css-bg.png"
            | "https://example.test/assets/dynamic-import-bg.png"
            | "https://example.test/assets/dynamic-inline-style-bg.png"
            | "https://example.test/assets/cssom-insert-bg.png"
            | "https://example.test/assets/cssom-replace-bg.png"
            | "https://example.test/assets/document-write-image.png"
            | "https://example.test/assets/namespace-image.png" => {
                response(200, "image/png", Vec::new(), ONE_PIXEL_PNG.to_vec())
            }
            "https://example.test/api/data" => response(
                200,
                "application/json",
                Vec::new(),
                br#"{"message":"fetch ok"}"#.to_vec(),
            ),
            "https://example.test/assets/runtime-base.json" => response(
                200,
                "application/json",
                Vec::new(),
                br#"{"message":"base fetch ok"}"#.to_vec(),
            ),
            "https://example.test/assets/srcdoc-fetch.json" => response(
                200,
                "application/json",
                Vec::new(),
                br#"{"message":"srcdoc fetch ok"}"#.to_vec(),
            ),
            "https://example.test/assets/forms/submit" => response(
                200,
                "text/plain; charset=utf-8",
                Vec::new(),
                b"xhr ok".to_vec(),
            ),
            "https://example.test/assets/form-result.html?q=test" => response(
                200,
                "text/html; charset=utf-8",
                Vec::new(),
                b"<!doctype html><title>form result</title><p id=\"form-result\">test result</p>"
                    .to_vec(),
            ),
            "https://example.test/assets/ping-one"
            | "https://example.test/assets/ping-two" => {
                response(200, "text/plain; charset=utf-8", Vec::new(), Vec::new())
            }
            "https://example.test/assets/ping-navigation.html" => response(
                200,
                "text/html; charset=utf-8",
                Vec::new(),
                b"<!doctype html><title>ping navigation</title>".to_vec(),
            ),
            "https://example.test/assets/refresh-navigation.html" => response(
                200,
                "text/html; charset=utf-8",
                Vec::new(),
                b"<!doctype html><title>refresh navigation</title>".to_vec(),
            ),
            "https://example.test/assets/window-open.html" => response(
                200,
                "text/html; charset=utf-8",
                Vec::new(),
                b"<!doctype html><title>window open</title>".to_vec(),
            ),
            other => Err(WebviewError::transport(format!(
                "unexpected browser fixture request {other}"
            ))),
        }
    }
}

#[test]
fn playwright_browser_renders_gateway_fixture_without_direct_remote_requests() -> Result<()> {
    let log = FixtureLog::default();
    let prefix = GatewayPrefix::new("/webview/")?;
    let target = TargetUrl::parse("https://example.test/docs/index.html")?;
    let bootstrap = format!(
        "{}\n{}",
        bootstrap_script(prefix.as_str(), target.as_url()),
        fixture_overlay_loader()
    );
    let gateway = WebviewGateway::new(prefix.clone(), BrowserFixtureTransport::new(log.clone()))
        .with_bootstrap_script(bootstrap);
    let server = BrowserFixtureServer::start(prefix.clone(), gateway)?;
    let page_url = server.gateway_url(&prefix.encode(target.as_url()));

    run_playwright_fixture(page_url.as_str())?;

    let requests = log.requests()?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Navigation,
        "GET",
        target.as_url().as_str(),
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Subresource,
        "GET",
        "https://example.test/assets/site.css",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Subresource,
        "GET",
        "https://example.test/assets/inline-import.css",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Subresource,
        "GET",
        "https://example.test/hero.png",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Fetch,
        "GET",
        "https://example.test/api/data",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Fetch,
        "GET",
        "https://example.test/assets/runtime-base.json",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Xhr,
        "POST",
        "https://example.test/assets/forms/submit",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Navigation,
        "GET",
        "https://example.test/assets/form-result.html?q=test",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Subresource,
        "GET",
        "https://example.test/assets/dynamic.png",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Subresource,
        "GET",
        "https://example.test/assets/srcdoc-image.png",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Subresource,
        "GET",
        "https://example.test/assets/srcdoc-attr-image.png",
    )?;
    assert_recorded_target(
        &requests,
        GatewayRequestKind::Fetch,
        "GET",
        "https://example.test/assets/srcdoc-fetch.json",
    )?;
    for target in [
        "https://example.test/assets/dynamic-html-image.png",
        "https://example.test/assets/dynamic-html-bg.png",
        "https://example.test/assets/adjacent-html-image.png",
        "https://example.test/assets/outer-html-image.png",
        "https://example.test/assets/dynamic-css-bg.png",
        "https://example.test/assets/dynamic-import.css",
        "https://example.test/assets/dynamic-import-bg.png",
        "https://example.test/assets/dynamic-inline-style-bg.png",
        "https://example.test/assets/cssom-insert-bg.png",
        "https://example.test/assets/cssom-replace-bg.png",
        "https://example.test/assets/document-write-image.png",
        "https://example.test/assets/namespace-image.png",
        "https://example.test/assets/refresh-navigation.html",
    ] {
        assert_recorded_target(&requests, GatewayRequestKind::Subresource, "GET", target)?;
    }
    for target in [
        "https://example.test/assets/ping-one",
        "https://example.test/assets/ping-two",
    ] {
        assert_recorded_target(&requests, GatewayRequestKind::Fetch, "POST", target)?;
    }
    for target in [
        "https://example.test/assets/ping-navigation.html",
        "https://example.test/assets/window-open.html",
    ] {
        assert_recorded_target(&requests, GatewayRequestKind::Navigation, "GET", target)?;
    }
    assert!(requests.iter().all(|request| {
        request.target.scheme() == "https" && request.target.host_str() == Some("example.test")
    }));
    assert!(requests
        .iter()
        .all(|request| request.headers.iter().all(|header| {
            !header.name_eq("host")
                && !header.name_eq("origin")
                && !header.name_eq("referer")
                && !header.name_eq("sec-fetch-dest")
                && !header.name_eq("sec-fetch-mode")
                && !header.name_eq("sec-fetch-site")
        })));

    server.stop()?;
    Ok(())
}

fn response(
    status: u16,
    content_type: &str,
    extra_headers: Vec<GatewayHeader>,
    body: Vec<u8>,
) -> Result<GatewayResponse> {
    let mut headers = vec![GatewayHeader::new("Content-Type", content_type)?];
    headers.extend(extra_headers);
    GatewayResponse::new(status, headers, body)
}

fn run_playwright_fixture(page_url: &str) -> Result<()> {
    if !playwright_available()? {
        return Err(WebviewError::Browser(
            "Playwright is not available to run browser fixture".to_string(),
        ));
    }
    let program = format!(
        r##"
const {{ chromium }} = require("playwright");
const pageUrl = {page_url:?};

(async () => {{
  const browser = await chromium.launch({{ headless: true }});
  try {{
  const context = await browser.newContext();
  const page = await context.newPage();
  const requests = [];
  const failures = [];
  context.on("request", (request) => requests.push(request.url()));
  context.on("requestfailed", (request) => failures.push(`${{request.url()}} ${{request.failure()?.errorText || ""}}`));
  await page.goto(pageUrl, {{ waitUntil: "domcontentloaded" }});
  try {{
  await page.waitForFunction(() => {{
    const srcdocFrame = document.querySelector("#dynamic-srcdoc");
    const srcdocDoc = srcdocFrame?.contentDocument;
    const srcdocAttrFrame = document.querySelector("#dynamic-srcdoc-attr");
    const srcdocAttrDoc = srcdocAttrFrame?.contentDocument;
    const documentWriteFrame = document.querySelector("#document-write-srcdoc");
    const documentWriteDoc = documentWriteFrame?.contentDocument;
    const refreshFrame = document.querySelector("#refresh-navigation-srcdoc");
    const refreshDoc = refreshFrame?.contentDocument;
    const hasGatewayBackground = (selector) => {{
      const element = document.querySelector(selector);
      return Boolean(element && getComputedStyle(element).backgroundImage.includes("/webview/"));
    }};
    return document.querySelector("#fetch-result")?.textContent === "fetch ok"
      && document.querySelector("#base-fetch-result")?.textContent === "base fetch ok"
      && document.querySelector("#xhr-result")?.textContent === "xhr ok"
      && document.querySelector("#static-image")?.complete
      && document.querySelector("#base-image")?.complete
      && document.querySelector("#dynamic-image")?.complete
      && srcdocDoc?.querySelector("#srcdoc-image")?.complete
      && srcdocDoc?.body?.dataset?.srcdocFetch === "srcdoc fetch ok"
      && !srcdocDoc?.body?.dataset?.srcdocError
      && srcdocAttrDoc?.querySelector("#srcdoc-attr-image")?.complete
      && document.querySelector("#dynamic-html-image")?.complete
      && document.querySelector("#adjacent-html-image")?.complete
      && document.querySelector("#outer-html-image")?.complete
      && document.querySelector("#namespace-image")?.complete
      && documentWriteDoc?.querySelector("#document-write-image")?.complete
      && refreshDoc?.title === "refresh navigation"
      && hasGatewayBackground("#dynamic-html-bg")
      && hasGatewayBackground("#dynamic-css-bg")
      && hasGatewayBackground("#dynamic-import-bg")
      && hasGatewayBackground("#dynamic-inline-style-bg")
      && hasGatewayBackground("#cssom-insert-bg")
      && hasGatewayBackground("#cssom-replace-bg");
  }}, null, {{ timeout: 10000 }});
  }} catch (error) {{
    const diagnostic = await page.evaluate(() => {{
      const frameState = (selector) => {{
        const frame = document.querySelector(selector);
        return {{
          url: frame?.contentWindow?.location?.href || "",
          title: frame?.contentDocument?.title || ""
        }};
      }};
      return {{
        fixtureError: document.body.dataset.fixtureError || "",
        refresh: frameState("#refresh-navigation-srcdoc"),
        namespaceImage: document.querySelector("#namespace-image")?.src || ""
      }};
    }});
    throw new Error(`${{error.message}} fixture state=${{JSON.stringify(diagnostic)}}`);
  }}
  const pingOne = encodeURIComponent("https://example.test/assets/ping-one");
  const pingTwo = encodeURIComponent("https://example.test/assets/ping-two");
  const pingRequests = new Promise((resolve, reject) => {{
    const observed = new Set();
    const timeout = setTimeout(() => reject(new Error(`timed out waiting for gateway ping requests: ${{JSON.stringify([...observed])}}`)), 10000);
    const observe = (request) => {{
      if (request.url().includes(pingOne)) observed.add(pingOne);
      if (request.url().includes(pingTwo)) observed.add(pingTwo);
      if (observed.size === 2) {{
        clearTimeout(timeout);
        context.off("request", observe);
        resolve();
      }}
    }};
    context.on("request", observe);
  }});
  const popupPromise = page.waitForEvent("popup");
  await page.locator("#dynamic-ping-link").click();
  const popup = await popupPromise;
  await popup.waitForLoadState("domcontentloaded");
  await pingRequests;
  const runtimeOpenPopupPromise = page.waitForEvent("popup");
  await page.locator("#runtime-open-button").click();
  const runtimeOpenPopup = await runtimeOpenPopupPromise;
  await runtimeOpenPopup.waitForLoadState("domcontentloaded");
  const result = await page.evaluate(() => {{
    const title = document.querySelector("#title");
    const dynamicLink = document.querySelector("#dynamic-link");
    const staticImage = document.querySelector("#static-image");
    const baseImage = document.querySelector("#base-image");
    const dynamicImage = document.querySelector("#dynamic-image");
    const styleBg = document.querySelector("#style-bg");
    const importedBg = document.querySelector("#imported-bg");
    const srcdocFrame = document.querySelector("#dynamic-srcdoc");
    const srcdocDoc = srcdocFrame?.contentDocument;
    const srcdocImage = srcdocDoc?.querySelector("#srcdoc-image");
    const srcdocAttrFrame = document.querySelector("#dynamic-srcdoc-attr");
    const srcdocAttrDoc = srcdocAttrFrame?.contentDocument;
    const srcdocAttrImage = srcdocAttrDoc?.querySelector("#srcdoc-attr-image");
    const documentWriteFrame = document.querySelector("#document-write-srcdoc");
    const documentWriteImage = documentWriteFrame?.contentDocument?.querySelector("#document-write-image");
    const dynamicHtmlImage = document.querySelector("#dynamic-html-image");
    const adjacentHtmlImage = document.querySelector("#adjacent-html-image");
    const outerHtmlImage = document.querySelector("#outer-html-image");
    const dynamicPing = document.querySelector("#dynamic-ping-link");
    const namespaceImage = document.querySelector("#namespace-image");
    const refreshFrame = document.querySelector("#refresh-navigation-srcdoc");
    const overlayScript = document.querySelector("script[data-rings-webview-overlay-loader]");
    const backgroundImage = (selector) => {{
      const element = document.querySelector(selector);
      return element ? getComputedStyle(element).backgroundImage : "";
    }};
    return {{
      overlayMounted: Boolean(document.querySelector("#rings-webview-debug-overlay")),
      overlayScriptSrc: overlayScript?.src || "",
      titleText: title?.textContent,
      titleColor: title ? getComputedStyle(title).color : "",
      fetchText: document.querySelector("#fetch-result")?.textContent,
      baseFetchText: document.querySelector("#base-fetch-result")?.textContent,
      xhrText: document.querySelector("#xhr-result")?.textContent,
      staticImageSrc: staticImage?.src,
      staticImageComplete: Boolean(staticImage?.complete),
      baseImageSrc: baseImage?.src,
      baseImageComplete: Boolean(baseImage?.complete),
      dynamicImageSrc: dynamicImage?.src,
      dynamicImageComplete: Boolean(dynamicImage?.complete),
      dynamicLinkHref: dynamicLink?.href,
      styleBgImage: styleBg ? getComputedStyle(styleBg).backgroundImage : "",
      importedBgImage: importedBg ? getComputedStyle(importedBg).backgroundImage : "",
      dynamicHtmlImageSrc: dynamicHtmlImage?.src,
      dynamicHtmlImageComplete: Boolean(dynamicHtmlImage?.complete),
      adjacentHtmlImageSrc: adjacentHtmlImage?.src,
      adjacentHtmlImageComplete: Boolean(adjacentHtmlImage?.complete),
      outerHtmlImageSrc: outerHtmlImage?.src,
      outerHtmlImageComplete: Boolean(outerHtmlImage?.complete),
      documentWriteImageSrc: documentWriteImage?.src,
      documentWriteImageComplete: Boolean(documentWriteImage?.complete),
      dynamicHtmlBgImage: backgroundImage("#dynamic-html-bg"),
      dynamicCssBgImage: backgroundImage("#dynamic-css-bg"),
      dynamicImportBgImage: backgroundImage("#dynamic-import-bg"),
      dynamicInlineStyleBgImage: backgroundImage("#dynamic-inline-style-bg"),
      cssomInsertBgImage: backgroundImage("#cssom-insert-bg"),
      cssomReplaceBgImage: backgroundImage("#cssom-replace-bg"),
      dynamicPingHref: dynamicPing?.href,
      dynamicPingValue: dynamicPing?.getAttribute("ping"),
      namespaceImageSrc: namespaceImage?.src,
      refreshFrameUrl: refreshFrame?.contentWindow?.location?.href || "",
      srcdocImageSrc: srcdocImage?.src,
      srcdocImageComplete: Boolean(srcdocImage?.complete),
      srcdocFetchText: srcdocDoc?.body?.dataset?.srcdocFetch || "",
      srcdocError: srcdocDoc?.body?.dataset?.srcdocError || "",
      srcdocAttrImageSrc: srcdocAttrImage?.src,
      srcdocAttrImageComplete: Boolean(srcdocAttrImage?.complete),
      fixtureError: document.body.dataset.fixtureError || ""
    }};
  }});
  await page.locator("#gateway-search-submit").click();
  const encodedFormTarget = encodeURIComponent("https://example.test/assets/form-result.html?q=test");
  await page.waitForFunction((target) => (
    document.title === "form result"
      && document.querySelector("#form-result")?.textContent === "test result"
      && location.href.endsWith(`/webview/${{target}}`)
  ), encodedFormTarget);
  const formResult = await page.evaluate(() => ({{
    text: document.querySelector("#form-result")?.textContent || "",
    title: document.title,
    url: location.href
  }}));
  if (formResult.title !== "form result" || formResult.text !== "test result") {{
    throw new Error(`GET form result did not render: ${{JSON.stringify(formResult)}}`);
  }}
  if (!formResult.url.endsWith(`/webview/${{encodedFormTarget}}`)) {{
    throw new Error(`GET form escaped its encoded gateway target: ${{JSON.stringify(formResult)}}`);
  }}
  const directRemoteRequests = requests.filter((url) => {{
    try {{
      return new URL(url).hostname === "example.test";
    }} catch (_error) {{
      return false;
    }}
  }});
  const failuresWithoutFavicon = failures.filter((failure) => !failure.includes("/favicon.ico"));
  if (directRemoteRequests.length > 0) {{
    throw new Error(`direct remote requests escaped gateway: ${{directRemoteRequests.join(", ")}}`);
  }}
  if (failuresWithoutFavicon.length > 0) {{
    throw new Error(`browser request failures: ${{failuresWithoutFavicon.join(", ")}}`);
  }}
  if (result.fixtureError) {{
    throw new Error(`page fixture error: ${{result.fixtureError}}`);
  }}
  if (result.titleText !== "Rings WebView Fixture") {{
    throw new Error(`page title did not render: ${{JSON.stringify(result)}}`);
  }}
  if (!result.overlayMounted || !result.overlayScriptSrc.endsWith("/assets/webview-overlay.js")) {{
    throw new Error(`webview overlay did not mount from local asset: ${{JSON.stringify(result)}}`);
  }}
  if (result.titleColor !== "rgb(1, 2, 3)") {{
    throw new Error(`stylesheet did not apply: ${{JSON.stringify(result)}}`);
  }}
  if (result.fetchText !== "fetch ok" || result.xhrText !== "xhr ok") {{
    throw new Error(`dynamic requests did not complete: ${{JSON.stringify(result)}}`);
  }}
  if (result.baseFetchText !== "base fetch ok") {{
    throw new Error(`base-relative fetch did not complete: ${{JSON.stringify(result)}}`);
  }}
  if (result.srcdocError) {{
    throw new Error(`srcdoc fixture error: ${{result.srcdocError}} ${{JSON.stringify(result)}}`);
  }}
  if (result.srcdocFetchText !== "srcdoc fetch ok") {{
    throw new Error(`srcdoc fetch did not complete: ${{JSON.stringify(result)}}`);
  }}
  if (!result.staticImageSrc.includes("/webview/") || !result.baseImageSrc.includes("/webview/") || !result.dynamicImageSrc.includes("/webview/")) {{
    throw new Error(`image URLs did not stay on gateway: ${{JSON.stringify(result)}}`);
  }}
  if (!result.srcdocImageSrc.includes("/webview/") || !result.srcdocAttrImageSrc.includes("/webview/")) {{
    throw new Error(`srcdoc image URLs did not stay on gateway: ${{JSON.stringify(result)}}`);
  }}
  if (!result.dynamicHtmlImageSrc.includes("/webview/") || !result.adjacentHtmlImageSrc.includes("/webview/") || !result.outerHtmlImageSrc.includes("/webview/") || !result.documentWriteImageSrc.includes("/webview/")) {{
    throw new Error(`runtime HTML URLs did not stay on gateway: ${{JSON.stringify(result)}}`);
  }}
  if (!result.namespaceImageSrc.includes("/webview/")) {{
    throw new Error(`setAttributeNS URL did not stay on gateway: ${{JSON.stringify(result)}}`);
  }}
  if (!result.styleBgImage.includes("/webview/") || !result.importedBgImage.includes("/webview/")) {{
    throw new Error(`CSS URLs did not stay on gateway: ${{JSON.stringify(result)}}`);
  }}
  if (![result.dynamicHtmlBgImage, result.dynamicCssBgImage, result.dynamicImportBgImage, result.dynamicInlineStyleBgImage, result.cssomInsertBgImage, result.cssomReplaceBgImage].every((value) => value.includes("/webview/"))) {{
    throw new Error(`runtime CSS URLs did not stay on gateway: ${{JSON.stringify(result)}}`);
  }}
  if (!result.dynamicLinkHref.includes("/webview/")) {{
    throw new Error(`dynamic link URL did not stay on gateway: ${{JSON.stringify(result)}}`);
  }}
  if (!result.dynamicPingHref.includes("/webview/") || !result.dynamicPingValue.includes("/webview/")) {{
    throw new Error(`dynamic ping URLs did not stay on gateway: ${{JSON.stringify(result)}}`);
  }}
  if (!result.refreshFrameUrl.includes("/webview/")) {{
    throw new Error(`runtime refresh navigation did not stay on gateway: ${{JSON.stringify(result)}}`);
  }}
  }} finally {{
  await browser.close();
  }}
}})().catch(async (error) => {{
  console.error(error && error.stack ? error.stack : String(error));
  process.exit(1);
}});
"##
    );
    let output = Command::new("node")
        .arg("-e")
        .arg(program)
        .output()
        .map_err(|error| WebviewError::Browser(error.to_string()))?;
    if !output.status.success() {
        return Err(WebviewError::Browser(format!(
            "Playwright browser fixture failed: stdout={} stderr={}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        )));
    }
    Ok(())
}

fn fixture_overlay_loader() -> &'static str {
    r#"
(() => {
  if (globalThis.__ringsWebviewGateway?.loadLocalScript?.("/assets/webview-overlay.js", "data-rings-webview-overlay-loader")) return;
  const script = document.createElement("script");
  script.src = "/assets/webview-overlay.js";
  script.async = false;
  script.dataset.ringsWebviewOverlayLoader = "";
  (document.head || document.documentElement).append(script);
})();
"#
}

fn playwright_available() -> Result<bool> {
    let output = Command::new("node")
        .arg("-e")
        .arg("require.resolve('playwright')")
        .output()
        .map_err(|error| WebviewError::Browser(error.to_string()))?;
    Ok(output.status.success())
}

fn assert_recorded_target(
    requests: &[GatewayRequest],
    kind: GatewayRequestKind,
    method: &str,
    target: &str,
) -> Result<()> {
    let found = requests.iter().any(|request| {
        request.kind == kind && request.method == method && request.target.as_str() == target
    });
    if found {
        Ok(())
    } else {
        Err(WebviewError::transport(format!(
            "missing recorded {kind:?} {method} request to {target}"
        )))
    }
}

const ONE_PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];
