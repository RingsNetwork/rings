use crate::err::{Error, Result};
use serde::Deserialize;
use serde::Serialize;
use serde_json;
use std::sync::Arc;
use wasm_bindgen::JsValue;
use web_sys::RtcSdpType;
use web_sys::RtcSessionDescription;

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SdpType {
    Offer,
    Pranswer,
    Answer,
    Rollback,
}

impl From<SdpType> for web_sys::RtcSdpType {
    fn from(s: SdpType) -> Self {
        match s {
            SdpType::Offer => RtcSdpType::Offer,
            SdpType::Pranswer => RtcSdpType::Pranswer,
            SdpType::Answer => RtcSdpType::Answer,
            SdpType::Rollback => RtcSdpType::Rollback,
        }
    }
}

impl From<web_sys::RtcSdpType> for SdpType {
    fn from(s: web_sys::RtcSdpType) -> Self {
        match s {
            RtcSdpType::Offer => SdpType::Offer,
            RtcSdpType::Pranswer => SdpType::Pranswer,
            RtcSdpType::Answer => SdpType::Answer,
            RtcSdpType::Rollback => SdpType::Rollback,
            _ => SdpType::Offer,
        }
    }
}

#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct RtcSessionDescriptionWrapper {
    pub sdp: String,
    #[serde(rename = "type")]
    pub type_: SdpType,
}

impl From<JsValue> for RtcSessionDescriptionWrapper {
    fn from(s: JsValue) -> Self {
        let sdp = web_sys::RtcSessionDescription::from(s);
        sdp.into()
    }
}

impl From<web_sys::RtcSessionDescription> for RtcSessionDescriptionWrapper {
    fn from(sdp: RtcSessionDescription) -> Self {
        Self {
            sdp: sdp.sdp(),
            type_: sdp.type_().into(),
        }
    }
}

impl TryFrom<String> for RtcSessionDescriptionWrapper {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        serde_json::from_str::<RtcSessionDescriptionWrapper>(&s)
            .map_err(|e| Error::Serialize(Arc::new(e)))
    }
}

impl From<RtcSessionDescriptionWrapper> for web_sys::RtcSessionDescriptionInit {
    fn from(s: RtcSessionDescriptionWrapper) -> Self {
        let mut sdp = web_sys::RtcSessionDescriptionInit::new(s.type_.into());
        sdp.sdp(&s.sdp).clone()
    }
}

// may cause panic here; fix later
impl From<RtcSessionDescriptionWrapper> for web_sys::RtcSessionDescription {
    fn from(s: RtcSessionDescriptionWrapper) -> Self {
        let sdp: web_sys::RtcSessionDescriptionInit = s.into();
        RtcSessionDescription::new_with_description_init_dict(&sdp).unwrap()
    }
}
