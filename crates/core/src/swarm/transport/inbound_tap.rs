//! Test builds: a tap on every payload the inbound pipeline is about to handle.
//!
//! Core hands the application only application data (#935), so tests that observe core's own
//! protocol traffic read this tap instead. It sits where a payload enters the logical stage,
//! after the connection's admission gate and before dispatch: a chunk, the payload reassembled
//! from it, and a payload drained from the relay inbox each pass it once. Every observation
//! records activity, so a quiescence probe sees a payload before its handler runs.
//!
//! A test may also hold a logical lane: an event of a held lane waits at the logical stage
//! until the hold is dropped, so a witness of lane isolation stalls one lane with nothing of
//! production's own (#913 R10: the application inbox no longer stalls a lane).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use event_listener::Event;
use futures::channel::mpsc;

use crate::message::MessagePayload;
use crate::swarm::callback::InboundLane;

/// The tap: one optional subscriber, fed a clone of every observed payload, and the lanes a
/// test holds.
#[derive(Default)]
pub(crate) struct InboundTap {
    subscriber: Mutex<Option<mpsc::UnboundedSender<MessagePayload>>>,
    held: Arc<HeldLanes>,
}

/// The lanes a test holds, shared with the holds themselves.
#[derive(Default)]
struct HeldLanes {
    lanes: Mutex<BTreeSet<InboundLane>>,
    /// Notified whenever a hold is dropped.
    released: Event,
}

impl HeldLanes {
    /// The held lanes; every critical section leaves them consistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<InboundLane>> {
        self.lanes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Holds one logical lane until dropped; see [`InboundTap::hold_lane`]. It owns its share of
/// the held lanes, so it may outlive the borrow of the tap that made it.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(crate) struct LaneHold {
    held: Arc<HeldLanes>,
    lane: InboundLane,
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
impl Drop for LaneHold {
    fn drop(&mut self) {
        self.held.lock().remove(&self.lane);
        self.held.released.notify(usize::MAX);
    }
}

impl InboundTap {
    /// Hold `lane`: its events wait at the logical stage until the hold is dropped.
    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    pub(crate) fn hold_lane(&self, lane: InboundLane) -> LaneHold {
        self.held.lock().insert(lane);
        LaneHold {
            held: Arc::clone(&self.held),
            lane,
        }
    }

    /// Resolve once `lane` is not held.
    pub(crate) async fn pass_lane(&self, lane: InboundLane) {
        loop {
            let listener = self.held.released.listen();
            if !self.held.lock().contains(&lane) {
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
