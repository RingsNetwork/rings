//! Relay inbox production: holding messages for offline peers.
//!
//! A `CustomMessage` that reaches the node responsible for its destination's ring position while
//! the destination has no connection is held: wrapped in a [`HeldMessage`] under this node's
//! signature at this instant and written into the destination's inbox carrier (see
//! [`crate::dht::entry::inbox`]) through the ordinary storage write path, so the storage owner
//! admits it only under the inbox write law. The consumer side, delivering one's own inbox, is
//! the swarm's interpretation of the storage maintenance intent (`swarm::inbox`).

use std::sync::Arc;

use super::custom::Reachability;
use super::storage::operate_entry;
use crate::dht::entry::inbox::HeldMessage;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::Did;
use crate::error::Result;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
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
