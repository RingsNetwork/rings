//! This module contains the [Notifier] struct.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::task::Context;
use std::task::Poll;

use crate::sync_utils::lock_recover;

#[derive(Default)]
struct NotifierState {
    /// Indicates whether state has woken.
    pub(crate) woken: bool,

    /// The wakers associated with State.
    pub(crate) wakers: Vec<std::task::Waker>,
}

/// A notifier that can be woken by calling `wake` or `set_timeout`.
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

    /// Wake the notifier after the specified time.
    #[cfg(not(any(feature = "web-sys-webrtc", feature = "native-webrtc")))]
    pub fn set_timeout(&self, seconds: u8) {
        self.set_timeout_ms(u64::from(seconds) * 1000);
    }

    /// Wake the notifier after the specified time.
    #[cfg(feature = "native-webrtc")]
    pub fn set_timeout(&self, seconds: u8) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(seconds.into())).await;
            this.wake();
        });
    }

    /// Wake the notifier after the specified number of milliseconds.
    #[cfg(feature = "native-webrtc")]
    pub fn set_timeout_ms(&self, millis: u64) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(millis)).await;
            this.wake();
        });
    }

    /// Wake the notifier after the specified number of milliseconds.
    #[cfg(not(any(feature = "web-sys-webrtc", feature = "native-webrtc")))]
    pub fn set_timeout_ms(&self, millis: u64) {
        native_timeout_scheduler::schedule_wake(self.clone(), millis);
    }

    /// Wake the notifier after the specified time.
    #[cfg(feature = "web-sys-webrtc")]
    pub fn set_timeout(&self, seconds: u8) {
        self.set_timeout_ms(u64::from(seconds) * 1000);
    }

    /// Wake the notifier after the specified number of milliseconds.
    #[cfg(feature = "web-sys-webrtc")]
    pub fn set_timeout_ms(&self, millis: u64) {
        use wasm_bindgen::JsCast;

        let millis = i32::try_from(millis).unwrap_or(i32::MAX);

        let timeout_notifier = self.clone();
        let fallback_notifier = self.clone();
        let wake = wasm_bindgen::closure::Closure::once_into_js(move || {
            timeout_notifier.wake();
        });

        let Some(global) = js_utils::global() else {
            fallback_notifier.wake();
            return;
        };

        let callback = wake.as_ref().unchecked_ref();
        let scheduled = global.set_timeout_0(callback, millis);
        if scheduled.is_err() {
            fallback_notifier.wake();
        }
    }
}

impl Future for Notifier {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state();

        if state.woken {
            return Poll::Ready(());
        }

        state.wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(not(any(feature = "web-sys-webrtc", feature = "native-webrtc")))]
mod native_timeout_scheduler {
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;
    use std::sync::mpsc;
    use std::sync::OnceLock;
    use std::time::Duration;
    use std::time::Instant;

    use super::Notifier;

    static TIMER_SCHEDULER: OnceLock<Option<mpsc::Sender<ScheduledWake>>> = OnceLock::new();
    static TIMER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct ScheduledWake {
        deadline: Instant,
        sequence: u64,
        notifier: Notifier,
    }

    impl ScheduledWake {
        fn new(notifier: Notifier, millis: u64) -> Self {
            let now = Instant::now();
            let duration = Duration::from_millis(millis);
            let deadline = now.checked_add(duration).unwrap_or(now);
            let sequence = TIMER_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
            Self {
                deadline,
                sequence,
                notifier,
            }
        }

        #[cfg(test)]
        fn at(notifier: Notifier, deadline: Instant, sequence: u64) -> Self {
            Self {
                deadline,
                sequence,
                notifier,
            }
        }
    }

    impl Ord for ScheduledWake {
        fn cmp(&self, other: &Self) -> Ordering {
            other
                .deadline
                .cmp(&self.deadline)
                .then_with(|| other.sequence.cmp(&self.sequence))
        }
    }

    impl PartialOrd for ScheduledWake {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    impl PartialEq for ScheduledWake {
        fn eq(&self, other: &Self) -> bool {
            self.deadline == other.deadline && self.sequence == other.sequence
        }
    }

    impl Eq for ScheduledWake {}

    pub(super) fn schedule_wake(notifier: Notifier, millis: u64) {
        let request = ScheduledWake::new(notifier.clone(), millis);
        send_or_wake(notifier, request, scheduler());
    }

    fn send_or_wake(
        notifier: Notifier,
        request: ScheduledWake,
        sender: Option<&mpsc::Sender<ScheduledWake>>,
    ) {
        let scheduled = sender
            .map(|sender| sender.send(request).is_ok())
            .unwrap_or(false);
        if !scheduled {
            notifier.wake();
        }
    }

