//! The application end of a test transport: what core handed the application, and what entered
//! the logical inbound stage (read in dummy-backend builds).
//!
//! ```text
//!   InboundTap ──every payload entering the logical stage──▶ observed, tapped
//!   ApplicationInbox ─┬─ Message ──▶ inbounds, inbound_custom_data
//!                     └─ Event   ──▶ events
//! ```
//!
//! The recorder is the transport's one tap subscriber: subscribing again would replace it.
//! Both sources are written synchronously by the pipeline, so every reading first drains its
//! source without waiting: a reading taken after an awaited delivery sees that delivery. A tap
//! reading never drains the inbox, so a test that keeps the inbox full reads the tap freely.

#[cfg(feature = "dummy")]
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

#[cfg(feature = "dummy")]
use futures::channel::mpsc;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use futures::StreamExt;

use crate::dht::Did;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::error::Error;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::CustomMessage;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::Message;
#[cfg(feature = "dummy")]
use crate::message::MessagePayload;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationDelivery;
use crate::swarm::application::ApplicationInbox;
use crate::swarm::application::SwarmEvent;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::inbound_tap::LaneHold;
use crate::swarm::transport::SwarmTransport;

/// What the recorder has read so far.
#[derive(Default)]
struct Readings {
    /// Payloads that entered the logical stage.
    #[cfg(feature = "dummy")]
    observed: usize,
    /// Observed payloads no waiting tap reading has taken yet, in tap order.
    #[cfg(feature = "dummy")]
    tapped: VecDeque<MessagePayload>,
    inbound_custom_data: Vec<Vec<u8>>,
    inbounds: usize,
    events: Vec<SwarmEvent>,
}

/// The claimed application inbox and the inbound tap of one transport.
pub(super) struct ApplicationRecorder {
    inbox: ApplicationInbox,
    /// The tap subscription; only dummy-backend tests read the logical stage.
    #[cfg(feature = "dummy")]
    tap: Mutex<mpsc::UnboundedReceiver<MessagePayload>>,
    readings: Mutex<Readings>,
}

impl ApplicationRecorder {
    /// Claim `transport`'s application inbox and subscribe to its inbound tap.
    pub(super) fn attach(transport: &SwarmTransport) -> crate::error::Result<Self> {
        Ok(Self {
            inbox: transport.application().outlet.claim()?,
            #[cfg(feature = "dummy")]
            tap: Mutex::new(transport.inbound_tap_for_test().subscribe()),
            readings: Mutex::default(),
        })
    }

    /// [`Self::attach`], with `transport`'s Application lane held: an application frame is
    /// observed entering the logical stage and then waits there, undelivered, until the
    /// returned hold is dropped.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn attach_holding_application_lane(
        transport: &SwarmTransport,
    ) -> crate::error::Result<(Self, LaneHold)> {
        let hold = transport.inbound_tap_for_test().hold_application_lane();
        Ok((Self::attach(transport)?, hold))
    }

    /// The readings, however a panicking reader left them.
    fn readings(&self) -> MutexGuard<'_, Readings> {
        self.readings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record one inbox item.
    fn record(readings: &mut Readings, delivery: ApplicationDelivery) {
        match delivery {
            ApplicationDelivery::Message(message) => {
                readings.inbounds += 1;
                if let ApplicationData::Custom(custom) = message.data {
                    readings.inbound_custom_data.push(custom.0);
                }
            }
            ApplicationDelivery::Event(event) => readings.events.push(event),
        }
    }

    /// Drain the tap without waiting, and read the readings.
    #[cfg(feature = "dummy")]
    fn read_tap<T>(&self, reading: impl FnOnce(&mut Readings) -> T) -> T {
        let mut readings = self.readings();
        let mut tap = self.tap.lock().unwrap_or_else(PoisonError::into_inner);
        while let Ok(payload) = tap.try_recv() {
            readings.observed += 1;
            readings.tapped.push_back(payload);
        }
        reading(&mut readings)
    }

    /// Drain the inbox without waiting, and read the readings.
    fn read_inbox<T>(&self, reading: impl FnOnce(&Readings) -> T) -> T {
        let mut readings = self.readings();
        while let Some(delivery) = self.inbox.try_next() {
            Self::record(&mut readings, delivery);
        }
        reading(&readings)
    }

    /// Payloads that entered the logical inbound stage.
    #[cfg(feature = "dummy")]
    pub(super) fn observed(&self) -> usize {
        self.read_tap(|readings| readings.observed)
    }

    /// The message of the next observed payload no tap reading has taken, waiting for it.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(super) async fn next_tapped(&self) -> crate::error::Result<Message> {
        let payload = match self.read_tap(|readings| readings.tapped.pop_front()) {
            Some(payload) => payload,
            None => {
                let payload = futures::future::poll_fn(|cx| {
                    self.tap
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .poll_next_unpin(cx)
                })
                .await
                .ok_or_else(|| Error::InvalidMessage("inbound tap closed".to_string()))?;
                self.readings().observed += 1;
                payload
            }
        };
        payload.transaction.data()
    }

    /// The body of [`Self::next_tapped`], which must be a custom message.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(super) async fn next_tapped_custom(&self) -> crate::error::Result<Vec<u8>> {
        match self.next_tapped().await? {
            Message::CustomMessage(CustomMessage(body)) => Ok(body),
            other => Err(Error::InvalidMessage(format!(
                "expected a custom message, found {other:?}"
            ))),
        }
    }

    /// The messages of every observed payload no tap reading has taken, without waiting.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn drain_tapped(&self) -> crate::error::Result<Vec<Message>> {
        self.read_tap(|readings| readings.tapped.drain(..).collect::<Vec<_>>())
            .into_iter()
            .map(|payload| payload.transaction.data())
            .collect()
    }

    /// Live application data the full inbox dropped.
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn dropped(&self) -> u64 {
        self.inbox.dropped()
    }

    /// Application messages handed to the application.
    #[cfg(feature = "dummy")]
    pub(super) fn inbounds(&self) -> usize {
        self.read_inbox(|readings| readings.inbounds)
    }

    /// The bytes of every custom message handed to the application, in inbox order.
    #[cfg(feature = "dummy")]
    pub(super) fn inbound_custom_data(&self) -> Vec<Vec<u8>> {
        self.read_inbox(|readings| readings.inbound_custom_data.clone())
    }

    /// Every peer fact handed to the application, in inbox order.
    pub(super) fn events(&self) -> Vec<SwarmEvent> {
        self.read_inbox(|readings| readings.events.clone())
    }

    /// Every peer announced retired, in inbox order.
    pub(super) fn retired(&self) -> Vec<Did> {
        self.events()
            .into_iter()
            .filter(|event| matches!(event, SwarmEvent::PeerRetired { .. }))
            .map(|event| event.peer())
            .collect()
    }

    /// Resolve once at least `count` application messages were handed to the application,
    /// waking on the inbox.
    #[cfg(feature = "dummy")]
    pub(super) async fn wait_for_inbounds_at_least(&self, count: usize) {
        while self.inbounds() < count {
            let Some(delivery) = self.inbox.next().await else {
                return;
            };
            Self::record(&mut self.readings(), delivery);
        }
    }
}
