mod helper;
mod transport;
use crate::types::transport::IceCandidate;
pub use transport::WasmTransport;
use web_sys::RtcIceCandidateInit;

impl From<IceCandidate> for RtcIceCandidateInit {
    fn from(cand: IceCandidate) -> Self {
        let mut ret = RtcIceCandidateInit::new(&cand.candidate);
        ret.sdp_mid(cand.sdp_mid.as_deref());
        ret.sdp_m_line_index(cand.sdp_m_line_index);
        ret
    }
}
