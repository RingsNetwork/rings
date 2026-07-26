use std::rc::Rc;

use js_sys::Object;
use js_sys::Reflect;
use rings_node::onion::OnionExitPolicy;
use rings_node::prelude::rings_core::ecc::SecretKey;
use rings_node::prelude::rings_core::prelude::uuid;
use rings_node::prelude::rings_core::session::SessionSk;
use rings_node::prelude::rings_core::storage::idb::IdbStorage;
use rings_node::prelude::rings_core::utils::js_utils::window_sleep;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::processor::ProcessorConfig;
use rings_node::provider::Provider;
use rings_webview::browser::BOOTSTRAP_MARKER;
use rings_webview::GatewayHeader;
use rings_webview::GatewayPrefix;
use rings_webview::GatewayResponse;
use rings_webview::Result as WebviewResult;
use rings_webview::TargetUrl;
use rings_webview::WebviewError;
use serde::Deserialize;
use url::Url;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;

use super::WebviewHostOutcome;
use super::WebviewHostRequest;
use super::WebviewNode;
use super::GATEWAY_PREFIX;

const TEST_DHT_FINGER_TABLE_SIZE: usize = 8;
const TEST_NETWORK_ID: u32 = 665;
const TEST_ICE_SERVERS: &str = "stun://stun.l.google.com:19302";
const TEST_STABILIZE_INTERVAL_SECS: u64 = 15;
const FIXTURE_AUTHORITY: &str = "fixture.rings.test:443";
const FIXTURE_INDEX: &str = "https://fixture.rings.test/index.html";
const FIXTURE_CSS: &str = "https://fixture.rings.test/site.css";
const FIXTURE_API: &str = "https://fixture.rings.test/api/data";
const FIXTURE_SUBMIT: &str = "https://fixture.rings.test/forms/submit";

#[derive(Debug, Deserialize)]
struct FetchCall {
    url: String,
    method: String,
    body: String,
}

#[wasm_bindgen_test(async)]
async fn webview_node_fetches_page_resources_through_browser_onion_exit() {
    let installed = install_mock_exit_fetch();
    assert!(installed.is_ok(), "install mock exit fetch: {installed:?}");

    let result = run_browser_onion_webview_flow().await;
    let restored = restore_mock_exit_fetch();

    assert!(restored.is_ok(), "restore mock exit fetch: {restored:?}");
    assert!(
        result.is_ok(),
        "browser onion WebView flow failed: {result:?}"
    );
}

async fn run_browser_onion_webview_flow() -> WebviewResult<()> {
    let storage_suffix = uuid::Uuid::new_v4().to_simple().to_string();
    let client = browser_provider(
        &format!("rings-webview-onion-client-{storage_suffix}"),
        None,
    )
    .await?;
    let exit = browser_provider(
        &format!("rings-webview-onion-exit-{storage_suffix}"),
        Some(FIXTURE_AUTHORITY),
    )
    .await?;
    let _client_listener = client.listen();
    let _exit_listener = exit.listen();
    connect_browser_providers(&client, &exit).await?;
    window_sleep(1_000).await.map_err(js_webview_error)?;

    let node = WebviewNode::new(client, controlled_origin()?)?;
    let index_target = TargetUrl::parse(FIXTURE_INDEX)?;
    let index = retry_gateway_navigation(&node, &index_target).await?;
    expect_status(&index, "index navigation", 200)?;
    let index_body = utf8_body(index)?;
    assert_contains(&index_body, "Rings Onion Fixture")?;
    assert_contains(&index_body, BOOTSTRAP_MARKER)?;
    assert_contains(&index_body, gateway_path(FIXTURE_CSS)?.as_str())?;
    assert_contains(
        &index_body,
        gateway_path("https://fixture.rings.test/hero.png")?.as_str(),
    )?;

    let css = expect_response(
        node.handle(WebviewHostRequest::subresource(
            controlled_gateway_target(FIXTURE_CSS)?,
            Some(index_target.clone()),
            "GET",
            Vec::new(),
            Vec::new(),
        ))
        .await?,
        "stylesheet subresource",
    )?;
    expect_status(&css, "stylesheet subresource", 200)?;
    let css_body = utf8_body(css)?;
    assert_contains(
        &css_body,
        gateway_path("https://fixture.rings.test/bg.png")?.as_str(),
    )?;

    let api = expect_response(
        node.handle(WebviewHostRequest::fetch(
            controlled_gateway_target(FIXTURE_API)?,
            index_target.clone(),
            "GET",
            vec![GatewayHeader::new("accept", "application/json")?],
            Vec::new(),
        ))
        .await?,
        "runtime fetch",
    )?;
    expect_status(&api, "runtime fetch", 200)?;
    assert_contains(&utf8_body(api)?, "through browser onion exit")?;

    let submit = expect_response(
        node.handle(WebviewHostRequest::xhr(
            controlled_gateway_target(FIXTURE_SUBMIT)?,
            index_target,
            "POST",
            vec![GatewayHeader::new("content-type", "text/plain")?],
            b"name=value".to_vec(),
        ))
        .await?,
        "runtime xhr",
    )?;
    expect_status(&submit, "runtime xhr", 200)?;
    assert_contains(&utf8_body(submit)?, "name=value")?;

    let calls = fetch_calls()?;
    assert_fetch_call(&calls, FIXTURE_INDEX, "GET", None)?;
    assert_fetch_call(&calls, FIXTURE_CSS, "GET", None)?;
    assert_fetch_call(&calls, FIXTURE_API, "GET", None)?;
    assert_fetch_call(&calls, FIXTURE_SUBMIT, "POST", Some("name=value"))?;
    Ok(())
}

