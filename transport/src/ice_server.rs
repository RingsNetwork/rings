//! This module contains the IceServer structure.

use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;
use url::Url;

use crate::error::IceServerError;

/// WebRTC IceCredentialType enums.
#[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub enum IceCredentialType {
    /// IceCredentialType::Password describes username and password based
    /// credentials as described in <https://tools.ietf.org/html/rfc5389>.
    #[default]
    Password,

    /// IceCredentialType::Oauth describes token based credential as described
    /// in <https://tools.ietf.org/html/rfc7635>.
    /// Not supported in WebRTC 1.0 spec
    Oauth,
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
    /// Convert String to Vec<IceServer>. Will split the string by `;` and parse each part.
    pub fn vec_from_str(s: &str) -> Result<Vec<Self>, IceServerError> {
        s.split(';').map(IceServer::from_str).collect()
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

/// [stun|turn]://[username]:[password]@[url]
/// For current implementation all type is `password` as default
/// E.g: stun://foo:bar@stun.l.google.com:19302
///      turn://ethereum.org:9090
///      turn://ryan@ethereum.org:9090/nginx/v2
impl FromStr for IceServer {
    type Err = IceServerError;
    fn from_str(s: &str) -> Result<Self, IceServerError> {
        let parsed = Url::parse(s)?;
        let scheme = parsed.scheme();
        if !(["turn", "stun"].contains(&scheme)) {
            return Err(IceServerError::SchemeNotSupported(scheme.into()));
        }
        if !parsed.has_host() {
            return Err(IceServerError::UrlMissHost);
        }
        let username = parsed.username();
        let password = parsed.password().unwrap_or("");
        // must have host
        let host = parsed.host_str().unwrap();
        // parse port as `:<port>`
        let port = parsed
            .port()
            .map(|p| format!(":{}", p))
            .unwrap_or_else(|| "".to_string());
        let path = parsed.path();
        let url = format!("{}:{}{}{}", scheme, host, port, path);
        Ok(Self {
            urls: vec![url],
            username: username.to_string(),
            credential: password.to_string(),
            credential_type: IceCredentialType::default(),
        })
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use super::IceServer;

    #[test]
    fn test_parsing() {
        let a = "stun://foo:bar@stun.l.google.com:19302";
        let b = "turn://ethereum.org:9090";
        let c = "turn://ryan@ethereum.org:9090/nginx/v2";
        let d = "turn://ryan@ethereum.org/nginx/v2";
        let e = "http://ryan@ethereum.org/nginx/v2";
        let ret_a = IceServer::from_str(a).unwrap();
        let ret_b = IceServer::from_str(b).unwrap();
        let ret_c = IceServer::from_str(c).unwrap();
        let ret_d = IceServer::from_str(d).unwrap();
        let ret_e = IceServer::from_str(e);

        assert_eq!(ret_a.urls[0], "stun:stun.l.google.com:19302".to_string());
        assert_eq!(ret_a.credential, "bar".to_string());
        assert_eq!(ret_a.username, "foo".to_string());

        assert_eq!(ret_b.urls[0], "turn:ethereum.org:9090".to_string());
        assert_eq!(ret_b.credential, "".to_string());
        assert_eq!(ret_b.username, "".to_string());

        assert_eq!(ret_c.urls[0], "turn:ethereum.org:9090/nginx/v2".to_string());
        assert_eq!(ret_c.credential, "".to_string());
        assert_eq!(ret_c.username, "ryan".to_string());

        assert_eq!(ret_d.urls[0], "turn:ethereum.org/nginx/v2".to_string());
        assert_eq!(ret_d.credential, "".to_string());
        assert_eq!(ret_d.username, "ryan".to_string());

        assert!(ret_e.is_err());
    }
}
