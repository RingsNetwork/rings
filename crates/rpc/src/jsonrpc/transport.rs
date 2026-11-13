//! Credential transport law: HTTPS, or direct HTTP to a literal loopback IP;
//! never follow redirects after validating the initial destination.

use reqwest::Client;
use reqwest::ClientBuilder;
use reqwest::Url;

use super::Result;
use super::RpcError;

/// Parse a credential destination without including its potentially sensitive URL in errors.
///
/// Post: the URL is HTTPS, or — when the transport is direct (`!proxied`) — HTTP with a parsed
/// IPv4/IPv6 loopback literal. Userinfo and fragments are rejected so credentials have one
/// explicit source.
pub(super) fn authenticated_endpoint(endpoint: &str, proxied: bool) -> Result<Url> {
    // Parsed authority is the only input to the credential destination decision.
    let url = Url::parse(endpoint).map_err(|_| RpcError::InvalidAuthenticatedEndpoint)?;
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(RpcError::InvalidAuthenticatedEndpoint);
    }
    if url.scheme() == "https" || (!proxied && url.scheme() == "http" && is_literal_loopback(&url))
    {
        Ok(url)
    } else {
        Err(RpcError::InsecureAuthenticatedEndpoint)
    }
}

/// Recognize loopback from the parsed address, never a hostname or textual prefix.
fn is_literal_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(_)) | None => false,
    }
}

/// Preserve caller transport configuration except ambient proxies and redirects, which could
/// invalidate the credential destination proof; when `proxy` is given, install that explicit
/// proxy after clearing ambient proxy discovery. TLS validation remains enabled by default.
#[cfg(not(target_family = "wasm"))]
pub(super) fn build_client(
    builder: ClientBuilder,
    proxy: Option<reqwest::Proxy>,
) -> Result<Client> {
    let builder = builder
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    let builder = match proxy {
        Some(proxy) => builder.proxy(proxy),
        None => builder,
    };
    builder.build().map_err(RpcError::TransportBuild)
}

/// Build the browser HTTP client; authenticated browser requests use Fetch directly.
#[cfg(target_family = "wasm")]
pub(super) fn build_client(builder: ClientBuilder) -> Result<Client> {
    builder.build().map_err(RpcError::TransportBuild)
}

/// Send one RPC request after validating its credential destination.
///
/// Native redirects are disabled by `build_client`; browser authenticated requests
/// use Fetch's error redirect mode because reqwest's browser adapter cannot set it.
pub(super) async fn send(
    client: &Client,
    endpoint: &str,
    token: Option<&str>,
    proxied: bool,
    body: String,
) -> Result<Vec<u8>> {
    // Preserve the parsed proof through the send boundary, including URL canonicalization.
    let authenticated_url = token
        .map(|_| authenticated_endpoint(endpoint, proxied))
        .transpose()?;
    // Both platform adapters receive exactly the canonical URL validated above.
    let endpoint = authenticated_url
        .as_ref()
        .map(Url::as_str)
        .unwrap_or(endpoint);
    #[cfg(target_family = "wasm")]
    if let Some(token) = token {
        return browser::send(endpoint, token, body).await;
    }
    // Build the payload before adding the optional credential header.
    let request = client
        .post(endpoint)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .body(body);
    // The private token can reach this header only after validation.
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    // A redirect response is rejected rather than interpreted as a JSON-RPC result.
    #[cfg(target_family = "wasm")]
    {
        let built = request
            .build()
            .map_err(|error| RpcError::Client(error.to_string()))?;
        let body = built
            .body()
            .and_then(reqwest::Body::as_bytes)
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or_default()
            .to_owned();
        return browser::send_unauthenticated(endpoint, body).await;
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                RpcError::Timeout
            } else {
                RpcError::Client(error.to_string())
            }
        })?;
        if response.status().is_redirection() {
            return Err(RpcError::RedirectRejected);
        }
        let mut response = response
            .error_for_status()
            .map_err(|error| RpcError::Client(error.to_string()))?;
        if let Some(length) = response.content_length() {
            if length > super::MAX_RPC_RESPONSE_BYTES as u64 {
                return Err(RpcError::Client(format!(
                    "RPC response length {length} exceeds maximum {} bytes",
                    super::MAX_RPC_RESPONSE_BYTES
                )));
            }
        }
        let mut collected = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| RpcError::Client(error.to_string()))?
        {
            if collected.len().saturating_add(chunk.len()) > super::MAX_RPC_RESPONSE_BYTES {
                return Err(RpcError::Client(format!(
                    "RPC response exceeds maximum {} bytes",
                    super::MAX_RPC_RESPONSE_BYTES
                )));
            }
            collected.extend_from_slice(&chunk);
        }
        Ok(collected)
    }
}

