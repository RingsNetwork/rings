#![warn(missing_docs)]
//! Native HTTP CONNECT ingress for onion proxy clients.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::lookup_host;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use crate::error::Error;
use crate::error::Result;
use crate::extension::protocols::relay::RelayHandle;
use crate::onion::OnionExitPolicy;
use crate::onion_proxy::OnionProxyConfig;
use crate::onion_proxy::OnionProxyTarget;
use crate::processor::Processor;

const MAX_CONNECT_HEADER_BYTES: usize = 8192;

/// Runtime options for the native onion HTTP CONNECT proxy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionHttpProxyOptions {
    /// Local bind address.
    pub listen_addr: SocketAddr,
    /// Desired hop count including the exit. `0` uses node default.
    pub hop_count: usize,
    /// Whether route selection may use fewer hops when too few relays are live.
    pub allow_short_paths: bool,
}

/// Run a native HTTP CONNECT proxy for onion TCP exits.
pub async fn run_onion_http_proxy(
    options: OnionHttpProxyOptions,
    processor: Arc<Processor>,
    relay: RelayHandle,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(options.listen_addr).await?;
    let listen_addr = listener.local_addr()?;
    println!("Onion HTTP CONNECT proxy endpoint: http://{listen_addr}");

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let processor = processor.clone();
        let relay = relay.clone();
        let options = options.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connect(stream, processor, relay, options).await {
                tracing::warn!("onion HTTP proxy request from {peer_addr} failed: {error:?}");
            }
        });
    }
}

/// Register exact allow-list targets as native TCP relay services for this exit.
pub async fn register_native_onion_exit_targets(
    relay: &RelayHandle,
    policy: &OnionExitPolicy,
) -> Result<()> {
    for authority in native_onion_exit_target_authorities(policy)? {
        let addr = resolve_target(&authority).await?;
        relay
            .register_tcp_service(authority, addr)
            .await
            .map_err(|error| {
                Error::InvalidConfig(format!("register onion exit target: {error}"))
            })?;
    }
    Ok(())
}

fn native_onion_exit_target_authorities(policy: &OnionExitPolicy) -> Result<Vec<String>> {
    policy
        .allowed_targets
        .iter()
        .filter(|target| policy.allows_target(target))
        .map(|target| OnionProxyTarget::parse_authority(target).map(|target| target.authority()))
        .collect()
}

async fn handle_connect(
    mut stream: TcpStream,
    processor: Arc<Processor>,
    relay: RelayHandle,
    options: OnionHttpProxyOptions,
) -> Result<()> {
    let target = match read_connect_target(&mut stream).await {
        Ok(target) => target,
        Err(error) => {
            let _ = write_proxy_response(&mut stream, "400 Bad Request").await;
            return Err(error);
        }
    };
    let proxy_route = processor
        .build_onion_proxy_route(
            OnionProxyConfig::tcp_connect(options.hop_count, options.allow_short_paths),
            target,
        )
        .await?;
    let service = proxy_route.target.authority();
    let exit = proxy_route.exit_did();

    write_proxy_response(&mut stream, "200 Connection Established").await?;
    relay.relay_tcp_stream(stream, exit, service).await
}

async fn read_connect_target(stream: &mut TcpStream) -> Result<OnionProxyTarget> {
    let header = read_http_header(stream).await?;
    let header = std::str::from_utf8(&header)
        .map_err(|_| Error::HttpRequestError("HTTP CONNECT header is not UTF-8".to_string()))?;
    let request_line = header
        .lines()
        .next()
        .ok_or_else(|| Error::HttpRequestError("missing HTTP request line".to_string()))?;
    parse_connect_request_line(request_line)
}

async fn read_http_header(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    while header.len() < MAX_CONNECT_HEADER_BYTES {
        let n = stream.read(byte.as_mut_slice()).await.map_err(|error| {
            Error::HttpRequestError(format!("read HTTP CONNECT header: {error}"))
        })?;
        if n == 0 {
            return Err(Error::HttpRequestError(
                "connection closed before HTTP CONNECT header completed".to_string(),
            ));
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            return Ok(header);
        }
    }
    Err(Error::HttpRequestError(format!(
        "HTTP CONNECT header exceeded {MAX_CONNECT_HEADER_BYTES} bytes"
    )))
}

fn parse_connect_request_line(request_line: &str) -> Result<OnionProxyTarget> {
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| Error::HttpRequestError("missing HTTP method".to_string()))?;
    let authority = parts
        .next()
        .ok_or_else(|| Error::HttpRequestError("missing HTTP CONNECT target".to_string()))?;
    let version = parts
        .next()
        .ok_or_else(|| Error::HttpRequestError("missing HTTP version".to_string()))?;

    if parts.next().is_some() {
        return Err(Error::HttpRequestError(format!(
            "invalid HTTP CONNECT request line {request_line:?}"
        )));
    }
    if method != "CONNECT" {
        return Err(Error::HttpRequestError(format!(
            "unsupported onion proxy method {method:?}; expected CONNECT"
        )));
    }
    if !version.starts_with("HTTP/") {
        return Err(Error::HttpRequestError(format!(
            "invalid HTTP version {version:?}"
        )));
    }

    OnionProxyTarget::parse_authority(authority)
}

async fn write_proxy_response(stream: &mut TcpStream, status: &str) -> Result<()> {
    let response = format!("HTTP/1.1 {status}\r\n\r\n");
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| Error::HttpRequestError(format!("write HTTP proxy response: {error}")))
}

async fn resolve_target(authority: &str) -> Result<SocketAddr> {
    lookup_host(authority)
        .await
        .map_err(|error| {
            Error::InvalidConfig(format!("resolve onion exit target {authority:?}: {error}"))
        })?
        .next()
        .ok_or_else(|| {
            Error::InvalidConfig(format!("onion exit target {authority:?} resolved empty"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_request_line_parses_target() -> Result<()> {
        let target = parse_connect_request_line("CONNECT Example.COM:443 HTTP/1.1")?;

        assert_eq!(target.authority(), "example.com:443");
        Ok(())
    }

    #[test]
    fn connect_request_line_rejects_plain_http_request() {
        assert!(matches!(
            parse_connect_request_line("GET http://example.com/ HTTP/1.1"),
            Err(Error::HttpRequestError(_))
        ));
    }

    #[test]
    fn native_exit_target_registration_honors_deny_list() -> Result<()> {
        let policy = OnionExitPolicy {
            allowed_targets: vec![
                "Example.COM:443".to_string(),
                "blocked.example.com:443".to_string(),
            ],
            denied_targets: vec!["blocked.example.com:443".to_string()],
            max_circuits: 0,
            max_streams_per_circuit: 0,
            max_bytes_per_minute: 0,
        };

        assert_eq!(native_onion_exit_target_authorities(&policy)?, vec![
            "example.com:443".to_string()
        ]);
        Ok(())
    }
}
