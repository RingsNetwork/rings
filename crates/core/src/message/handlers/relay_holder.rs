//! Handlers of relay custody (see `dht::relay_holder`): the previous holder a predecessor passes,
//! a recipient's fetch of its inbox, the holder's report back, a holder's acknowledgement or a
//! target's refusal of a hold, back to its writer, and a keeper's query of a key's owner for its
//! holder, with the answer.

use async_trait::async_trait;

use crate::dht::entry::inbox::inbox_key;
use crate::dht::entry::EntryOperation;
use crate::dht::relay_holder::owner_answer;
use crate::dht::relay_holder::PreviousHolder;
use crate::dht::relay_holder::RelayHolder;
use crate::dht::Chord;
use crate::dht::PeerRingAction;
use crate::dht::ReplicaAcknowledgement;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::handlers::inbox::send_relay_hold;
use crate::message::handlers::relay_replica::ask_owner;
use crate::message::handlers::relay_replica::forward;
use crate::message::handlers::relay_replica::replicate_taken_hold;
use crate::message::handlers::relay_replica::resume_relay_replications;
use crate::message::handlers::HandlerContext;
use crate::message::types::AnnounceRelayHolder;
use crate::message::types::DeclareRelayCustody;
use crate::message::types::FetchRelayInbox;
use crate::message::types::FindRelayHolder;
use crate::message::types::Message;
use crate::message::types::RelayCarrierHeld;
use crate::message::types::RelayCarrierRefused;
use crate::message::types::RelayHolderFound;
use crate::message::types::RelayInboxReport;
use crate::message::types::ReplicateRelayHold;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::PayloadSender;
use crate::message::RelayHoldDigest;
use crate::swarm::inbox::SwarmInboxDelivery;
use crate::utils::get_epoch_ms;

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<AnnounceRelayHolder> for MessageHandler {
    /// Take the previous holders the authenticated origin passed, iff it is this node's
    /// predecessor. A record this node does not take is ignored: the predecessor passes it again
    /// on its next repair pass.
    async fn handle(&self, context: &HandlerContext<'_>, msg: &AnnounceRelayHolder) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let from = ctx.transaction.origin();
        let previous = msg
            .previous
            .iter()
            .map(|link| PreviousHolder {
                holder: link.holder,
                run_start: link.run_start,
            })
            .collect::<Vec<_>>();
        if !self.dht.accept_previous_relay_holder(from, previous)? {
            tracing::debug!(
                local = %self.dht.did,
                %from,
                previous = ?msg.previous,
                "previous relay holders not taken"
            );
        }
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FetchRelayInbox> for MessageHandler {
    /// Report the live inbox of the authenticated origin to it, iff this node is a member of its
    /// inbox key under its own view (#777); otherwise the fetch is refused with
    /// `RelayCarrierNotHeldHere`.
    async fn handle(&self, context: &HandlerContext<'_>, _: &FetchRelayInbox) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let recipient = ctx.transaction.origin();
        if !self.dht.is_relay_member(inbox_key(recipient))? {
            return Err(Error::RelayCarrierNotHeldHere);
        }
        let inbox = self
            .dht
            .live_storage_entry(StorageKey::inbox_of(recipient), get_epoch_ms())
            .await?;
        self.run_effects([CoreEffect::send_report_message(
            ctx,
            Message::RelayInboxReport(RelayInboxReport { inbox }),
        )])
        .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayInboxReport> for MessageHandler {
    /// Drain the inbox the authenticated origin reported, iff it is a member of this node's inbox
    /// key under this node's own view (see `SwarmInboxDelivery::deliver_fetched`).
    async fn handle(&self, context: &HandlerContext<'_>, msg: &RelayInboxReport) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        SwarmInboxDelivery::new(self.transport.clone())
            .deliver_fetched(ctx.transaction.origin(), msg.inbox.clone())
            .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayCarrierHeld> for MessageHandler {
    /// Pass a next member's acknowledgement of a hold this node forwards up the chain, to the
    /// node the hold came from (`Law (Chain replication)`); otherwise stop keeping a hold this
    /// node wrote, iff the node it was last sent to acknowledges it. An acknowledgement from any
    /// other node, or of a hold this node neither forwards nor keeps, changes nothing.
    async fn handle(&self, context: &HandlerContext<'_>, msg: &RelayCarrierHeld) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let from = ctx.transaction.origin();
        match self.dht.acknowledge_relay_replica(msg.hold, from)? {
            ReplicaAcknowledgement::NotReplicated => {
                self.dht.acknowledge_relay_hold(msg.hold, from).map(drop)
            }
            ReplicaAcknowledgement::Held { upstream } if upstream == self.dht.did => Ok(()),
            ReplicaAcknowledgement::Held { upstream } => self
                .transport
                .protocol_egress()
                .send_message(
                    Message::RelayCarrierHeld(RelayCarrierHeld { hold: msg.hold }),
                    upstream,
                )
                .await
                .map(drop),
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<ReplicateRelayHold> for MessageHandler {
    /// Take a hold the authenticated origin forwarded as this node's previous member of its key
    /// (`Law (Chain replication)`), forward it on unless this node ends the chain, and
    /// acknowledge it to the origin once the rest of the chain did. A forward of the digest alone
    /// names a hold this node forwards already; for any other, the origin is asked for the bytes
    /// (`RelayCarrierRefused`). The hold is judged as a relocated carrier is, as of its hold
    /// instant, since the holder judged its freshness when it took it.
    async fn handle(&self, context: &HandlerContext<'_>, msg: &ReplicateRelayHold) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let sender = ctx.transaction.origin();
        let (hold, fresh) = match &msg.hold {
            Some(hold) => (hold.clone(), true),
            None => match self.dht.replicated_relay_hold(msg.digest)? {
                Some(hold) => (hold, false),
                None => {
                    return self
                        .run_effects([CoreEffect::send_report_message(
                            ctx,
                            Message::RelayCarrierRefused(RelayCarrierRefused { hold: msg.digest }),
                        )])
                        .await;
                }
            },
        };
        if RelayHoldDigest::of(&hold)? != msg.digest {
            return Err(Error::InvalidMessage(
                "forwarded relay hold does not match its digest".to_string(),
            ));
        }
        let EntryOperation::Extend(entry) = &hold else {
            return Err(Error::RelayInboxOperationNotAllowed);
        };
        if !entry.kind.is_relay_inbox() {
            return Err(Error::RelayInboxOperationNotAllowed);
        }
        if !self.dht.is_previous_relay_member(entry.did, sender)? {
            return Err(Error::RelayCarrierNotHeldHere);
        }
        if fresh {
            let now_ms = get_epoch_ms();
            let admitted = entry
                .clone()
                .validate_admissible_at(now_ms, self.transport.network_id)?;
            self.dht
                .join_admitted_storage_entry(now_ms, entry.did, admitted, context.introducer())
                .await?;
        }
        if replicate_taken_hold(&self.transport, &hold, sender).await? {
            self.run_effects([CoreEffect::send_report_message(
                ctx,
                Message::RelayCarrierHeld(RelayCarrierHeld { hold: msg.digest }),
            )])
            .await?;
        }
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayCarrierRefused> for MessageHandler {
    /// Send the bytes of a hold this node forwarded by its digest alone to the next member that
    /// asks for them (`Law (Chain replication)`). Otherwise send again at once a kept hold the
    /// node it was last sent to refuses, iff this node's view now names another holder; the next
    /// repair pass sends it again besides. A refusal of a hold this node neither forwards nor
    /// keeps, or from another node, changes nothing.
    async fn handle(&self, context: &HandlerContext<'_>, msg: &RelayCarrierRefused) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let refuser = ctx.transaction.origin();
        if let Some(hold) = self.dht.refused_relay_replica(msg.hold, refuser)? {
            forward(&self.transport, msg.hold, Some(hold), refuser).await;
            return Ok(());
        }
        let Some(hold) = self.dht.refused_relay_hold(msg.hold, refuser)? else {
            return Ok(());
        };
        match self.dht.relay_holder(hold.entry().did)? {
            RelayHolder::Holder(holder) if holder != refuser => {
                send_relay_hold(&self.transport, hold).await
            }
            _ => Ok(()),
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<FindRelayHolder> for MessageHandler {
    /// Answer with the holder of `msg.key` as this node's view names it, when this node owns the
    /// key (`Law (Owner places)`); otherwise route the query on toward the owner, as a lookup is.
    async fn handle(&self, context: &HandlerContext<'_>, msg: &FindRelayHolder) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        match self.dht.find_successor(msg.key)? {
            PeerRingAction::Some(_) => {
                let holder = owner_answer(&self.dht.relay_segment()?, self.dht.did, msg.key);
                // With no holder, the last node this owner places, past which the asker asks
                // again for its next member (#777).
                let horizon = match holder {
                    Some(_) => None,
                    None => self
                        .dht
                        .with_topology_state(|state| state.successors.last().copied())?,
                };
                self.run_effects([CoreEffect::send_report_message(
                    ctx,
                    Message::RelayHolderFound(RelayHolderFound {
                        key: msg.key,
                        holder,
                        horizon,
                    }),
                )])
                .await
            }
            PeerRingAction::RemoteAction(next, _) => {
                self.run_effects([CoreEffect::reset_destination(ctx, next)])
                    .await
            }
            action => Err(Error::unexpected_peer_ring_action(action)),
        }
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayHolderFound> for MessageHandler {
    /// Take the answer to this node's outstanding query for `msg.key` (`Law (Asked)`). An answer
    /// about its next holder (#777) is cached, the holds waiting for it resume, and an owner that
    /// names none is asked past by its horizon. Otherwise record the holder the owner named as a
    /// keeper's hint, and ask for a repair pass, which offers the carrier there, only when that
    /// hint changed. An answer nobody asked for, a second answer, or a wrong one costs no pass and
    /// moves nothing.
    async fn handle(&self, context: &HandlerContext<'_>, msg: &RelayHolderFound) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        if self.dht.asks_next_relay_holder(msg.key)? {
            return match self
                .dht
                .take_next_relay_holder(msg.key, msg.holder, msg.horizon)?
            {
                Some(again) => ask_owner(&self.transport, again).await,
                None => resume_relay_replications(&self.transport).await,
            };
        }
        if self.dht.name_relay_holder(msg.key, msg.holder)? {
            self.run_effects([CoreEffect::request_storage_repair()])
                .await?;
        }
        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<DeclareRelayCustody> for MessageHandler {
    /// Record the declaration the authenticated origin made after its handshake (`Law (Age
    /// gate)`), as its handshake's declaration is recorded.
    async fn handle(&self, context: &HandlerContext<'_>, msg: &DeclareRelayCustody) -> Result<()> {
        let ctx = context.payload();
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        self.dht
            .observe_relay_custody(ctx.transaction.origin(), msg.custody)
    }
}
