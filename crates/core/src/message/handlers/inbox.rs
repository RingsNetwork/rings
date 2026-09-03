//! Relay inbox handling: holding messages for offline peers and draining one's own inbox.
//!
//! Producer: a `CustomMessage` that reaches the node responsible for its destination's ring
//! position while the destination is not connected is held in the destination's inbox
//! carrier (see [`crate::dht::entry::inbox`]) through the ordinary storage write path, so the
//! storage owner and every replica admit it only under the inbox witness.
//!
//! Consumer: the inbox carrier `d + 1` lies in `d`'s own storage interval once `d` is online, so
//! ownership hand-off moves the held messages to `d` as its predecessor adopts it as successor.
//! Every stabilization round `d` reads its own inbox from local storage, delivers every element
//! that passes the witness under the local overlay to the application exactly as an inbound
//! message would be, and compacts the delivered messages out of the carrier. The compaction
//! floor also reaches every replica and excludes the delivered elements from later joins, so a
//! stale copy synced afterwards cannot redeliver.

use std::sync::Arc;

use super::storage::operate_entry;
use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoder;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// Hold `payload` in the relay inbox of its destination.
///
/// Pre: this node is responsible for the destination's ring position and the destination is not
/// connected.
pub(crate) async fn hold_for_offline_destination(
    transport: Arc<SwarmTransport>,
    payload: &MessagePayload,
) -> Result<()> {
    operate_entry(
        transport,
        EntryOperation::Extend(Entry::inbox_delta(payload)?),
    )
    .await
}

/// Deliver the messages held in this node's own inbox and compact them out of the carrier.
///
/// Post: every deliverable message in the locally stored inbox was offered to the application,
/// and the carrier (locally and at every replica) carries a compaction floor above them.
pub(crate) async fn drain_inbox(
    transport: Arc<SwarmTransport>,
    callback: &SharedSwarmCallback,
) -> Result<()> {
    let now_ms = get_epoch_ms();
    let key = inbox_key(transport.dht.did);
    let Some(inbox) = transport.dht.live_storage_entry(key, now_ms).await? else {
        return Ok(());
    };
    let messages = inbox.deliverable_inbox_messages(transport.network_id);
    if messages.is_empty() {
        return Ok(());
    }

    for payload in &messages {
        if let Err(error) = deliver_inbox_message(callback, payload).await {
            tracing::warn!(
                local = %transport.dht.did,
                tx_id = %payload.transaction.tx_id,
                error = ?error,
                "relay inbox message rejected by the application"
            );
        }
    }

    let delivered = messages
        .iter()
        .map(Encoder::encode)
        .collect::<Result<Vec<_>>>()?;
    let compaction =
        EntryOperation::CompactData(Entry::new(key, delivered, EntryKind::RelayMessage));
    operate_entry(transport, compaction).await
}

/// Offer one inbox message to the application through the same callbacks an inbound message
/// passes: validation first, then delivery.
async fn deliver_inbox_message(
    callback: &SharedSwarmCallback,
    payload: &MessagePayload,
) -> Result<()> {
    callback
        .on_validate(payload)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    callback
        .on_inbound(payload)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))
}

impl MessageHandler {
    /// Whether `destination` is a peer this node must hold messages for: it occupies a ring
    /// position this node is the successor of, and it is not connected.
    pub(super) fn destination_is_offline(&self, destination: Did) -> Result<bool> {
        Ok(destination != self.dht.did
            && !self.transport.is_connected(destination)
            && self.dht.is_responsible_for(destination)?)
    }
}
