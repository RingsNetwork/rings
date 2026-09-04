//! Relay inbox handling: holding messages for offline peers and draining one's own inbox.
//!
//! Producer: a `CustomMessage` that reaches the node responsible for its destination's ring
//! position while the destination has no connection is held: wrapped in a
//! [`HeldMessage`] under this node's signature and written into the destination's inbox carrier
//! (see [`crate::dht::entry::inbox`]) through the ordinary storage write path, so the storage
//! owner admits it only under the inbox witness and the holder authority.
//!
//! Consumer: the inbox carrier `d + 1` lies in `d`'s own storage interval once `d` is online, so
//! the predecessor's storage repair pass hands the held messages to `d`. Every storage
//! maintenance pass `d` reads its own inbox from local storage, delivers every element that
//! passes the witness under the local overlay through the inbound pipeline (application
//! validation, handler dispatch, `on_inbound`, each under the inbound deadline), and retires
//! every element of the drained carrier by its add dot. Delivery is at least once: an element
//! held again between the read and the tombstone is delivered by the next pass, and a message
//! rejected by the application is retired like a delivered one, as the inbound path drops it.

use std::sync::Arc;

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
use crate::swarm::callback::deliver_local_payload;
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
    let held = HeldMessage::hold(payload.clone(), transport.message_signer())?;
    operate_entry(
        transport,
        EntryOperation::Extend(Entry::inbox_delta(&held)?),
    )
    .await
}

/// Deliver the messages held in this node's own inbox and retire the drained elements.
///
/// Post: every element of the locally stored inbox that passes the witness was offered to the
/// application, and every element of that carrier is tombstoned at its owner (locally and, once
/// relocated, wherever the carrier went).
pub(crate) async fn drain_inbox(
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
    let drain = inbox.drain_inbox(now_ms, transport.network_id);
    if drain.rejected > 0 {
        tracing::warn!(
            local = %transport.dht.did,
            rejected = drain.rejected,
            "relay inbox elements failed the witness and are retired undelivered"
        );
    }
    for payload in &drain.deliverable {
        if let Err(error) =
            deliver_local_payload(transport.clone(), callback.clone(), payload).await
        {
            tracing::warn!(
                local = %transport.dht.did,
                tx_id = %payload.transaction.tx_id,
                error = ?error,
                "relay inbox message not accepted by the application"
            );
        }
    }
    operate_entry(transport, EntryOperation::Tombstone(drain.retired)).await
}

impl MessageHandler {
    /// Whether `destination` is a peer this node must hold messages for: it occupies a ring
    /// position this node is responsible for and it has no connection, admitted or pending.
    pub(super) fn destination_is_offline(&self, destination: Did) -> Result<bool> {
        Ok(destination != self.dht.did
            && !self.transport.has_connection_attempt(destination)?
            && self.dht.is_responsible_for(destination)?)
    }
}