/// Browser Fetch adapter with a pre-network redirect prohibition, bounded timeout, and response size cap.
#[cfg(target_family = "wasm")]
mod browser {
    use futures_lite_or_pin::*;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    use super::super::DEFAULT_RPC_REQUEST_TIMEOUT;
    use super::super::MAX_RPC_RESPONSE_BYTES;
    use super::Result;
    use super::RpcError;

    mod futures_lite_or_pin {
        pub(super) use core::future::Future;
        pub(super) use core::pin::Pin;
        pub(super) use core::task::Context;
        pub(super) use core::task::Poll;
    }

    #[wasm_bindgen]
    extern "C" {
        /// Invoke global Fetch in either a Window or Worker execution context.
        #[wasm_bindgen(catch, js_name = fetch)]
        fn fetch(request: &web_sys::Request) -> std::result::Result<js_sys::Promise, JsValue>;
    }

    fn transport_error(has_token: bool, detail: impl Into<String>) -> RpcError {
        if has_token {
            RpcError::BrowserTransport
        } else {
            RpcError::Client(detail.into())
        }
    }

    async fn race_with_abort_timeout<F, T>(
        future: F,
        abort_controller: &web_sys::AbortController,
    ) -> std::result::Result<T, Option<JsValue>>
    where
        F: Future<Output = std::result::Result<T, JsValue>>,
    {
        let mut future = core::pin::pin!(future);
        let mut deadline = core::pin::pin!(rings_runtime::sleep(DEFAULT_RPC_REQUEST_TIMEOUT));
        core::future::poll_fn(|cx: &mut Context<'_>| {
            if let Poll::Ready(result) = future.as_mut().poll(cx) {
                return Poll::Ready(result.map_err(Some));
            }
            if Pin::new(&mut deadline).poll(cx).is_ready() {
                abort_controller.abort();
                return Poll::Ready(Err(None));
            }
            Poll::Pending
        })
        .await
    }

    /// Send credentials with redirect mode `error`, before a redirect can emit another request.
    pub(super) async fn send(endpoint: &str, token: &str, body: String) -> Result<Vec<u8>> {
        send_with_optional_token(endpoint, Some(token), body).await
    }

    /// Send an unauthenticated RPC request with bounded timeout and response size.
    pub(super) async fn send_unauthenticated(endpoint: &str, body: String) -> Result<Vec<u8>> {
        send_with_optional_token(endpoint, None, body).await
    }

    /// Send an RPC request with redirect mode `error`, bounded timeout, and bounded response size.
    async fn send_with_optional_token(
        endpoint: &str,
        token: Option<&str>,
        body: String,
    ) -> Result<Vec<u8>> {
        let has_token = token.is_some();
        let abort_controller =
            web_sys::AbortController::new().map_err(|_| RpcError::BrowserTransport)?;
        let request = build_request(endpoint, token, &body, Some(&abort_controller.signal()))?;
        let fetch_promise =
            fetch(&request).map_err(|_| transport_error(has_token, "browser RPC fetch failed"))?;
        let response =
            match race_with_abort_timeout(JsFuture::from(fetch_promise), &abort_controller).await {
                Ok(value) => value
                    .dyn_into::<web_sys::Response>()
                    .map_err(|_| transport_error(has_token, "invalid browser RPC response"))?,
                Err(None) => return Err(RpcError::Timeout),
                Err(Some(_)) => {
                    return Err(transport_error(has_token, "browser RPC fetch rejected"));
                }
            };
        if !response.ok() {
            return Err(transport_error(
                has_token,
                format!("HTTP {}", response.status()),
            ));
        }
        if let Ok(Some(length_str)) = response.headers().get("content-length") {
            if let Ok(length) = length_str.trim().parse::<usize>() {
                if length > MAX_RPC_RESPONSE_BYTES {
                    return Err(transport_error(
                        has_token,
                        format!("RPC response exceeds maximum {MAX_RPC_RESPONSE_BYTES} bytes"),
                    ));
                }
            }
        }
        let buffer_promise = response
            .array_buffer()
            .map_err(|_| transport_error(has_token, "failed to read browser RPC body"))?;
        let buffer = match race_with_abort_timeout(
            JsFuture::from(buffer_promise),
            &abort_controller,
        )
        .await
        {
            Ok(buffer) => buffer,
            Err(None) => return Err(RpcError::Timeout),
            Err(Some(_)) => {
                return Err(transport_error(has_token, "browser RPC body read failed"));
            }
        };
        let bytes = js_sys::Uint8Array::new(&buffer);
        let len = usize::try_from(bytes.byte_length()).unwrap_or(usize::MAX);
        if len > MAX_RPC_RESPONSE_BYTES {
            return Err(transport_error(
                has_token,
                format!("RPC response exceeds maximum {MAX_RPC_RESPONSE_BYTES} bytes"),
            ));
        }
        Ok(bytes.to_vec())
    }

    /// Build the browser request whose redirect mode prevents any second-hop disclosure.
    fn build_request(
        endpoint: &str,
        token: Option<&str>,
        body: &str,
        signal: Option<&web_sys::AbortSignal>,
    ) -> Result<web_sys::Request> {
        let has_token = token.is_some();
        // The browser enforces TLS certificate validation and rejects any redirect.
        let options = web_sys::RequestInit::new();
        options.set_method("POST");
        options.set_redirect(web_sys::RequestRedirect::Error);
        options.set_body(&JsValue::from_str(body));
        options.set_signal(signal);
        // Invalid URLs or header values fail before invoking Fetch.
        let request = web_sys::Request::new_with_str_and_init(endpoint, &options)
            .map_err(|_| transport_error(has_token, "invalid browser RPC endpoint"))?;
        let headers = request.headers();
        headers
            .set("Content-Type", "application/json")
            .map_err(|_| transport_error(has_token, "failed to set Content-Type"))?;
        headers
            .set("Accept", "application/json")
            .map_err(|_| transport_error(has_token, "failed to set Accept"))?;
        if let Some(token) = token {
            headers
                .set("Authorization", &format!("Bearer {token}"))
                .map_err(|_| RpcError::BrowserTransport)?;
        }
        Ok(request)
    }

    /// Browser-native request properties witness the policy passed to Fetch.
    #[cfg(test)]
    mod tests {
        use wasm_bindgen_test::wasm_bindgen_test;

        use super::*;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        /// Every authenticated browser request rejects redirects before another fetch.
        #[wasm_bindgen_test]
        fn authenticated_fetch_disallows_redirects() -> Result<()> {
            // Request construction carries the browser redirect prohibition.
            let request = build_request(
                "https://example.com/rpc",
                Some("synthetic-token"),
                "{}",
                None,
            )?;
            assert_eq!(request.redirect(), web_sys::RequestRedirect::Error);
            assert_eq!(request.method(), "POST");
            assert_eq!(
                request
                    .headers()
                    .get("Authorization")
                    .map_err(|_| RpcError::BrowserTransport)?,
                Some("Bearer synthetic-token".to_owned())
            );
            Ok(())
        }
    }
}
