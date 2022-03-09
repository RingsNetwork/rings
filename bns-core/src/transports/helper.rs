use serde::Deserialize;
use serde::Serialize;

// Struct From [webrtc-rs](https://docs.rs/webrtc/latest/webrtc/ice_transport/ice_candidate/struct.RTCIceCandidateInit.html)
// For [RFC Std](https://w3c.github.io/webrtc-pc/#dom-rtcicecandidate-tojson), ICE Candidate should be camelCase
// dictionary RTCIceCandidateInit {
//  DOMString candidate = "";
//  DOMString? sdpMid = null;
//  unsigned short? sdpMLineIndex = null;
//  DOMString? usernameFragment = null;
// };

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidateSerializer {
    pub candidate: String,
    pub sdp_mid: String,
    pub sdp_m_line_index: u16,
    pub username_fragment: String,
}
