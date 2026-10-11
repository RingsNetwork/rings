//! The application end of a test transport: what core handed the application, and what entered
//! the logical inbound stage.
//!
//! ```text
//!   InboundTap ──every payload entering the logical stage──▶ observed
//!   ApplicationInbox ─┬─ Message ──▶ inbounds, inbound_custom_data
//!                     └─ Event   ──▶ events
//! ```
//!
//! Both sources are written synchronously by the pipeline, so every reading first drains them
//! without waiting: a reading taken after an awaited delivery sees that delivery.

use std::sync::Mutex;
use std::sync::PoisonError;

use futures::channel::mpsc;

use crate::dht::Did;
use crate::message::MessagePayload;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationDelivery;
use crate::swarm::application::ApplicationInbox;
use crate::swarm::application::SwarmEvent;
use crate::swarm::transport::SwarmTransport;

/// What the recorder has read so far.
#[derive(Default)]
struct Readings {
    observed: usize,
    inbound_custom_data: Vec<Vec<u8>>,
    inbounds: usize,
    events: Vec<SwarmEvent>,
}

/// The claimed application inbox and the inbound tap of one transport.
pub(super) struct ApplicationRecorder {
    inbox: ApplicationInbox,
    tap: Mutex<mpsc::UnboundedReceiver<MessagePayload>>,
    readings: Mutex<Readings>,
}

impl ApplicationRecorder {
    /// Claim `transport`'s application inbox and subscribe to its inbound tap.
    pub(super) fn attach(transport: &SwarmTransport) -> crate::error::Result<Self> {
        Ok(Self {
            inbox: transport.application().outlet.claim()?,
            tap: Mutex::new(transport.inbound_tap_for_test().subscribe()),
            readings: Mutex::default(),
        })
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

    /// Drain both sources without waiting, and read the readings.
    fn read<T>(&self, reading: impl FnOnce(&Readings) -> T) -> T {
        let mut readings = self.readings.lock().unwrap_or_else(PoisonError::into_inner);
        let mut tap = self.tap.lock().unwrap_or_else(PoisonError::into_inner);
        while let Ok(_payload) = tap.try_recv() {
            readings.observed += 1;
        }
        while let Some(delivery) = self.inbox.try_next() {
            Self::record(&mut readings, delivery);
        }
        reading(&readings)
    }

    /// Payloads that entered the logical inbound stage.
    #[cfg(feature = "dummy")]
    pub(super) fn observed(&self) -> usize {
        self.read(|readings| readings.observed)
    }

    /// Application messages handed to the application.
    #[cfg(feature = "dummy")]
    pub(super) fn inbounds(&self) -> usize {
        self.read(|readings| readings.inbounds)
    }

    /// The bytes of every custom message handed to the application, in inbox order.
    #[cfg(feature = "dummy")]
    pub(super) fn inbound_custom_data(&self) -> Vec<Vec<u8>> {
        self.read(|readings| readings.inbound_custom_data.clone())
    }

    /// Every peer fact handed to the application, in inbox order.
    pub(super) fn events(&self) -> Vec<SwarmEvent> {
        self.read(|readings| readings.events.clone())
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
            Self::record(
                &mut self.readings.lock().unwrap_or_else(PoisonError::into_inner),
                delivery,
            );
        }
    }
}
