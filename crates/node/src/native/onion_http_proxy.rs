#![warn(missing_docs)]
//! Native HTTP CONNECT ingress for onion proxy clients.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use crate::error::Error;
use crate::error::Result;
use crate::onion::tcp::NativeOnionCircuitHandle;
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
    onion: NativeOnionCircuitHandle,
) -> Result<()> {
    let listener = TcpListener::bind(options.listen_addr)
        .await
        .map_err(|error| Error::OnionProxyIoError(format!("bind HTTP proxy listener: {error}")))?;
    let listen_addr = listener.local_addr().map_err(|error| {
        Error::OnionProxyIoError(format!("read HTTP proxy listener address: {error}"))
    })?;
    println!("Onion HTTP CONNECT proxy endpoint: http://{listen_addr}");

    loop {
        let (stream, peer_addr) = listener.accept().await.map_err(|error| {
            Error::OnionProxyIoError(format!("accept HTTP proxy connection: {error}"))
        })?;
        let processor = processor.clone();
        let onion = onion.clone();
        let options = options.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_connect(stream, processor, onion, options).await {
                tracing::warn!("onion HTTP proxy request from {peer_addr} failed: {error:?}");
            }
        });
    }
}

async fn handle_connect(
    mut stream: TcpStream,
    processor: Arc<Processor>,
    onion: NativeOnionCircuitHandle,
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
    let opened = onion
        .open_tcp_stream(proxy_route.route, proxy_route.target)
        .await?;
    write_proxy_response(&mut stream, "200 Connection Established").await?;
    opened.relay(stream);
    Ok(())
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
}
