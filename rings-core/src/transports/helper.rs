use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use serde::Deserialize;
use serde::Serialize;

use crate::err::Error;
use crate::err::Result;
use crate::peer::PeerService;
use crate::types::ice_transport::IceCandidate;

/// Custom futures state.
#[derive(Default)]
pub struct State {
    pub completed: bool,
    pub successed: Option<bool>,
    pub waker: Option<std::task::Waker>,
}

/// TricklePayload contain webrtc sdp and candidates.
#[derive(Deserialize, Serialize, Debug)]
pub struct TricklePayload {
    pub sdp: String,
    pub candidates: Vec<IceCandidate>,
    pub services: Vec<PeerService>,
}

/// Custom futures Promise act like js Promise.
#[derive(Default)]
pub struct Promise(pub Arc<Mutex<State>>);

impl Promise {
    pub fn state(&self) -> Arc<Mutex<State>> {
        Arc::clone(&self.0)
    }
}

impl Future for Promise {
    type Output = Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.0.lock().unwrap();
        if state.completed {
            match &state.successed {
                Some(true) => Poll::Ready(Ok(())),
                _ => Poll::Ready(Err(Error::PromiseStateFailed)),
            }
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
