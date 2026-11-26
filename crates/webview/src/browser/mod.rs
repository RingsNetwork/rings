//! Browser-facing webview helpers.

use serde::Serialize;
use url::Url;

#[cfg(test)]
use crate::error::Result;
#[cfg(test)]
use crate::url::GatewayPrefix;

/// JavaScript bootstrap template marker used by tests and consumers.
pub const BOOTSTRAP_MARKER: &str = "__ringsWebviewGateway";

/// Bundled browser runtime shim injected into controlled pages.
const BROWSER_RUNTIME: &str = include_str!("../browser_runtime.mjs");
/// Bundled URL and attribute transform helpers shared with the browser runtime.
const BROWSER_TRANSFORMS: &str = include_str!("../browser_runtime_transforms.mjs");

/// Typed configuration consumed by the browser runtime asset.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserBootstrapConfig<'a> {
    /// Gateway route prefix used to rewrite runtime URLs.
    prefix: &'a str,
    /// Target document base URL used to resolve relative references.
    target_base: &'a str,
    /// Global marker property set once the bootstrap has initialized.
    marker: &'a str,
    /// Whether `Worker` and `SharedWorker` constructors should be blocked outright.
    block_workers: bool,
    /// Whether top-level navigation should be delegated to an extension host bridge.
    delegate_navigation: bool,
}

/// Host environment governing worker blocking and navigation delegation in the bootstrap script.
#[derive(Clone, Copy)]
enum BrowserHostPolicy {
    /// Standard web/sandbox host where workers are blocked.
    Web,
    /// Browser extension host providing fail-closed navigation and worker bridges.
    Extension,
}

/// Build a runtime that rewrites browser-created URLs through the gateway prefix.
///
/// DOM URL rewriting remains useful in opaque-origin sandboxed documents. Dynamic `fetch` and
/// `XMLHttpRequest` routing additionally requires a trusted host bridge; an opaque document is
/// not controlled by the Service Worker that served its gateway URL. See the crate-level
/// opaque-origin deployment boundary.
pub fn bootstrap_script(gateway_prefix: &str, document_url: &Url) -> String {
    bootstrap_script_with_policy(gateway_prefix, document_url, BrowserHostPolicy::Web)
}

/// Build a browser runtime that delegates navigation and Worker effects to an extension host.
///
/// The caller must install fail-closed navigation, `Worker`, and `SharedWorker` adapters before
/// executing the returned script. All other unsupported browser-global network protocols remain
/// blocked.
pub fn bootstrap_script_with_extension_bridge(gateway_prefix: &str, document_url: &Url) -> String {
    bootstrap_script_with_policy(gateway_prefix, document_url, BrowserHostPolicy::Extension)
}

/// Assemble the full bootstrap script for `gateway_prefix`, `document_url`, and `host_policy`.
fn bootstrap_script_with_policy(
    gateway_prefix: &str,
    document_url: &Url,
    host_policy: BrowserHostPolicy,
) -> String {
    let config = BrowserBootstrapConfig {
        prefix: gateway_prefix,
        target_base: document_url.as_str(),
        marker: BOOTSTRAP_MARKER,
        block_workers: matches!(host_policy, BrowserHostPolicy::Web),
        delegate_navigation: matches!(host_policy, BrowserHostPolicy::Extension),
    };
    let config = match serde_json::to_string(&config) {
        Ok(config) => config,
        Err(_) => return String::new(),
    };

    let mut script = String::from("globalThis.__ringsWebviewBootstrapConfig=");
    script.push_str(&config);
    script.push_str(";\n");
    script.push_str(BROWSER_TRANSFORMS);
    script.push('\n');
    script.push_str(BROWSER_RUNTIME);
    script
}

/// Resolve a runtime URL the same way the bootstrap does and encode it for the gateway.
#[cfg(test)]
pub fn runtime_gateway_url(
    gateway_prefix: &GatewayPrefix,
    document_url: &Url,
    input: &str,
) -> Result<Option<String>> {
    gateway_prefix.rewrite_url_value(document_url, input)
}

#[cfg(test)]
mod test_browser;
