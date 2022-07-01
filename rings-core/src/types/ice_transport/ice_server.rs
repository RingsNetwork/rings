use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;
use url::Url;

use crate::err::Error;
use crate::err::Result;

#[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub enum IceCredentialType {
    Unspecified,
    #[default]
    Password,
    Oauth,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
    pub credential_type: IceCredentialType,
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
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let parsed = Url::parse(s)?;
        let scheme = parsed.scheme();
        if !(["turn", "stun"].contains(&scheme)) {
            return Err(Error::IceServerSchemeNotSupport(scheme.into()));
        }
        if !parsed.has_host() {
            return Err(Error::IceServerURLMissHost);
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

#[cfg(feature = "wasm")]
mod wasm {
    use js_sys::Array;
    use wasm_bindgen::JsValue;
    use web_sys::RtcIceCredentialType;
    use web_sys::RtcIceServer;

    use super::IceCredentialType;
    use super::IceServer;

    // set default to password
    impl From<IceCredentialType> for RtcIceCredentialType {
        fn from(s: IceCredentialType) -> Self {
            match s {
                IceCredentialType::Unspecified => Self::Password,
                IceCredentialType::Password => Self::Password,
                IceCredentialType::Oauth => Self::Token,
            }
        }
    }

    impl From<IceServer> for RtcIceServer {
        fn from(s: IceServer) -> Self {
            let mut ret = RtcIceServer::new();
            let urls = Array::new();
            for u in s.urls {
                let url = JsValue::from_str(&u);
                urls.push(&url);
            }
            if !s.username.is_empty() {
                ret.username(&s.username);
            }
            if !s.credential.is_empty() {
                ret.credential(&s.credential);
            }
            ret.credential_type(s.credential_type.into());
            ret.urls(&urls);
            ret
        }
    }

    impl From<IceServer> for JsValue {
        fn from(a: IceServer) -> Self {
            let ret: RtcIceServer = a.into();
            ret.into()
        }
    }
}

#[cfg(not(feature = "wasm"))]
mod default {
    use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
    use webrtc::ice_transport::ice_server::RTCIceServer;

    use super::IceCredentialType;
    use super::IceServer;

    impl From<IceCredentialType> for RTCIceCredentialType {
        fn from(s: IceCredentialType) -> Self {
            match s {
                IceCredentialType::Unspecified => Self::Unspecified,
                IceCredentialType::Password => Self::Password,
                IceCredentialType::Oauth => Self::Oauth,
            }
        }
    }

    impl From<IceServer> for RTCIceServer {
        fn from(s: IceServer) -> Self {
            Self {
                urls: s.urls,
                username: s.username,
                credential: s.credential,
                credential_type: s.credential_type.into(),
            }
        }
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
