mod consts;
/// A dummy transport use for test.
pub mod transport;

use rand::distributions::Distribution;
use tokio::time::sleep;
use tokio::time::Duration;
pub use transport::DummyTransport;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

use crate::types::ice_transport::IceCandidate;

fn random() -> u64 {
    let range = rand::distributions::Uniform::new(consts::DUMMY_DELAY_MIN, consts::DUMMY_DELAY_MAX);
    let mut rng = rand::thread_rng();
    range.sample(&mut rng)
}

async fn random_delay() {
    sleep(Duration::from_millis(random())).await;
}

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
            sdp_mid: cand.sdp_mid.clone(),
            sdp_mline_index: cand.sdp_m_line_index,
            username_fragment: cand.username_fragment,
        }
    }
}
