//! Relay inbox handling: holding messages for offline peers and delivering one's own inbox.
//!
//! Producer: a `CustomMessage` that reaches the node responsible for its destination's ring
//! position while the destination has no connection is held: wrapped in a
//! [`HeldMessage`] under this node's signature at this instant and written into the
//! destination's inbox carrier (see [`crate::dht::entry::inbox`]) through the ordinary storage
//! write path, so the storage owner admits it only under the inbox write law.
//!
//! Consumer: the inbox carrier `d + 1` lies in `d`'s own storage interval once `d` is online, so
//! the predecessor's storage repair pass hands the held messages to `d`. Every storage
//! maintenance pass `d` reads its own inbox from local storage, retires at once every element
//! that fails the witness under the local overlay, and then, element by element, delivers
//! through the inbound pipeline (application validation, handler dispatch, `on_inbound`, each
//! under the inbound deadline) and retires the delivered element by its add dot. Retiring per
//! element makes progress durable: a pass cut short by its step deadline resumes after the last
//! retired element instead of redelivering the same prefix. Delivery is at least once: an
//! element held again between the read and its tombstone is delivered by the next pass, and a
//! message rejected by the application is retired like a delivered one, as the inbound path
//! drops it.

use std::sync::Arc;

use super::custom::Reachability;
use super::storage::operate_entry;
use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::inbox::HeldMessage;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::Did;
use crate::error::Result;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::swarm::callback::LocalDelivery;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// Hold `payload` in the relay inbox of its destination under this node's signature.
///
/// Pre: this node is responsible for the destination's ring position and the destination has
/// no connection; `payload` was verified live on arrival.
pub(crate) async fn hold_for_offline_destination(
    transport: Arc<SwarmTransport>,
    payload: &MessagePayload,
) -> Result<()> {
    let held = HeldMessage::hold(payload.clone(), transport.message_signer(), get_epoch_ms())?;
    operate_entry(
        transport,
        EntryOperation::Extend(Entry::inbox_delta(&held)?),
    )
    .await
}

/// Deliver the messages held in this node's own inbox, retiring each element as it goes.
///
/// Post: every element of the locally stored inbox that passes the witness was offered to the
/// application and then tombstoned at its owner; every element that fails the witness was
/// tombstoned unread.
pub(crate) async fn deliver_inbox(
    transport: Arc<SwarmTransport>,
    callback: SharedSwarmCallback,
) -> Result<()> {
    let now_ms = get_epoch_ms();
    let key = inbox_key(transport.dht.did);
    let Some(inbox) = transport.dht.live_storage_entry(key, now_ms).await? else {
        return Ok(());
    };
    if inbox.data.is_empty() {
        return Ok(());
    }
    let drain = inbox.partition_inbox(now_ms, transport.network_id);
    if !drain.rejected.crdt.dots.is_empty() {
        tracing::warn!(
            local = %transport.dht.did,
            rejected = drain.rejected.crdt.dots.len(),
            "relay inbox elements failed the witness and are retired undelivered"
        );
        retire(&transport, drain.rejected).await?;
    }
    let delivery = LocalDelivery::new(transport.clone(), callback);
    for element in drain.deliverable {
        if let Err(error) = delivery.deliver(&element.payload).await {
            tracing::warn!(
                local = %transport.dht.did,
                tx_id = %element.payload.transaction.tx_id,
                error = ?error,
                "relay inbox message not accepted by the application"
            );
        }
        retire(&transport, inbox.removal_of([element.dot])).await?;
    }
    Ok(())
}

/// Tombstone `removal` at the carrier's owner.
async fn retire(transport: &Arc<SwarmTransport>, removal: Entry) -> Result<()> {
    operate_entry(transport.clone(), EntryOperation::Tombstone(removal)).await
}

impl MessageHandler {
    /// How this node stands to `destination`: a peer it must hold messages for occupies a ring
    /// position this node is responsible for and has no connection, admitted or pending.
    pub(super) fn destination_reachability(&self, destination: Did) -> Result<Reachability> {
        let offline = destination != self.dht.did
            && !self.transport.has_connection_attempt(destination)?
            && self.dht.is_responsible_for(destination)?;
        Ok(if offline {
            Reachability::Offline
        } else {
            Reachability::Reachable
        })
    }
}
