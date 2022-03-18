pub mod transport;

use super::helper::IceCandidateSerializer;
pub use transport::DefaultTransport;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

impl From<RTCIceCandidateInit> for IceCandidateSerializer {
    fn from(cand: RTCIceCandidateInit) -> Self {
        Self {
            candidate: cand.candidate.clone(),
            sdp_mid: cand.sdp_mid.clone(),
            sdp_m_line_index: cand.sdp_mline_index,
            username_fragment: cand.username_fragment,
        }
    }
}

impl From<IceCandidateSerializer> for RTCIceCandidateInit {
    fn from(cand: IceCandidateSerializer) -> Self {
        Self {
            candidate: cand.candidate.clone(),
            sdp_mid: cand.sdp_mid.clone(),
            sdp_mline_index: cand.sdp_m_line_index,
            username_fragment: cand.username_fragment,
        }
    }
}
