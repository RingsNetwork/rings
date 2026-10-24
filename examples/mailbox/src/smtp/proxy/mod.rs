//! The PROXY protocol v2 header a Rings DNS relay sends before a client's bytes, on a port
//! served with `proxy_protocol: v2`: it names the client, which the receiver otherwise sees as
//! its own node.
//!
//! ```text
//!   16 bytes: signature (12), version and command, family and protocol, length L (big endian)
//!   L bytes:  the addresses, then TLVs
//!
//!   PROXY over TCP ─▶ Some(source)          LOCAL, or another family ─▶ None (the TCP peer)
//! ```
//!
//! The header is unauthenticated: only a relay can be trusted to send it, so a receiver reads
//! it only on a port that only its node reaches.

use std::net::SocketAddr;
use std::time::Duration;

use ppp::v2::Addresses;
use ppp::v2::Command;
use ppp::v2::Header;
use ppp::v2::ParseError;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

/// Bytes of the fixed part of a v2 header.
const FIXED_LEN: usize = 16;
/// The signature every v2 header starts with.
const SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";
/// How long the header may take to arrive.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Why no header was read.
#[derive(Debug, Error)]
pub enum ProxyError {
    /// The stream failed or ended first.
    #[error("PROXY header: {0}")]
    Io(#[from] std::io::Error),
    /// The header did not arrive in time.
    #[error("PROXY header: none within {HEADER_TIMEOUT:?}")]
    Timeout,
    /// The bytes are not a v2 header.
    #[error("PROXY header: {0}")]
    Malformed(#[from] ParseError),
}

/// Read the header at the start of `stream`; return the client it names, or `None` when it
/// names none (a `LOCAL` header, or an address family other than IP).
pub async fn read_client<S>(stream: &mut S) -> Result<Option<SocketAddr>, ProxyError>
where S: AsyncRead + Unpin {
    timeout(HEADER_TIMEOUT, read_header(stream))
        .await
        .map_err(|_elapsed| ProxyError::Timeout)?
}

/// [`read_client`] without its time bound.
async fn read_header<S>(stream: &mut S) -> Result<Option<SocketAddr>, ProxyError>
where S: AsyncRead + Unpin {
    let mut bytes = vec![0u8; FIXED_LEN];
    stream.read_exact(&mut bytes).await?;
    // Checked first, so bytes that are no header are refused before their "length" is waited for.
    if !bytes.starts_with(SIGNATURE) {
        return Err(ProxyError::Malformed(ParseError::Prefix));
    }
    let length = match bytes.get(14..FIXED_LEN) {
        Some([high, low]) => usize::from(u16::from_be_bytes([*high, *low])),
        _ => 0,
    };
    bytes.resize(FIXED_LEN.saturating_add(length), 0);
    stream
        .read_exact(bytes.get_mut(FIXED_LEN..).unwrap_or_default())
        .await?;
    Ok(client_of(&Header::try_from(bytes.as_slice())?))
}

/// The client `header` names (the module doc).
pub fn client_of(header: &Header<'_>) -> Option<SocketAddr> {
    if header.command == Command::Local {
        return None;
    }
    match &header.addresses {
        Addresses::IPv4(ip) => Some(SocketAddr::from((ip.source_address, ip.source_port))),
        Addresses::IPv6(ip) => Some(SocketAddr::from((ip.source_address, ip.source_port))),
        Addresses::Unspecified | Addresses::Unix(_) => None,
    }
}

#[cfg(test)]
mod test_proxy;
