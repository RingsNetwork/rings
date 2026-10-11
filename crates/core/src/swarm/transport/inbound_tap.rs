//! Test builds: a tap on every payload the inbound pipeline is about to handle.
//!
//! Core hands the application only application data (#935), so tests that observe core's own
//! protocol traffic read this tap instead. It sits where a payload enters the logical stage,
//! after the connection's admission gate and before dispatch: a chunk, the payload reassembled
//! from it, and a payload drained from the relay inbox each pass it once. Every observation
//! records activity, so a quiescence probe sees a payload before its handler runs.

use std::sync::Mutex;
use std::sync::PoisonError;

use futures::channel::mpsc;

use crate::message::MessagePayload;

/// The tap: one optional subscriber, fed a clone of every observed payload.
#[derive(Default)]
pub(crate) struct InboundTap {
    subscriber: Mutex<Option<mpsc::UnboundedSender<MessagePayload>>>,
}

impl InboundTap {
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
