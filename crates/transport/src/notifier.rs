//! This module contains the [Notifier] struct.

use std::future::poll_fn;
use std::future::Future;
use std::pin::pin;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use rings_runtime::TimerError;

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use crate::core::transport::WebrtcConnectionState;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use crate::error::Error;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use crate::error::Result;
use crate::sync_utils::lock_recover;

#[derive(Default)]
struct NotifierState {
    /// Indicates whether state has woken.
    pub(crate) woken: bool,

    /// The wakers associated with State.
    pub(crate) wakers: Vec<std::task::Waker>,
}

/// A notifier that can be woken by calling `wake`, and awaited with or without a timeout.
/// Used to notify the data channel state changing in `webrtc_wait_for_data_channel_open` of
/// [crate::core::transport::ConnectionInterface].
#[derive(Clone, Default)]
pub struct Notifier(Arc<Mutex<NotifierState>>);

impl Notifier {
    fn state(&self) -> MutexGuard<'_, NotifierState> {
        lock_recover(&self.0)
    }

    /// Immediately wake the notifier.
    pub fn wake(&self) {
        let mut state = self.state();
        state.woken = true;
        for waker in state.wakers.drain(..) {
            waker.wake();
        }
    }

    /// Wait until the notifier is woken or `timeout` has elapsed, whichever comes first.
    ///
    /// The timeout ends only *this* wait; it never wakes the notifier, so no timer outlives
    /// its waiter and later wakes a shared notifier. The wait is composed in place — nothing
    /// is spawned — so it needs no runtime beyond the one polling it.
    ///
    /// Post: `Ok(())` once woken or elapsed, after which the caller re-checks the condition it
    /// waits on; `Err` iff the runtime could not run the timer (browser only), which ends the
    /// wait at once.
    pub async fn notified_within(&self, timeout: Duration) -> std::result::Result<(), TimerError> {
        let mut woken = pin!(self.clone());
        let mut elapsed = pin!(rings_runtime::sleep(timeout));
        poll_fn(|context| match woken.as_mut().poll(context) {
            Poll::Ready(()) => Poll::Ready(Ok(())),
            Poll::Pending => elapsed.as_mut().poll(context),
        })
        .await
    }
}

impl Future for Notifier {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state();

        if state.woken {
            return Poll::Ready(());
        }

        // A wait that ended by timeout leaves its waker registered, so a task re-waiting on
        // the same notifier must not add another: `wakers` stays bounded by distinct tasks.
        if !state.wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
            state.wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
pub(crate) async fn wait_for_data_channel_open(
    state: WebrtcConnectionState,
    data_channel_is_open: impl Fn() -> Result<bool>,
    notifier: &Notifier,
    timeout_seconds: u8,
) -> Result<()> {
    // `Disconnected` remains eligible: buffered bytes may flush after ICE
    // recovery, and the delivery future observes whether that happened.
    if state.is_terminal() {
        return Err(Error::DataChannelOpen("Connection unavailable".to_string()));
    }
    if data_channel_is_open()? {
        return Ok(());
    }

    notifier
        .notified_within(Duration::from_secs(timeout_seconds.into()))
        .await?;

    if data_channel_is_open()? {
        Ok(())
    } else {
        Err(Error::DataChannelOpen(format!(
            "DataChannel not open in {timeout_seconds} seconds"
        )))
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test_notifier;
