#![warn(missing_docs)]
//! Native HTTP CONNECT ingress for onion proxy clients.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use super::OnionProxyConfig;
use super::OnionProxyTarget;
use crate::error::Error;
use crate::error::Result;
use crate::onion::tcp::NativeOnionCircuitHandle;
use crate::onion::OnionServiceName;
use crate::processor::Processor;

const MAX_CONNECT_HEADER_BYTES: usize = 8192;
/// Default deadline for receiving a complete CONNECT header.
pub const DEFAULT_CONNECT_HEADER_TIMEOUT_SECS: u64 = 10;
/// Default concurrent connection bound for the native CONNECT ingress.
pub const DEFAULT_MAX_CONNECT_CONNECTIONS: usize = 1024;

/// Return the default CONNECT header deadline in seconds.
pub const fn default_connect_header_timeout_secs() -> u64 {
    DEFAULT_CONNECT_HEADER_TIMEOUT_SECS
}

/// Return the default native CONNECT ingress concurrency bound.
pub const fn default_max_connect_connections() -> usize {
    DEFAULT_MAX_CONNECT_CONNECTIONS
}

/// Runtime options for the native onion HTTP CONNECT proxy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionHttpProxyOptions {
    /// Local bind address.
    pub listen_addr: SocketAddr,
    /// TCP onion-exit service used for local CONNECT requests.
    pub service: OnionServiceName,
    /// Desired hop count including the exit. `0` uses node default.
    pub hop_count: usize,
    /// Whether route selection may use fewer hops when too few relays are live.
    pub allow_short_paths: bool,
    /// Maximum concurrent local CONNECT requests accepted by this ingress.
    pub max_connections: usize,
    /// Deadline for receiving a complete CONNECT header.
    pub header_timeout: Duration,
}

impl OnionHttpProxyOptions {
    /// Build options with production defaults for resource bounds.
    pub fn new(
        listen_addr: SocketAddr,
        service: OnionServiceName,
        hop_count: usize,
        allow_short_paths: bool,
    ) -> Self {
        Self {
            listen_addr,
            service,
            hop_count,
            allow_short_paths,
            max_connections: DEFAULT_MAX_CONNECT_CONNECTIONS,
            header_timeout: Duration::from_secs(DEFAULT_CONNECT_HEADER_TIMEOUT_SECS),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.max_connections == 0 {
            return Err(Error::InvalidConfig(
                "onion_http_proxy_max_connections must be greater than zero".to_string(),
            ));
        }
        if self.header_timeout.is_zero() {
            return Err(Error::InvalidConfig(
                "onion_http_proxy_header_timeout_secs must be greater than zero".to_string(),
            ));
        }
        self.proxy_config().map(|_| ())
    }

    fn proxy_config(&self) -> Result<OnionProxyConfig> {
        OnionProxyConfig::tcp_connect_service(
            self.service.clone(),
            self.hop_count,
            self.allow_short_paths,
        )
    }
}

/// Run a native HTTP CONNECT proxy for onion TCP exits.
pub async fn run_onion_http_proxy(
    options: OnionHttpProxyOptions,
    processor: Arc<Processor>,
    onion: NativeOnionCircuitHandle,
) -> Result<()> {
    options.validate()?;
    let listener = TcpListener::bind(options.listen_addr)
        .await
        .map_err(|error| Error::OnionProxyIoError(format!("bind HTTP proxy listener: {error}")))?;
    let listen_addr = listener.local_addr().map_err(|error| {
        Error::OnionProxyIoError(format!("read HTTP proxy listener address: {error}"))
    })?;
    println!("Onion HTTP CONNECT proxy endpoint: http://{listen_addr}");
    let permits = Arc::new(Semaphore::new(options.max_connections));

    loop {
        let permit = permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Lock)?;
        let (stream, peer_addr) = listener.accept().await.map_err(|error| {
            Error::OnionProxyIoError(format!("accept HTTP proxy connection: {error}"))
        })?;
        let processor = processor.clone();
        let onion = onion.clone();
        let options = options.clone();
        tokio::spawn(async move {
            let _permit = permit;
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
    let target = match read_connect_target(&mut stream, options.header_timeout).await {
        Ok(target) => target,
        Err(error) => {
            let _ = write_proxy_response(&mut stream, "400 Bad Request").await;
            return Err(error);
        }
    };
    let proxy_route = processor
        .build_onion_proxy_route(options.proxy_config()?, target)
        .await?;
    let opened = onion
        .open_tcp_stream(proxy_route.route, proxy_route.target)
        .await?;
    write_proxy_response(&mut stream, "200 Connection Established").await?;
    opened.relay(stream);
    Ok(())
}

async fn read_connect_target(
    stream: &mut TcpStream,
    header_timeout: Duration,
) -> Result<OnionProxyTarget> {
    let header = timeout(header_timeout, read_http_header(stream))
        .await
        .map_err(|_| {
            Error::HttpRequestError(format!(
                "HTTP CONNECT header timed out after {} ms",
                header_timeout.as_millis()
            ))
        })??;
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
mod tests;

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn options() -> OnionHttpProxyOptions {
        OnionHttpProxyOptions::new(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            OnionServiceName::tcp(),
            0,
            false,
        )
    }

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
    fn proxy_options_build_custom_tcp_service_config() -> Result<()> {
        let mut options = options();
        options.service = OnionServiceName::parse("web")?;

        let proxy = options.proxy_config()?;

        assert_eq!(proxy.exit_service(), "web");
        Ok(())
    }

    #[test]
    fn proxy_options_reject_unbounded_connection_model() {
        let mut zero_connections = options();
        zero_connections.max_connections = 0;
        assert!(matches!(
            zero_connections.validate(),
            Err(Error::InvalidConfig(_))
        ));

        let mut zero_timeout = options();
        zero_timeout.header_timeout = Duration::ZERO;
        assert!(matches!(
            zero_timeout.validate(),
            Err(Error::InvalidConfig(_))
        ));
    }
}
