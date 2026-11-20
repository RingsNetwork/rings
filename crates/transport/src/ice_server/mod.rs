//! This module contains the IceServer structure.

use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;
use url::Url;

use crate::error::IceServerError;
use crate::sdp::IceCandidateTier;

/// WebRTC IceCredentialType enums.
#[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub enum IceCredentialType {
    /// IceCredentialType::Password describes username and password based
    /// credentials as described in <https://tools.ietf.org/html/rfc5389>.
    #[default]
    Password,
}

/// This structure is used to validate whether the parameter in String format is valid
/// and convert it to the format required by the underlying library.
///
/// In order to create Connection correctly, each Connection needs to implement the conversion
/// from IceServer to its underlying library parameters.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct IceServer {
    /// Urls is an array of URIs that can be used as STUN and TURN servers.
    pub urls: Vec<String>,
    /// The username to use if the server requires authorization.
    pub username: String,
    /// The secret to use for authentication with the
    pub credential: String,
    /// CredentialType indicates which type of credential the ICEAgent will use.
    pub credential_type: IceCredentialType,
}

impl IceServer {
    /// Convert String to `Vec<IceServer>`. Will split the string by `;` and parse each part.
    pub fn vec_from_str(s: &str) -> Result<Vec<Self>, IceServerError> {
        s.split(';').map(IceServer::from_str).collect()
    }

    /// The highest [`IceCandidateTier`] that a peer connection configured with `servers` can
    /// gather (`Host` when empty, `Reflexive` when only `stun:` URLs are present, `Relay` when any
    /// `turn:` or `turns:` URL is present).
    pub fn target_candidate_tier(servers: &[Self]) -> IceCandidateTier {
        servers
            .iter()
            .flat_map(|server| server.urls.iter())
            .filter_map(|url| {
                let (scheme, _) = url.trim().split_once(':')?;
                if scheme.eq_ignore_ascii_case("turn") || scheme.eq_ignore_ascii_case("turns") {
                    Some(IceCandidateTier::Relay)
                } else if scheme.eq_ignore_ascii_case("stun") {
                    Some(IceCandidateTier::Reflexive)
                } else {
                    None
                }
            })
            .max()
            .unwrap_or(IceCandidateTier::Host)
    }
}

/// Parse ICE servers, log invalid configuration, and continue without servers.
#[cfg(any(
    all(feature = "dummy", not(target_family = "wasm")),
    all(feature = "native-webrtc", not(target_family = "wasm")),
    all(feature = "web-sys-webrtc", target_family = "wasm"),
))]
pub(crate) fn parse_ice_servers_or_warn(config: &str, transport: &str) -> Vec<IceServer> {
    let config = config.trim();
    if config.is_empty() {
        return Vec::new();
    }

    match IceServer::vec_from_str(config) {
        Ok(ice_servers) => ice_servers,
        Err(error) => {
            tracing::warn!(%error, %transport, "Ignoring invalid ICE server configuration");
            Vec::new()
        }
    }
}

impl Default for IceServer {
    fn default() -> Self {
        Self {
            urls: ["stun://stun.l.google.com:19302".to_string()].to_vec(),
            username: String::default(),
            credential: String::default(),
            credential_type: IceCredentialType::default(),
        }
    }
}

/// [stun|turn|turns]://[username]:[password]@[url][?transport=tcp|udp]
/// For current implementation all type is `password` as default
/// E.g: stun://foo:bar@stun.l.google.com:19302
///      turn://ethereum.org:9090
///      turn://ryan@ethereum.org:9090/nginx/v2
///      turn://user:pass@ethereum.org:80?transport=tcp
///      turns://user:pass@ethereum.org:5349?transport=tcp
impl FromStr for IceServer {
    type Err = IceServerError;
    fn from_str(s: &str) -> Result<Self, IceServerError> {
        let trimmed = s.trim();
        let normalized;
        let to_parse = if !trimmed.contains("://") {
            if let Some((scheme_part, rest)) = trimmed.split_once(':') {
                if !rest.is_empty() && !rest.starts_with('/') {
                    normalized = format!("{scheme_part}://{rest}");
                    normalized.as_str()
                } else {
                    trimmed
                }
            } else {
                trimmed
            }
        } else {
            trimmed
        };
        let parsed = Url::parse(to_parse)?;
        let scheme = parsed.scheme();
        if !(["turn", "turns", "stun"].contains(&scheme)) {
            return Err(IceServerError::SchemeNotSupported(scheme.into()));
        }
        if !parsed.has_host() {
            return Err(IceServerError::UrlMissHost);
        }
        let username = parsed.username();
        let password = parsed.password().unwrap_or("");
        let host = parsed.host_str().ok_or(IceServerError::UrlMissHost)?;
        // parse port as `:<port>`
        let port = parsed
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_else(|| "".to_string());
        let path = parsed.path();
        let query = normalize_ice_query(parsed.query());
        let url = format!("{scheme}:{host}{port}{path}{query}");
        Ok(Self {
            urls: vec![url],
            username: username.to_string(),
            credential: password.to_string(),
            credential_type: IceCredentialType::default(),
        })
    }
}

fn normalize_ice_query(raw_query: Option<&str>) -> String {
    let Some(raw_query) = raw_query else {
        return String::new();
    };
    let mut params = Vec::new();
    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some((key, value)) = pair.split_once('=') {
            if key.eq_ignore_ascii_case("pubkey") {
                continue;
            }
            if key.eq_ignore_ascii_case("transport") {
                if value.eq_ignore_ascii_case("tcp") {
                    params.push("transport=tcp".to_string());
                } else if value.eq_ignore_ascii_case("udp") {
                    params.push("transport=udp".to_string());
                } else {
                    params.push(format!("transport={value}"));
                }
            } else {
                params.push(pair.to_string());
            }
        } else {
            params.push(pair.to_string());
        }
    }
    if params.is_empty() {
        String::new()
    } else {
        format!("?{}", params.join("&"))
    }
}

#[cfg(test)]
mod test_ice_server;