async fn browser_provider(
    storage_name: &str,
    exit_target: Option<&str>,
) -> WebviewResult<Rc<Provider>> {
    let session_sk = SessionSk::new_with_seckey(&SecretKey::random()).map_err(|error| {
        WebviewError::Transport(format!("build browser session key: {error:?}"))
    })?;
    let mut config = ProcessorConfig::new(
        TEST_NETWORK_ID,
        TEST_ICE_SERVERS.to_string(),
        session_sk,
        TEST_STABILIZE_INTERVAL_SECS,
    );
    if let Some(target) = exit_target {
        let policy = OnionExitPolicy::from_target_strings(vec![target.to_string()], Vec::new())
            .map_err(|error| WebviewError::Transport(format!("build exit policy: {error:?}")))?;
        config = config.enable_https_onion_exit().onion_exit_policy(policy);
    }
    let storage = Box::new(
        IdbStorage::new_with_cap_and_name(50_000, storage_name)
            .await
            .map_err(|error| WebviewError::Transport(format!("open idb storage: {error:?}")))?,
    );
    let processor = ProcessorBuilder::from_config(&config)
        .map_err(|error| WebviewError::Transport(format!("build processor config: {error:?}")))?
        .storage(storage)
        .dht_finger_table_size(TEST_DHT_FINGER_TABLE_SIZE)
        .build()
        .map_err(|error| WebviewError::Transport(format!("build processor: {error:?}")))?;
    let provider = Rc::new(provider_from_processor(processor));
    provider
        .set_backend()
        .map_err(|error| WebviewError::Transport(format!("install backend: {error:?}")))?;
    if let Some(target) = exit_target {
        provider
            .install_onion_https_exit(vec![target.to_string()], Vec::new())
            .map_err(|error| {
                WebviewError::Transport(format!("install onion HTTPS exit: {error:?}"))
            })?;
    }
    Ok(provider)
}

#[expect(
    clippy::arc_with_non_send_sync,
    reason = "Provider::from_processor requires Arc<Processor>; this browser-only test stores Provider in Rc after the API boundary"
)]
fn provider_from_processor(processor: Processor) -> Provider {
    Provider::from_processor(std::sync::Arc::new(processor))
}

async fn connect_browser_providers(client: &Provider, exit: &Provider) -> WebviewResult<()> {
    let offer = string_field(
        &rpc(
            client,
            "createOffer",
            object(&[("did", exit.address().as_str())]),
        )
        .await?,
        "offer",
    )?;
    let answer = string_field(
        &rpc(exit, "answerOffer", object(&[("offer", offer.as_str())])).await?,
        "answer",
    )?;
    let _accepted = rpc(
        client,
        "acceptAnswer",
        object(&[("answer", answer.as_str())]),
    )
    .await?;
    Ok(())
}

