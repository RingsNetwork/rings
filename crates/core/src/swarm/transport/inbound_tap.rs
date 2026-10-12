//! Test builds: a tap on every payload the inbound pipeline is about to handle.
//!
//! Core hands the application only application data (#935), so tests that observe core's own
//! protocol traffic read this tap instead. It sits where a payload enters the logical stage,
//! after the connection's admission gate and before dispatch: a chunk, the payload reassembled
//! from it, and a payload drained from the relay inbox each pass it once. Every observation
//! records activity, so a quiescence probe sees a payload before its handler runs.
//!
//! A test may also hold the application lane: its events wait at the logical stage until the
//! hold is dropped, so a witness of lane isolation stalls that lane with nothing of
//! production's own (the application inbox never stalls a lane).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use event_listener::Event;
use futures::channel::mpsc;

use crate::message::MessagePayload;
use crate::swarm::callback::InboundLane;

/// The tap: one optional subscriber, fed a clone of every observed payload, and the holds a
/// test keeps on the application lane.
#[derive(Default)]
pub(crate) struct InboundTap {
    subscriber: Mutex<Option<mpsc::UnboundedSender<MessagePayload>>>,
    holds: Arc<ApplicationLaneHolds>,
}

/// The live holds on the application lane, shared with the holds themselves.
#[derive(Default)]
struct ApplicationLaneHolds {
    count: Mutex<usize>,
    /// Notified whenever a hold is dropped.
    released: Event,
}

impl ApplicationLaneHolds {
    /// The number of live holds; every critical section leaves it consistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, usize> {
        self.count.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether events of `lane` wait: exactly the application lane, while it is held.
    fn holds(&self, lane: InboundLane) -> bool {
        lane == InboundLane::Application && *self.lock() > 0
    }
}

/// Holds the application lane until dropped; see [`InboundTap::hold_application_lane`]. It
/// owns its share of the holds, so it may outlive the borrow of the tap that made it.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) struct LaneHold {
    holds: Arc<ApplicationLaneHolds>,
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
impl Drop for LaneHold {
    fn drop(&mut self) {
        let mut count = self.holds.lock();
        *count = count.saturating_sub(1);
        drop(count);
        self.holds.released.notify(usize::MAX);
    }
}

impl InboundTap {
    /// Hold the application lane: its events wait at the logical stage until the hold is
    /// dropped.
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    pub(crate) fn hold_application_lane(&self) -> LaneHold {
        *self.holds.lock() += 1;
        LaneHold {
            holds: Arc::clone(&self.holds),
        }
    }

    /// Resolve once `lane` is not held.
    pub(crate) async fn pass_lane(&self, lane: InboundLane) {
        loop {
            let listener = self.holds.released.listen();
            if !self.holds.holds(lane) {
                return;
            }
            listener.await;
        }
    }

    /// Subscribe to every later observation, replacing any earlier subscriber. The subscriber
    /// is the native test `Node` (`crate::tests::default`); browser tests probe on activity.
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    pub(crate) fn subscribe(&self) -> mpsc::UnboundedReceiver<MessagePayload> {
        let (sender, receiver) = mpsc::unbounded();
        *self
            .subscriber
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sender);
        receiver
    }

    /// Observe `payload` entering the logical stage.
    pub(crate) fn observe(&self, payload: &MessagePayload) {
        crate::tests::activity::record_activity();
        if let Some(subscriber) = self
            .subscriber
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = subscriber.unbounded_send(payload.clone());
        }
    }
}
