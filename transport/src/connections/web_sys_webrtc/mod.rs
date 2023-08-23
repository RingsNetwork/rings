use js_sys::Array;
use wasm_bindgen::JsValue;
use web_sys::RtcIceCredentialType;
use web_sys::RtcIceServer;

use crate::ice_server::IceCredentialType;
use crate::ice_server::IceServer;

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
