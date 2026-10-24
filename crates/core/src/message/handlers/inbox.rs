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
use crate::dht::relay_holder::is_unplaced_hold;
use crate::dht::relay_holder::RelayHolder;
use crate::dht::Did;
use crate::error::Result;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::RelayHoldDigest;
use crate::swarm::transport::Attempts;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// Hold `payload` in the relay inbox of its destination under this node's signature.
///
/// Pre: this node is responsible for the destination's ring position and the destination has
/// no connection; `payload` was verified live on arrival.
///
/// One attempt (`Attempts::Single`): this runs on the inbound lane, which a rerouting wait must
/// never hold, and it is sent under the protocol discipline. A refusal before acceptance is `SingleAttemptRefused` (the hold had no effect);
/// before #859 the same refusal was the send's raw error, and a detached `Cancelled` was `Ok`.
///
/// Post: as [`send_relay_hold`]: the hold is kept until a holder acknowledges it.
pub(crate) async fn hold_for_offline_destination(
    transport: Arc<SwarmTransport>,
    payload: &MessagePayload,
) -> Result<()> {
    let now_ms = get_epoch_ms();
    let held = HeldMessage::hold(payload.clone(), transport.message_signer(), now_ms)?;
    let hold = transport
        .dht
        .stamp_operation(
            EntryOperation::Extend(Entry::inbox_delta(&held)?),
            now_ms,
            None,
        )
        .await?;
    send_relay_hold(&transport, hold).await
}

/// Send `hold`, a stamped hold this node wrote, to the holder its view names, in one attempt.
///
/// Law (Acknowledged): the hold is kept from before its first send until the holder it was last
/// sent to acknowledges it (`RelayCarrierHeld`), it takes effect here, it is refused for good, or
/// its sender proof expires; every repair pass sends every kept hold again (stamping is
/// idempotent, so a resend is byte-identical and its join at the holder changes nothing). No
/// message carries the mail back, so a lost acknowledgement or refusal costs a resend, never the
/// hold. At most `UNACKNOWLEDGED_RELAY_HOLDS_MAX` holds are kept: another is refused to its
/// local sender with `RelayHoldsUnacknowledgedFull`.
///
/// Post: the error of a hold that took no effect is returned, so it is observed as not placed
/// yet; it is still kept (`is_unplaced_hold`). An error refusing it for good forgets it.
pub(crate) async fn send_relay_hold(
    transport: &Arc<SwarmTransport>,
    hold: EntryOperation,
) -> Result<()> {
    let digest = RelayHoldDigest::of(&hold)?;
    let local = transport.dht.did;
    let target = match transport.dht.relay_holder(hold.entry().did)? {
        RelayHolder::Holder(holder) => Some(holder),
        RelayHolder::Unheld | RelayHolder::Unknown => None,
    };
    transport
        .dht
        .await_relay_hold(digest, hold.clone(), target)?;
    let result = operate_entry(transport, hold, Attempts::Single).await;
    let settled = match &result {
        Ok(()) => target == Some(local),
        Err(error) => !is_unplaced_hold(error),
    };
    if settled {
        transport.dht.forget_relay_hold(digest)?;
    }
    result
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
