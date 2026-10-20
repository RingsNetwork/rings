//! The swarm's interpretation of the storage maintenance intent to deliver this node's inbox.
//!
//! The inbox carrier `d + 1` lies in `d`'s own storage interval once `d` is online, so the
//! predecessor's storage repair pass hands the held messages to `d`. Every storage maintenance
//! pass the DHT emits [`InboxDelivery::deliver_inbox`]; this interpreter decodes the elements of
//! the inbox from local storage and offers each it has not handed over yet through the inbound
//! pipeline (handler dispatch, then the application inbox), verifying its witness first (an
//! element held ahead of this clock waits for a later pass, as
//! [`InboxElement::is_deliverable_at`] states). An element is retired by its add dot once core
//! is done with it: a message core handles itself at once, an application message only once the
//! application took it from the inbox (its consumption signal, read by a later pass). Taking
//! ends core's custody; what the application does with the message afterwards is its own.
//!
//! ```text
//!   pass:  elements ──poll──▶ Consumed ─────────────────────────────┐
//!                          ├─▶ Untaken: kept                        ├─▶ one removal of their dots
//!                          └─▶ Unknown ──witness ▸ deliver──▶ Handled or refused ┘
//!                                                       ├─▶ Queued: kept, now Untaken
//!                                                       ├─▶ origin quota spent: origin skipped
//!                                                       └─▶ no room, no reader or a local
//!                                                           failure: the pass ends
//! ```
//!
//! Laws:
//! - **At least once within a run.** An element is retired only once core handled it, refused
//!   it as the live path would drop it, or the application took it, so a later pass, a released
//!   inbox or a failed retirement before that never loses it. A restart in between still can:
//!   the replay window refuses the redelivery (#936). An element held again between the read and
//!   its tombstone is delivered by the next pass.
//! - **Never waits on the application.** An application message is offered only through a
//!   reservation in the application inbox; when it finds no room or no reader, the pass ends
//!   and leaves this element and the rest stored for a later pass.
//! - **Per-origin quota.** An origin whose quota is spent for now keeps its later elements stored
//!   for a later pass; the elements of other origins are still offered.
//! - **One retirement per pass.** The pass retires every element it is done with by one removal
//!   of their dots, so a pass costs one storage write. A pass cut short by its step deadline
//!   retires nothing: the next pass finds the taken elements still consumed, and refuses the
//!   ones core handled as replays, retiring both.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::entry::inbox::InboxElement;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryDot;
use crate::dht::entry::EntryOperation;
use crate::dht::Did;
use crate::dht::InboxDelivery;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::handlers::storage::operate_entry;
use crate::swarm::application::RelayConsumption;
use crate::swarm::callback::LocalDelivered;
use crate::swarm::callback::LocalDelivery;
use crate::swarm::transport::Attempts;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// Delivery of this node's inbox to the application inbox.
pub(crate) struct SwarmInboxDelivery {
    transport: Arc<SwarmTransport>,
}

/// What a pass does after offering one element.
enum Offered {
    /// Core is done with it: retire it.
    Retire,
    /// Keep it stored for a later pass and go on with the next element.
    Keep,
    /// Keep it and every later element stored for a later pass.
    EndPass,
}

/// Whether `error` is a failure of this node's own replay store rather than a refusal of the
/// payload: the admission was not committed, so the element waits for a later pass instead of
/// being retired as one the live path would drop.
fn is_local_failure(error: &Error) -> bool {
    matches!(error, Error::TransactionReplayPersistence { .. })
}

impl SwarmInboxDelivery {
    /// Deliver through `transport` to its application inbox.
    pub(crate) fn new(transport: Arc<SwarmTransport>) -> Self {
        Self { transport }
    }

    /// Tombstone `removal` at the carrier's owner, in one attempt (`Attempts::Single`): a
    /// refusal before acceptance is `SingleAttemptRefused`, and the removal had no effect.
    async fn retire(&self, removal: Entry) -> Result<()> {
        let removal = EntryOperation::Tombstone(removal);
        operate_entry(&self.transport, removal, Attempts::Single).await
    }

    /// Offer one element not yet handed over, unless its origin's quota is spent for this pass
    /// (`spent`) or its witness does not admit delivery now.
    async fn offer(
        &self,
        delivery: &LocalDelivery,
        element: &InboxElement,
        now_ms: u128,
        spent: &mut BTreeSet<Did>,
    ) -> Offered {
        let origin = element.held.payload.transaction.origin();
        if spent.contains(&origin) || !element.is_deliverable_at(now_ms, self.transport.network_id)
        {
            return Offered::Keep;
        }
        let local = self.transport.dht.did;
        let tx_id = element.tx_id();
        match delivery.deliver(&element.held.payload).await {
            Ok(LocalDelivered::Handled) => Offered::Retire,
            Ok(LocalDelivered::Queued(taken)) => {
                self.transport
                    .application()
                    .relay_in_flight
                    .queued(tx_id, taken);
                Offered::Keep
            }
            Ok(LocalDelivered::Deferred) => Offered::EndPass,
            Err(Error::OriginQuota(_)) => {
                spent.insert(origin);
                Offered::Keep
            }
            Err(error) if is_local_failure(&error) => {
                tracing::warn!(%local, %tx_id, error = ?error, "relay inbox delivery deferred");
                Offered::EndPass
            }
            Err(error) => {
                tracing::warn!(%local, %tx_id, error = ?error, "relay inbox message refused by core");
                Offered::Retire
            }
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl InboxDelivery for SwarmInboxDelivery {
    /// Post: the elements of the locally stored inbox were taken in order, as the module laws
    /// say, until one found no room, no reader or a local failure; every element core is done
    /// with was tombstoned at its owner by one removal.
    ///
    /// A removal refused before acceptance (`SingleAttemptRefused`, a detached `Cancelled`
    /// included) ends the drain with that error: the element stays at its owner and is taken
    /// again on the next drain. No element is lost, and none is tombstoned twice.
    async fn deliver_inbox(&self) -> Result<()> {
        let now_ms = get_epoch_ms();
        let key = StorageKey::inbox_of(self.transport.dht.did);
        let Some(inbox) = self.transport.dht.live_storage_entry(key, now_ms).await? else {
            return Ok(());
        };
        let stored = inbox.stored_inbox_elements();
        let relay = &self.transport.application().relay_in_flight;
        relay.retain(&stored.iter().map(InboxElement::tx_id).collect());
        let delivery = LocalDelivery::new(self.transport.clone());
        let mut retired: Vec<EntryDot> = Vec::new();
        let mut spent = BTreeSet::new();
        for element in stored {
            let offered = match relay.poll(element.tx_id()) {
                RelayConsumption::Consumed => Offered::Retire,
                RelayConsumption::Untaken => Offered::Keep,
                RelayConsumption::Unknown => {
                    self.offer(&delivery, &element, now_ms, &mut spent).await
                }
            };
            match offered {
                Offered::Retire => retired.push(element.dot),
                Offered::Keep => {}
                Offered::EndPass => break,
            }
        }
        if retired.is_empty() {
            return Ok(());
        }
        self.retire(inbox.removal_of(retired)).await
    }
}
