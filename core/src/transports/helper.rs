#![warn(missing_docs)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use crate::error::Error;
use crate::error::Result;

/// Custom futures state.
pub struct State {
    /// Indicates the completion status of State. It can be either failed or succeeded.
    pub(crate) completed: bool,

    /// Indicates whether the promise has succeeded.
    pub(crate) succeeded: Option<bool>,

    /// The waker associated with State.
    pub(crate) waker: Option<std::task::Waker>,

    /// The timestamp (in milliseconds) when State was created.
    pub(crate) ts_ms: i64,

    /// The timeout (in milliseconds) for State.
    pub(crate) ttl_ms: i64,
}

impl Default for State {
    fn default() -> Self {
        let ts_ms = chrono::Utc::now().timestamp_millis();
        let ttl_ms = 10 * 1000;
        Self {
            completed: false,
            succeeded: None,
            waker: None,
            ts_ms,
            ttl_ms,
        }
    }
}

/// Custom futures Promise act like js Promise.
#[derive(Default)]
pub struct Promise(pub Arc<Mutex<State>>);

impl Promise {
    /// Get state from a Promise
    pub fn state(&self) -> Arc<Mutex<State>> {
        Arc::clone(&self.0)
    }
}

impl Future for Promise {
    type Output = Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.0.lock().unwrap();
        if state.completed {
            match &state.succeeded {
                Some(true) => Poll::Ready(Ok(())),
                _ => Poll::Ready(Err(Error::PromiseStateFailed)),
            }
        } else {
            let time_now = chrono::Utc::now().timestamp_millis();
            if time_now - state.ts_ms > state.ttl_ms {
                return Poll::Ready(Err(Error::PromiseStateTimeout));
            }
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
