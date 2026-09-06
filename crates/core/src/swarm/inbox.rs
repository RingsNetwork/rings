//! The swarm's interpretation of the storage maintenance intent to deliver this node's inbox.
//!
//! The inbox carrier `d + 1` lies in `d`'s own storage interval once `d` is online, so the
//! predecessor's storage repair pass hands the held messages to `d`. Every storage maintenance
//! pass the DHT emits [`InboxDelivery::deliver_inbox`]; this interpreter reads the inbox from
//! local storage, retires at once every element that fails the witness under the local overlay,
//! and then, element by element, delivers through the inbound pipeline (application validation,
//! handler dispatch, `on_inbound`, each under the inbound deadline) and retires the delivered
//! element by its add dot. Retiring per element makes progress durable: a pass cut short by its
//! step deadline resumes after the last retired element instead of redelivering the same prefix.
//! Delivery is at least once: an element held again between the read and its tombstone is
//! delivered by the next pass, and a message rejected by the application is retired like a
//! delivered one, as the inbound path drops it. The application is resolved at delivery time, so
//! `Swarm::set_callback` after `listen` is honoured.

use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::InboxDelivery;
use crate::dht::StorageKey;
use crate::error::Result;
use crate::message::handlers::storage::operate_entry;
use crate::swarm::callback::LocalDelivery;
use crate::swarm::callback::SwarmCallbackSlot;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// Delivery of this node's inbox to whichever application the swarm currently serves.
pub(crate) struct SwarmInboxDelivery {
    transport: Arc<SwarmTransport>,
    callback: SwarmCallbackSlot,
}

impl SwarmInboxDelivery {
    /// Deliver through `transport` to the application in `callback` at delivery time.
    pub(crate) fn new(transport: Arc<SwarmTransport>, callback: SwarmCallbackSlot) -> Self {
        Self {
            transport,
            callback,
        }
    }

    /// Tombstone `removal` at the carrier's owner.
    async fn retire(&self, removal: Entry) -> Result<()> {
        operate_entry(self.transport.clone(), EntryOperation::Tombstone(removal)).await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl InboxDelivery for SwarmInboxDelivery {
    /// Post: every element of the locally stored inbox that passes the witness was offered to
    /// the application and then tombstoned at its owner; every element that fails the witness
    /// was tombstoned unread.
    async fn deliver_inbox(&self) -> Result<()> {
        let now_ms = get_epoch_ms();
        let key = StorageKey::inbox_of(self.transport.dht.did);
        let Some(inbox) = self.transport.dht.live_storage_entry(key, now_ms).await? else {
            return Ok(());
        };
        if inbox.data.is_empty() {
            return Ok(());
        }
        let drain = inbox.partition_inbox(now_ms, self.transport.network_id);
        if !drain.rejected.crdt.dots.is_empty() {
            tracing::warn!(
                local = %self.transport.dht.did,
                rejected = drain.rejected.crdt.dots.len(),
                "relay inbox elements failed the witness and are retired undelivered"
            );
            self.retire(drain.rejected).await?;
        }
        let delivery = LocalDelivery::new(self.transport.clone(), self.callback.current()?);
        for element in drain.deliverable {
            if let Err(error) = delivery.deliver(&element.payload).await {
                tracing::warn!(
                    local = %self.transport.dht.did,
                    tx_id = %element.payload.transaction.tx_id,
                    error = ?error,
                    "relay inbox message not accepted by the application"
                );
            }
            self.retire(inbox.removal_of([element.dot])).await?;
        }
        Ok(())
    }
}
