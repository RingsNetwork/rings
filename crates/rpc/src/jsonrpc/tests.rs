//! Credential destination laws and native transport regressions.

use super::transport::authenticated_endpoint;
use super::RpcError;

/// The exception contains parsed loopback addresses only on direct transports; hostnames,
/// userinfo, and proxied plain HTTP cannot widen it.
#[cfg_attr(not(target_family = "wasm"), test)]
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
fn authenticated_endpoint_policy() {
    for endpoint in [
        "https://example.com/rpc",
        "HTTPS://example.com:443/rpc",
        "http://127.0.0.1:50000",
        "http://127.255.255.254/rpc",
        "http://[::1]:50000",
    ] {
        assert!(
            authenticated_endpoint(endpoint, false).is_ok(),
            "{endpoint}"
        );
    }
    for endpoint in ["https://example.com/rpc", "HTTPS://example.com:443/rpc"] {
        assert!(authenticated_endpoint(endpoint, true).is_ok(), "{endpoint}");
    }
    for endpoint in [
        "http://127.0.0.1:50000",
        "http://127.255.255.254/rpc",
        "http://[::1]:50000",
    ] {
        assert!(
            matches!(
                authenticated_endpoint(endpoint, true),
                Err(RpcError::InsecureAuthenticatedEndpoint)
            ),
            "{endpoint}"
        );
    }
    for endpoint in [
        "http://example.com",
        "http://localhost",
        "http://localhost.example.com",
        "http://127.0.0.1.example.com",
        "http://127.0.0.1@evil.example",
        "http://192.168.1.1",
        "http://0.0.0.0",
        "http://[::]",
        "http://[::ffff:127.0.0.1]",
        "http://[2001:db8::1]",
        "ftp://127.0.0.1",
        "https://user:password@example.com",
        "https://example.com/#fragment",
        "not a URL",
    ] {
        for proxied in [false, true] {
            assert!(
                authenticated_endpoint(endpoint, proxied).is_err(),
                "{endpoint}"
            );
        }
    }
}

/// Error messages do not repeat URL credentials or query parameters.
#[cfg_attr(not(target_family = "wasm"), test)]
#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
fn endpoint_errors_do_not_disclose_input() {
    for proxied in [false, true] {
        let result = authenticated_endpoint(
            "http://secret:password@remote.example/?token=private",
            proxied,
        );
        assert!(matches!(
            result,
            Err(RpcError::InvalidAuthenticatedEndpoint)
        ));
    }
}

/// Native listeners witness actual requests, rather than just constructor behavior.
#[cfg(not(target_family = "wasm"))]
mod native;