async fn rpc(provider: &Provider, method: &str, params: JsValue) -> WebviewResult<JsValue> {
    JsFuture::from(provider.request(method.to_string(), params))
        .await
        .map_err(js_webview_error)
}

fn object(pairs: &[(&str, &str)]) -> JsValue {
    let object = Object::new();
    for (key, value) in pairs {
        let _set = Reflect::set(
            object.as_ref(),
            JsValue::from_str(key).as_ref(),
            JsValue::from_str(value).as_ref(),
        );
    }
    object.into()
}

fn string_field(value: &JsValue, field: &str) -> WebviewResult<String> {
    Reflect::get(value, &JsValue::from_str(field))
        .map_err(js_webview_error)?
        .as_string()
        .ok_or_else(|| WebviewError::Browser(format!("missing string field {field:?}")))
}

async fn retry_gateway_navigation(
    node: &WebviewNode,
    target: &TargetUrl,
) -> WebviewResult<GatewayResponse> {
    let mut last_error = None;
    for _ in 0..60 {
        match gateway_navigation(node, target).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                last_error = Some(error.to_string());
                window_sleep(250).await.map_err(js_webview_error)?;
            }
        }
    }
    Err(WebviewError::Transport(format!(
        "gateway navigation did not find a browser onion exit: {}",
        last_error.unwrap_or_else(|| "no attempt was made".to_string())
    )))
}

async fn gateway_navigation(
    node: &WebviewNode,
    target: &TargetUrl,
) -> WebviewResult<GatewayResponse> {
    let redirect = node
        .handle(WebviewHostRequest::navigation(target.clone()))
        .await?;
    let WebviewHostOutcome::Redirect(gateway_url) = redirect else {
        return Err(WebviewError::Transport(
            "external navigation did not redirect to the controlled gateway".to_string(),
        ));
    };
    let served = node
        .handle(WebviewHostRequest::navigation(TargetUrl::parse(
            gateway_url.as_str(),
        )?))
        .await?;
    expect_response(served, "gateway navigation")
}

fn expect_response(
    outcome: WebviewHostOutcome,
    context: &'static str,
) -> WebviewResult<GatewayResponse> {
    match outcome {
        WebviewHostOutcome::Response(response) => Ok(response),
        other => Err(WebviewError::Transport(format!(
            "{context} returned {other:?}, expected response"
        ))),
    }
}

fn expect_status(
    response: &GatewayResponse,
    context: &'static str,
    expected: u16,
) -> WebviewResult<()> {
    if response.status == expected {
        Ok(())
    } else {
        Err(WebviewError::Transport(format!(
            "{context} returned status {}, expected {expected}",
            response.status
        )))
    }
}

fn controlled_origin() -> WebviewResult<TargetUrl> {
    let origin = web_sys::window()
        .ok_or_else(|| WebviewError::Browser("missing browser window".to_string()))?
        .location()
        .origin()
        .map_err(js_webview_error)?;
    TargetUrl::parse(&format!("{}/", origin.trim_end_matches('/')))
}

fn controlled_gateway_target(target: &str) -> WebviewResult<TargetUrl> {
    let origin = web_sys::window()
        .ok_or_else(|| WebviewError::Browser("missing browser window".to_string()))?
        .location()
        .origin()
        .map_err(js_webview_error)?;
    let target = TargetUrl::parse(target)?;
    let prefix = GatewayPrefix::new(GATEWAY_PREFIX)?;
    TargetUrl::parse(&format!(
        "{}{}",
        origin.trim_end_matches('/'),
        prefix.encode(target.as_url())
    ))
}

fn gateway_path(target: &str) -> WebviewResult<String> {
    let target = Url::parse(target)?;
    Ok(GatewayPrefix::new(GATEWAY_PREFIX)?.encode(&target))
}

fn utf8_body(response: GatewayResponse) -> WebviewResult<String> {
    String::from_utf8(response.body)
        .map_err(|error| WebviewError::Transport(format!("response was not UTF-8: {error}")))
}

fn assert_contains(value: &str, expected: &str) -> WebviewResult<()> {
    if value.contains(expected) {
        Ok(())
    } else {
        Err(WebviewError::Transport(format!(
            "expected {expected:?} inside {value:?}"
        )))
    }
}

