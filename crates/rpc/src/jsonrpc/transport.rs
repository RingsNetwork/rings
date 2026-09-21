//! Credential transport law: HTTPS, or direct HTTP to a literal loopback IP;
//! never follow redirects after validating the initial destination.

use reqwest::Client;
use reqwest::ClientBuilder;
use reqwest::Url;

use super::Result;
use super::RpcError;

/// Parse a credential destination without including its potentially sensitive URL in errors.
///
/// Post: the URL is HTTPS, or HTTP with a parsed IPv4/IPv6 loopback literal.
/// Userinfo and fragments are rejected so credentials have one explicit source.
pub(super) fn authenticated_endpoint(endpoint: &str) -> Result<Url> {
    // Parsed authority is the only input to the credential destination decision.
    let url = Url::parse(endpoint).map_err(|_| RpcError::InvalidAuthenticatedEndpoint)?;
    if url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(RpcError::InvalidAuthenticatedEndpoint);
    }
    if url.scheme() == "https" || (url.scheme() == "http" && is_literal_loopback(&url)) {
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

/// Preserve caller transport configuration except proxies and redirects, which could
/// invalidate the credential destination proof. TLS validation remains enabled by default.
pub(super) fn build_client(builder: ClientBuilder) -> Result<Client> {
    #[cfg(not(target_family = "wasm"))]
    let builder = builder
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
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
    body: String,
) -> Result<Vec<u8>> {
    // Preserve the parsed proof through the send boundary, including URL canonicalization.
    let authenticated_url = token
        .map(|_| authenticated_endpoint(endpoint))
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
    let response = request
        .send()
        .await
        .map_err(|error| RpcError::Client(error.to_string()))?;
    if response.status().is_redirection() {
        return Err(RpcError::RedirectRejected);
    }
    response
        .error_for_status()
        .map_err(|error| RpcError::Client(error.to_string()))?
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| RpcError::Client(error.to_string()))
}

/// Browser Fetch adapter with a pre-network redirect prohibition.
#[cfg(target_family = "wasm")]
mod browser {
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    use super::Result;
    use super::RpcError;

    #[wasm_bindgen]
    extern "C" {
        /// Invoke global Fetch in either a Window or Worker execution context.
        #[wasm_bindgen(catch, js_name = fetch)]
        fn fetch(request: &web_sys::Request) -> std::result::Result<js_sys::Promise, JsValue>;
    }

    /// Send credentials with redirect mode `error`, before a redirect can emit another request.
    pub(super) async fn send(endpoint: &str, token: &str, body: String) -> Result<Vec<u8>> {
        // Request construction carries the browser redirect prohibition.
        let request = credential_request(endpoint, token, &body)?;
        // Fetch resolves only after its built-in redirect and TLS checks succeed.
        let response = JsFuture::from(fetch(&request).map_err(|_| RpcError::BrowserTransport)?)
            .await
            .map_err(|_| RpcError::BrowserTransport)?
            .dyn_into::<web_sys::Response>()
            .map_err(|_| RpcError::BrowserTransport)?;
        if !response.ok() {
            return Err(RpcError::BrowserTransport);
        }
        // Only a successful response is decoded into JSON-RPC bytes.
        let buffer = JsFuture::from(
            response
                .array_buffer()
                .map_err(|_| RpcError::BrowserTransport)?,
        )
        .await
        .map_err(|_| RpcError::BrowserTransport)?;
        Ok(js_sys::Uint8Array::new(&buffer).to_vec())
    }

    /// Build the browser request whose redirect mode prevents any second-hop disclosure.
    fn credential_request(endpoint: &str, token: &str, body: &str) -> Result<web_sys::Request> {
        // The browser enforces TLS certificate validation and rejects any redirect.
        let options = web_sys::RequestInit::new();
        options.set_method("POST");
        options.set_redirect(web_sys::RequestRedirect::Error);
        options.set_body(&JsValue::from_str(body));
        // Invalid URLs or header values fail before invoking Fetch.
        let request = web_sys::Request::new_with_str_and_init(endpoint, &options)
            .map_err(|_| RpcError::BrowserTransport)?;
        // These are the sole credential-bearing headers controlled by this adapter.
        let headers = request.headers();
        headers
            .set("Content-Type", "application/json")
            .map_err(|_| RpcError::BrowserTransport)?;
        headers
            .set("Accept", "application/json")
            .map_err(|_| RpcError::BrowserTransport)?;
        headers
            .set("Authorization", &format!("Bearer {token}"))
            .map_err(|_| RpcError::BrowserTransport)?;
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
            let request = credential_request("https://example.com/rpc", "synthetic-token", "{}")?;
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