    fn scheduler() -> Option<&'static mpsc::Sender<ScheduledWake>> {
        TIMER_SCHEDULER.get_or_init(spawn_timer_thread).as_ref()
    }

    fn spawn_timer_thread() -> Option<mpsc::Sender<ScheduledWake>> {
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("rings-transport-notifier-timer".to_string())
            .spawn(move || run_timer_thread(receiver));
        match thread {
            Ok(_) => Some(sender),
            Err(error) => {
                tracing::error!("failed to start notifier timer scheduler: {:?}", error);
                None
            }
        }
    }

    fn run_timer_thread(receiver: mpsc::Receiver<ScheduledWake>) {
        let mut pending: BinaryHeap<ScheduledWake> = BinaryHeap::new();
        loop {
            if let Some(next) = pending.peek() {
                let now = Instant::now();
                if next.deadline <= now {
                    if let Some(request) = pending.pop() {
                        request.notifier.wake();
                    }
                    continue;
                }

                match receiver.recv_timeout(next.deadline.saturating_duration_since(now)) {
                    Ok(request) => pending.push(request),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match receiver.recv() {
                    Ok(request) => pending.push(request),
                    Err(_) => return,
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::collections::BinaryHeap;

        use super::*;

        fn is_woken(notifier: &Notifier) -> bool {
            notifier.state().woken
        }

        #[test]
        fn scheduled_wake_heap_orders_earliest_deadline_first() {
            let early = Notifier::default();
            let late = Notifier::default();
            let now = Instant::now();

            let mut pending = BinaryHeap::new();
            pending.push(ScheduledWake::at(
                late.clone(),
                now + Duration::from_millis(100),
                0,
            ));
            pending.push(ScheduledWake::at(
                early.clone(),
                now + Duration::from_millis(10),
                1,
            ));

            pending.pop().unwrap().notifier.wake();
            assert!(is_woken(&early));
            assert!(!is_woken(&late));

            pending.pop().unwrap().notifier.wake();
            assert!(is_woken(&late));
        }

        #[test]
        fn send_or_wake_falls_back_when_scheduler_is_missing_or_closed() {
            let missing_scheduler = Notifier::default();
            let request = ScheduledWake::new(missing_scheduler.clone(), 10_000);
            send_or_wake(missing_scheduler.clone(), request, None);
            assert!(is_woken(&missing_scheduler));

            let closed_scheduler = Notifier::default();
            let (sender, receiver) = mpsc::channel();
            drop(receiver);
            let request = ScheduledWake::new(closed_scheduler.clone(), 10_000);
            send_or_wake(closed_scheduler.clone(), request, Some(&sender));
            assert!(is_woken(&closed_scheduler));
        }
    }
}

// This is copied from utils module of rings-core crate.
#[cfg(feature = "web-sys-webrtc")]
mod js_utils {
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;

    pub enum Global {
        Window(web_sys::Window),
        Worker(web_sys::WorkerGlobalScope),
        ServiceWorker(web_sys::ServiceWorkerGlobalScope),
    }

    impl Global {
        pub fn set_timeout_0(
            &self,
            callback: &js_sys::Function,
            millis: i32,
        ) -> Result<i32, JsValue> {
            match self {
                Global::Window(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
                Global::Worker(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
                Global::ServiceWorker(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
            }
        }
    }

    pub fn global() -> Option<Global> {
        let obj = JsValue::from(js_sys::global());
        if obj.has_type::<web_sys::Window>() {
            return Some(Global::Window(web_sys::Window::from(obj)));
        }
        if obj.has_type::<web_sys::WorkerGlobalScope>() {
            return Some(Global::Worker(web_sys::WorkerGlobalScope::from(obj)));
        }
        if obj.has_type::<web_sys::ServiceWorkerGlobalScope>() {
            return Some(Global::ServiceWorker(
                web_sys::ServiceWorkerGlobalScope::from(obj),
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_notifier() {
        let notifier = Notifier::default();
        notifier.set_timeout(1);

        let mut jobs = vec![];

        // Await three times.
        for _ in 0..3 {
            let notifier_clone = notifier.clone();
            jobs.push(tokio::spawn(async move {
                notifier_clone.await;
            }));
        }

        // Await three times after wake.
        for _ in 0..3 {
            let notifier_clone = notifier.clone();
            jobs.push(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                notifier_clone.await;
            }));
        }

        futures::future::join_all(jobs).await;
        notifier.await;
    }
}
