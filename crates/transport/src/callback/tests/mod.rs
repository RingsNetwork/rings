#[cfg(not(target_family = "wasm"))]
use std::sync::atomic::AtomicUsize;
#[cfg(not(target_family = "wasm"))]
use std::sync::atomic::Ordering;
#[cfg(not(target_family = "wasm"))]
use std::sync::Arc;
#[cfg(not(target_family = "wasm"))]
use std::sync::Mutex;

#[cfg(not(target_family = "wasm"))]
use async_trait::async_trait;
use bytes::Bytes;

use super::*;
#[cfg(not(target_family = "wasm"))]
use crate::core::callback::AdmittedInboundMessage;
use crate::core::callback::TransportCallback;
use crate::core::credit::LANE_CREDIT_WINDOW;
use crate::core::pool::ChannelLane;
use crate::core::transport::TransportMessage;
use crate::notifier::Notifier;

/// A transport callback that ignores every event, for tests that never dispatch.
struct IgnoredCallback;

#[cfg_attr(target_family = "wasm", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait::async_trait)]
impl TransportCallback for IgnoredCallback {}

/// The wire form of `message`.
fn wire(message: &TransportMessage) -> Bytes {
    Bytes::from(rings_codec::serialize(message).expect("transport frame must serialize"))
}

/// An [`IgnoredCallback`] for `peer` with the production credit window, its receive load
/// counted in `load`.
fn loaded_callback(peer: &str, load: NodeReceiveLoad) -> InnerTransportCallback {
    InnerTransportCallback::new(peer, Box::new(IgnoredCallback), Notifier::default(), load)
}

/// An [`IgnoredCallback`] for `peer` with the production credit window, alone on its node.
fn callback(peer: &str) -> InnerTransportCallback {
    loaded_callback(peer, NodeReceiveLoad::new())
}

/// Admit a full [`LANE_CREDIT_WINDOW`] of copies of the custom frame `raw` on `lane`, holding
/// every admission.
///
/// Post: `lane`'s window at `callback` is full, so the next custom frame on it is a credit
/// violation until one of the returned frames drops; no other lane is touched.
fn admit_window(
    callback: &InnerTransportCallback,
    raw: &Bytes,
    lane: ChannelLane,
) -> Vec<AdmittedInboundFrame> {
    (0..LANE_CREDIT_WINDOW)
        .map(|_| match callback.admit_inbound_frame(raw.clone(), lane) {
            InboundFrameAdmission::Admitted(frame) => frame,
            _ => panic!("a frame within the window must be admitted"),
        })
        .collect()
}

/// The credit `lane`'s pump at `callback` would send now, taken from its outbox; `None` when
/// none is due.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
fn take_due_credit(callback: &InnerTransportCallback, lane: ChannelLane) -> Option<u64> {
    use futures::FutureExt;

    use crate::core::credit::credit_index;

    callback
        .link_credit()
        .next_credit(credit_index(lane))
        .now_or_never()
        .flatten()
}

/// Reserve a full [`LANE_CREDIT_WINDOW`] of send credit on `lane` at `callback`, holding every
/// reservation.
///
/// Pre: `lane` holds a whole window of unreserved credit and the link is open. Post: the next
/// reservation on `lane` waits until one of the returned reservations drops or credit arrives.
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
fn reserve_window(
    callback: &InnerTransportCallback,
    lane: ChannelLane,
) -> Vec<crate::callback::link_credit::CreditReservation> {
    use futures::FutureExt;

    (0..LANE_CREDIT_WINDOW)
        .map(|_| {
            callback
                .link_credit()
                .reserve(lane)
                .now_or_never()
                .expect("a fresh lane has a window of credit")
                .expect("a fresh lane is open")
        })
        .collect()
}

#[cfg(not(target_family = "wasm"))]
type AdmittedPayloads = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

#[cfg(not(target_family = "wasm"))]
struct RecordingCallback {
    admitted: AdmittedPayloads,
}

#[cfg(not(target_family = "wasm"))]
struct InvalidRecordingCallback {
    invalid: Arc<AtomicUsize>,
}

#[cfg(not(target_family = "wasm"))]
struct PendingInvalidCallback;

#[cfg(not(target_family = "wasm"))]
#[async_trait]
impl TransportCallback for RecordingCallback {
    async fn on_admitted_message(
        &self,
        message: AdmittedInboundMessage<'_>,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (cid, payload, credit) = message.into_parts();
        self.admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((cid.to_owned(), payload.to_vec()));
        drop((payload, credit));
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
#[async_trait]
impl TransportCallback for InvalidRecordingCallback {
    async fn on_invalid_inbound_frame(
        &self,
        _cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.invalid.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
#[async_trait]
impl TransportCallback for PendingInvalidCallback {
    async fn on_invalid_inbound_frame(
        &self,
        _cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        std::future::pending().await
    }
}

#[cfg(not(target_family = "wasm"))]
mod test_admission;
mod test_capacity;
#[cfg(not(target_family = "wasm"))]
mod test_capacity_handoff;
#[cfg(all(not(target_family = "wasm"), feature = "native-webrtc"))]
mod test_credit_pump;
#[cfg(not(target_family = "wasm"))]
mod test_invalid_report;
mod test_wire;