fn assert_fetch_call(
    calls: &[FetchCall],
    url: &str,
    method: &str,
    body_fragment: Option<&str>,
) -> WebviewResult<()> {
    let expected = Url::parse(url)?;
    if calls.iter().any(|call| {
        urls_match(&call.url, &expected)
            && call.method == method
            && body_fragment.is_none_or(|fragment| call.body.contains(fragment))
    }) {
        Ok(())
    } else {
        Err(WebviewError::Transport(format!(
            "missing exit fetch call {method} {url}; calls: {calls:?}"
        )))
    }
}

fn urls_match(actual: &str, expected: &Url) -> bool {
    Url::parse(actual).is_ok_and(|actual| {
        actual.scheme() == expected.scheme()
            && actual.host_str() == expected.host_str()
            && actual.port_or_known_default() == expected.port_or_known_default()
            && actual.path() == expected.path()
            && actual.query() == expected.query()
    })
}

fn fetch_calls() -> WebviewResult<Vec<FetchCall>> {
    let text = js_sys::eval("JSON.stringify(globalThis.__ringsWebviewOnionFetchLog || [])")
        .map_err(js_webview_error)?
        .as_string()
        .ok_or_else(|| WebviewError::Browser("fetch log was not a string".to_string()))?;
    serde_json::from_str(text.as_str())
        .map_err(|error| WebviewError::Transport(format!("parse fetch log: {error}")))
}

fn install_mock_exit_fetch() -> WebviewResult<()> {
    js_sys::eval(
        r#"
(() => {
  const original = globalThis.fetch;
  globalThis.__ringsWebviewOriginalFetch = original;
  globalThis.__ringsWebviewOnionFetchLog = [];
  const decodeBody = (body) => {
    if (!body) return "";
    if (typeof body === "string") return body;
    if (body instanceof Uint8Array) return new TextDecoder().decode(body);
    if (body instanceof ArrayBuffer) return new TextDecoder().decode(body);
    return String(body);
  };
  globalThis.fetch = async (input, init = {}) => {
    const request = input instanceof Request ? input : undefined;
    const url = String(request ? request.url : input);
    const method = String(init.method || request?.method || "GET").toUpperCase();
    const body = decodeBody(init.body);
    globalThis.__ringsWebviewOnionFetchLog.push({ url, method, body });
    const parsed = new URL(url);
    let status = 200;
    let contentType = "text/plain; charset=utf-8";
    let responseBody = "";
    if (parsed.origin !== "https://fixture.rings.test") {
      status = 404;
      responseBody = `unexpected fixture target ${url}`;
    } else if (parsed.pathname === "/index.html") {
      contentType = "text/html; charset=utf-8";
      responseBody = `<!doctype html>
<html>
  <head>
    <title>Rings Onion Fixture</title>
    <link rel="stylesheet" href="/site.css">
  </head>
  <body>
    <h1>Rings Onion Fixture</h1>
    <img src="/hero.png" alt="hero">
  </body>
</html>`;
    } else if (parsed.pathname === "/site.css") {
      contentType = "text/css; charset=utf-8";
      responseBody = `body { background-image: url("/bg.png"); }`;
    } else if (parsed.pathname === "/api/data") {
      contentType = "application/json";
      responseBody = JSON.stringify({ message: "through browser onion exit" });
    } else if (parsed.pathname === "/forms/submit") {
      responseBody = `submitted:${body}`;
    } else {
      status = 404;
      responseBody = `missing fixture path ${parsed.pathname}`;
    }
    return new Response(responseBody, {
      status,
      headers: {
        "content-type": contentType,
        "access-control-allow-origin": "*",
        "access-control-expose-headers": "content-type,x-fixture",
        "x-fixture": "rings-webview-onion"
      }
    });
  };
})();
"#,
    )
    .map(|_| ())
    .map_err(js_webview_error)
}

fn restore_mock_exit_fetch() -> WebviewResult<()> {
    js_sys::eval(
        r#"
(() => {
  if (globalThis.__ringsWebviewOriginalFetch) {
    globalThis.fetch = globalThis.__ringsWebviewOriginalFetch;
    delete globalThis.__ringsWebviewOriginalFetch;
  }
})();
"#,
    )
    .map(|_| ())
    .map_err(js_webview_error)
}

fn js_webview_error(error: JsValue) -> WebviewError {
    WebviewError::Browser(format!("{error:?}"))
}
