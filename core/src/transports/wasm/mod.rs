mod helper;
mod transport;
pub use transport::WasmTransport;
use wasm_bindgen::JsValue;
use web_sys::RtcIceCandidateInit;

use crate::types::ice_transport::IceCandidate;

impl From<IceCandidate> for RtcIceCandidateInit {
    fn from(cand: IceCandidate) -> Self {
        let mut ret = RtcIceCandidateInit::new(&cand.candidate);
        if let Some(mid) = cand.sdp_mid {
            ret.sdp_mid(Some(&mid));
        }
        ret.sdp_m_line_index(cand.sdp_m_line_index);
        // hack here
        if let Some(ufrag) = cand.username_fragment {
            let r = js_sys::Reflect::set(
                &ret,
                &JsValue::from("UsernameFragment"),
                &JsValue::from(&ufrag),
            );
            debug_assert!(
                r.is_ok(),
                "setting properties should never fail on our dictionary objects"
            );
        }
        ret
    }
}
