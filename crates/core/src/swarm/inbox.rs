//! The swarm's interpretation of the storage maintenance intent to deliver this node's inbox.
//!
//! The inbox carrier `d + 1` lies in `d`'s own storage interval once `d` is online, so the
//! predecessor's storage repair pass hands the held messages to `d`. Every storage maintenance
//! pass the DHT emits [`InboxDelivery::deliver_inbox`]; this interpreter reads the deliverable
//! elements of the inbox from local storage (an element held ahead of this clock waits for a
//! later pass, as `Entry::deliverable_inbox_elements` states), and, element by element, delivers
//! through the inbound
//! pipeline (handler dispatch, then the application inbox) and retires the element by its add
//! dot once it is done with: a message core handles itself at once, an application message only
//! once the application took it from the inbox (its consumption signal, read by a later pass),
//! so a later pass or a released inbox before that never loses it (a restart in between still
//! can: the replay window refuses the redelivery, #936). Retiring per element
//! makes progress durable: a pass cut short by its step deadline resumes after the last retired
//! element instead of redelivering the same prefix. Delivery is at least once: an element held
//! again between the read and its tombstone is delivered by the next pass, and a message core
//! refuses is retired like a delivered one, as the inbound path drops it. The pass never waits
//! on the application: an application message is offered only through a reservation in the
//! application inbox, and when it finds no room or no reader, or its origin's quota is spent for
//! now, the pass stops and leaves this element and the rest stored for a later pass.

use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::InboxDelivery;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::handlers::storage::operate_entry;
use crate::swarm::application::RelayConsumption;
use crate::swarm::callback::LocalDelivered;
use crate::swarm::callback::LocalDelivery;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// Delivery of this node's inbox to the application inbox.
pub(crate) struct SwarmInboxDelivery {
    transport: Arc<SwarmTransport>,
}

impl SwarmInboxDelivery {
    /// Deliver through `transport` to its application inbox.
    pub(crate) fn new(transport: Arc<SwarmTransport>) -> Self {
        Self { transport }
    }

    /// Tombstone `removal` at the carrier's owner.
    async fn retire(&self, removal: Entry) -> Result<()> {
        let transport = &self.transport;
        operate_entry(
            transport,
            &transport.protocol_egress(),
            EntryOperation::Tombstone(removal),
        )
        .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl InboxDelivery for SwarmInboxDelivery {
    /// Post: the deliverable elements of the locally stored inbox were taken in order until one
    /// found no room, no reader or a spent origin quota: each was handled, and an application
    /// message was queued, to be tombstoned at its owner by a later pass once the application
    /// took it, while any other was tombstoned now; an element held ahead of this clock is left
    /// for a later pass.
    async fn deliver_inbox(&self) -> Result<()> {
        let now_ms = get_epoch_ms();
        let key = StorageKey::inbox_of(self.transport.dht.did);
        let Some(inbox) = self.transport.dht.live_storage_entry(key, now_ms).await? else {
            return Ok(());
        };
        if inbox.data.is_empty() {
            return Ok(());
        }
        let deliverable = inbox.deliverable_inbox_elements(now_ms, self.transport.network_id);
        let relay = &self.transport.application().relay_in_flight;
        relay.retain(
            &deliverable
                .iter()
                .map(|element| element.payload.transaction.tx_id)
                .collect(),
        );
        let delivery = LocalDelivery::new(self.transport.clone());
        for element in deliverable {
            let tx_id = element.payload.transaction.tx_id;
            match relay.poll(tx_id) {
                RelayConsumption::Consumed => {}
                RelayConsumption::Pending | RelayConsumption::Discarded => continue,
                RelayConsumption::Unknown => match delivery.deliver(&element.payload).await {
                    Ok(LocalDelivered::Handled) => {}
                    Ok(LocalDelivered::Queued(taken)) => {
                        relay.queued(tx_id, taken);
                        continue;
                    }
                    // No room or no reader, or the origin's quota is spent for now: the rest
                    // waits for a later pass, as it is.
                    Ok(LocalDelivered::Deferred) | Err(Error::OriginQuota(_)) => return Ok(()),
                    Err(error) => tracing::warn!(
                        local = %self.transport.dht.did,
                        %tx_id,
                        error = ?error,
                        "relay inbox message refused by core"
                    ),
                },
            }
            self.retire(inbox.removal_of([element.dot])).await?;
        }
        Ok(())
    }
}
