use std::net::IpAddr;
use std::net::SocketAddr;
use std::time::Duration;

use super::limits::reject_content_length_over_limit;
use super::limits::usize_to_u64;
use super::normalize_method;
use super::FetchResponse;
use super::OnionHttpsRequest;
use super::OnionHttpsRuntime;
use crate::error::Error;
use crate::error::Result;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::target::resolve_target_addresses;
use crate::onion::target::select_public_exit_addresses;
use crate::onion::target::PublicAddressSelection;
use crate::onion::OnionExitPolicy;

const HTTPS_EXIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Mutually exclusive native HTTPS egress strategies.
///
/// A direct request is allowed only after resolving and pinning public addresses. A proxied
/// request deliberately delegates name resolution to an operator-configured upstream proxy; the
/// explicit proxy object prevents reqwest from silently applying `NO_PROXY` and bypassing that
/// trust boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NativeHttpsEgress {
    Direct {
        host: String,
        addresses: Vec<SocketAddr>,
    },
    Proxy(String),
}

impl NativeHttpsEgress {
    fn configure(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> std::result::Result<reqwest::ClientBuilder, reqwest::Error> {
        match self {
            Self::Direct { host, addresses } => {
                Ok(builder.no_proxy().resolve_to_addrs(host, addresses))
            }
            Self::Proxy(proxy) => reqwest::Proxy::all(proxy).map(|proxy| builder.proxy(proxy)),
        }
    }
}

pub(super) async fn execute_https_request(
    url: &str,
    target: &OnionProxyTarget,
    request: &OnionHttpsRequest,
    max_body_bytes: u64,
    runtime: &OnionHttpsRuntime,
    policy: &OnionExitPolicy,
) -> Result<FetchResponse> {
    let addresses = resolve_target_addresses(target).await?;
    let egress = select_native_https_egress(target, addresses, runtime.native_proxy())?;
    native_fetch_with_timeout(
        url,
        request,
        max_body_bytes,
        HTTPS_EXIT_REQUEST_TIMEOUT,
        &egress,
        |bytes| runtime.record_exit_bytes(policy, bytes),
    )
    .await
}

/// Select native HTTPS egress from an immutable resolution result and proxy configuration.
///
/// Post: public resolution always selects a pinned direct path; only a hostname whose complete DNS
/// snapshot is in the proxy fake-IP range may delegate resolution to an operator-configured proxy;
/// every other non-public result remains denied.
pub(super) fn select_native_https_egress(
    target: &OnionProxyTarget,
    addresses: Vec<SocketAddr>,
    configured_proxy: Option<String>,
) -> Result<NativeHttpsEgress> {
    let proxy_synthetic_resolution = target.host().parse::<IpAddr>().is_err()
        && !addresses.is_empty()
        && addresses
            .iter()
            .all(|address| is_native_proxy_synthetic_ip(address.ip()));
    match select_public_exit_addresses(addresses) {
        PublicAddressSelection::Public(addresses) => Ok(NativeHttpsEgress::Direct {
            host: target.host().to_string(),
            addresses,
        }),
        PublicAddressSelection::Denied if proxy_synthetic_resolution => configured_proxy
            .map(NativeHttpsEgress::Proxy)
            .ok_or(Error::NoPermission),
        PublicAddressSelection::Denied => Err(Error::NoPermission),
        PublicAddressSelection::Empty => Err(Error::OnionTargetResolvedEmpty {
            authority: target.authority(),
        }),
    }
}

/// Return whether local DNS produced the narrow IPv4 range commonly reserved for proxy fake-IP
/// synthesis. No other non-public address is eligible for proxy-side resolution.
const fn is_native_proxy_synthetic_ip(address: IpAddr) -> bool {
    matches!(address, IpAddr::V4(address) if matches!(address.octets(), [198, 18..=19, _, _]))
}

fn native_http_error(context: &str, error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::HttpRequestError(format!("{context}: timed out"))
    } else {
        Error::HttpRequestError(format!("{context}: {error}"))
    }
}

pub(super) async fn native_fetch_with_timeout(
    url: &str,
    request: &OnionHttpsRequest,
    max_body_bytes: u64,
    timeout: Duration,
    egress: &NativeHttpsEgress,
    record_bytes: impl Fn(u64) -> Result<()>,
) -> Result<FetchResponse> {
    let method = reqwest::Method::from_bytes(normalize_method(&request.method).as_bytes())
        .map_err(|error| Error::HttpRequestError(format!("invalid HTTPS proxy method: {error}")))?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout);
    let client = egress
        .configure(client)
        .map_err(|error| native_http_error("configure HTTPS proxy", error))?
        .build()
        .map_err(|error| Error::HttpRequestError(format!("build HTTPS proxy client: {error}")))?;
    let mut builder = client.request(method, url);
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    if !request.body.is_empty() {
        builder = builder.body(request.body.clone());
    }
    let mut response = builder
        .send()
        .await
        .map_err(|error| native_http_error("native HTTPS proxy request", error))?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect::<Vec<_>>();
    reject_content_length_over_limit(&headers, max_body_bytes)?;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| native_http_error("read HTTPS proxy response", error))?
    {
        let body_len = usize_to_u64(body.len())?;
        let chunk_len = usize_to_u64(chunk.len())?;
        if max_body_bytes > 0 && body_len.saturating_add(chunk_len) > max_body_bytes {
            return Err(Error::NoPermission);
        }
        record_bytes(chunk_len)?;
        body.extend_from_slice(chunk.as_ref());
    }
    Ok(FetchResponse {
        status,
        headers,
        body,
    })
}
