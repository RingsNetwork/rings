//! Target authority parsing shared by onion policy and proxy adapters.

use crate::error::Error;
use crate::error::Result;

/// Host/port target requested through an onion proxy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionProxyTarget {
    host: String,
    port: u16,
}

impl OnionProxyTarget {
    /// Parse an HTTP CONNECT authority (`host:port` or `[ipv6]:port`).
    pub fn parse_authority(authority: &str) -> Result<Self> {
        let authority = authority.trim();
        if authority.is_empty() {
            return Err(Error::HttpRequestError(
                "onion proxy target authority must not be empty".to_string(),
            ));
        }

        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let Some((host, rest)) = rest.split_once(']') else {
                return Err(Error::HttpRequestError(format!(
                    "invalid IPv6 onion proxy authority {authority:?}"
                )));
            };
            let Some(port) = rest.strip_prefix(':') else {
                return Err(Error::HttpRequestError(format!(
                    "onion proxy authority {authority:?} must include a port"
                )));
            };
            (host, port)
        } else {
            authority.rsplit_once(':').ok_or_else(|| {
                Error::HttpRequestError(format!(
                    "onion proxy authority {authority:?} must be host:port"
                ))
            })?
        };

        let host = normalize_host(host)?;
        let port = port.parse::<u16>().map_err(|_| {
            Error::HttpRequestError(format!(
                "onion proxy authority {authority:?} has an invalid port"
            ))
        })?;
        if port == 0 {
            return Err(Error::HttpRequestError(
                "onion proxy target port must be non-zero".to_string(),
            ));
        }

        Ok(Self { host, port })
    }

    /// Return the normalized host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Return the target port.
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Return the canonical authority string used for exit policy/service lookup.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn normalize_host(host: &str) -> Result<String> {
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() {
        return Err(Error::HttpRequestError(
            "onion proxy target host must not be empty".to_string(),
        ));
    }
    if host.chars().any(char::is_whitespace) {
        return Err(Error::HttpRequestError(format!(
            "onion proxy target host {host:?} must not contain whitespace"
        )));
    }
    Ok(host.to_ascii_lowercase())
}
