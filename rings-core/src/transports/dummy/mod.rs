/// A dummy transport use for test.
pub mod transport;

pub use transport::DummyTransport;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

use crate::types::ice_transport::IceCandidate;

impl From<RTCIceCandidateInit> for IceCandidate {
    fn from(cand: RTCIceCandidateInit) -> Self {
        Self {
            candidate: cand.candidate.clone(),
            sdp_mid: cand.sdp_mid.clone(),
            sdp_m_line_index: cand.sdp_mline_index,
            username_fragment: cand.username_fragment,
        }
    }
}

impl From<IceCandidate> for RTCIceCandidateInit {
    fn from(cand: IceCandidate) -> Self {
        Self {
            candidate: cand.candidate.clone(),
            sdp_mid: cand.sdp_mid.clone().unwrap_or_else(|| "0".to_string()),
            sdp_mline_index: cand.sdp_m_line_index.unwrap_or(0),
            username_fragment: cand.username_fragment.unwrap_or_else(|| "".to_string()),
        }
    }
}
