use anyhow::anyhow;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

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

#[derive(Default)]
pub struct State {
    pub completed: bool,
    pub successed: Option<bool>,
    pub waker: Option<std::task::Waker>,
}

#[derive(Default)]
pub struct Promise(pub Arc<Mutex<State>>);

impl Future for Promise {
    type Output = Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.0.lock().unwrap();
        if state.completed {
            return match &state.successed {
                Some(true) => Poll::Ready(Ok(())),
                _ => Poll::Ready(Err(anyhow!("failed on promise"))),
            };
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
