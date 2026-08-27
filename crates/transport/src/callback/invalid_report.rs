#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::future::poll_fn;
#[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
use std::rc::Rc;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::sync::atomic::AtomicUsize;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::sync::atomic::Ordering;
#[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
use std::sync::Arc;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use std::task::Poll;

use super::InnerTransportCallback;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
use crate::core::drop_guard::ArmedDropGuard;

#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
pub(super) const INVALID_FRAME_REPORT_BACKLOG_CAPACITY: usize = 256;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
pub(super) const INVALID_FRAME_REPORT_QUANTUM: usize = 32;
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
const INVALID_FRAME_WORKER_ACTIVE: usize = 1 << (usize::BITS - 1);
#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
const INVALID_FRAME_REPORT_COUNT_MASK: usize = INVALID_FRAME_WORKER_ACTIVE - 1;

#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
const _: () = {
    assert!(INVALID_FRAME_REPORT_BACKLOG_CAPACITY <= INVALID_FRAME_REPORT_COUNT_MASK);
};

#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
fn release_invalid_frame_worker(state: &AtomicUsize) {
    state.swap(0, Ordering::AcqRel);
}

#[cfg(any(
    test,
    all(not(target_family = "wasm"), feature = "tokio"),
    all(target_family = "wasm", feature = "web-sys-webrtc")
))]
async fn yield_invalid_frame_report_worker() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

impl InnerTransportCallback {
    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    pub(super) fn queue_invalid_inbound_frame(&self) -> bool {
        self.invalid_frame_report_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let pending = (state & INVALID_FRAME_REPORT_COUNT_MASK)
                    .saturating_add(1)
                    .min(INVALID_FRAME_REPORT_BACKLOG_CAPACITY);
                Some(INVALID_FRAME_WORKER_ACTIVE | pending)
            })
            .map(|previous| previous & INVALID_FRAME_WORKER_ACTIVE == 0)
            .unwrap_or(false)
    }

    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    fn take_invalid_inbound_frame(&self) -> bool {
        self.invalid_frame_report_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                ((state & INVALID_FRAME_REPORT_COUNT_MASK) > 0).then_some(state - 1)
            })
            .is_ok()
    }

    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    fn release_invalid_frame_worker_if_idle(&self) -> bool {
        self.invalid_frame_report_state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                ((state & INVALID_FRAME_REPORT_COUNT_MASK) == 0).then_some(0)
            })
            .is_ok()
    }

    #[cfg(any(
        test,
        all(not(target_family = "wasm"), feature = "tokio"),
        all(target_family = "wasm", feature = "web-sys-webrtc")
    ))]
    pub(super) async fn drain_invalid_inbound_frames(&self) {
        let mut active_guard = ArmedDropGuard::new(
            &self.invalid_frame_report_state,
            release_invalid_frame_worker,
        );
        loop {
            let mut processed = 0;
            while processed < INVALID_FRAME_REPORT_QUANTUM && self.take_invalid_inbound_frame() {
                self.notify_invalid_inbound_frame().await;
                processed += 1;
            }

            if self.release_invalid_frame_worker_if_idle() {
                active_guard.disarm();
                return;
            }
            yield_invalid_frame_report_worker().await;
        }
    }

    /// Notify the callback about one malformed or oversized inbound frame.
    ///
    /// Custom adapters without a built-in runtime feature can spawn or await
    /// this method after [`Self::admit_inbound_frame`] rejects remote-invalid
    /// input. Local capacity pressure must not call it.
    pub async fn notify_invalid_inbound_frame(&self) {
        if let Err(error) = self.callback.on_invalid_inbound_frame(&self.cid).await {
            tracing::error!("Callback on_invalid_inbound_frame failed: {error:?}");
        }
    }

    #[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
    /// Report one malformed or oversized frame without blocking adapter ingress.
    pub fn report_invalid_inbound_frame(self: &Arc<Self>) {
        if !self.queue_invalid_inbound_frame() {
            return;
        }
        let callback = Arc::clone(self);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            callback
                .invalid_frame_report_state
                .swap(0, Ordering::AcqRel);
            tracing::error!(peer = %callback.cid, "invalid-frame reporter requires a Tokio runtime");
            return;
        };
        runtime.spawn(async move { callback.drain_invalid_inbound_frames().await });
    }

    #[cfg(all(target_family = "wasm", feature = "web-sys-webrtc"))]
    /// Report one malformed or oversized frame without blocking adapter ingress.
    pub fn report_invalid_inbound_frame(self: &Rc<Self>) {
        if self.queue_invalid_inbound_frame() {
            let callback = Rc::clone(self);
            wasm_bindgen_futures::spawn_local(async move {
                callback.drain_invalid_inbound_frames().await;
            });
        }
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn pending_invalid_frame_count_for_test(&self) -> usize {
        self.invalid_frame_report_state.load(Ordering::Acquire) & INVALID_FRAME_REPORT_COUNT_MASK
    }
}
