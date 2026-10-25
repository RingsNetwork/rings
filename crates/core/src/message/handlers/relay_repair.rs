//! Chain repair of relay carriers (#777, `Law (Chain repair)`, `Law (Event-driven repair)`;
//! durable-records, Section X): each repair pass, periodic or brought forward by the loss of a
//! chain neighbour, offers one digest set to each chain neighbour; a neighbour asks for the
//! carriers it lacks or holds otherwise, and they are copied to it as additive repair.

use std::sync::Arc;

use async_trait::async_trait;

use super::storage::run_storage_repair_transport_effects;
use crate::dht::entry::PlacedEntry;
use crate::dht::PeerRingAction;
use crate::dht::StorageSyncDestination;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::types::Message;
use crate::message::types::RelayCarrierDigests;
use crate::message::types::RelayCarriersWanted;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms;

/// Send this pass's repair offers: one digest set to each chain neighbour (`Law (Chain
/// repair)`), best effort, as the next pass offers again.
pub(crate) async fn offer_relay_repair(transport: &Arc<SwarmTransport>) -> Result<()> {
    for (neighbour, carriers) in transport.dht.relay_repair_offers(get_epoch_ms()).await? {
        let offer = Message::RelayCarrierDigests(RelayCarrierDigests { carriers });
        if let Err(error) = transport
            .protocol_egress()
            .send_message(offer, neighbour)
            .await
        {
            tracing::debug!(
                local = %transport.dht.did,
                %neighbour,
                ?error,
                "relay repair offer not queued; offered again next pass"
            );
        }
    }
    Ok(())
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayCarrierDigests> for MessageHandler {
    /// Ask the authenticated origin, a chain neighbour, for the offered carriers this node lacks
    /// or holds otherwise, of the keys whose chain makes the origin its neighbour.
    async fn handle(&self, ctx: &MessagePayload, msg: &RelayCarrierDigests) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let keys = self
            .dht
            .wanted_relay_carriers(ctx.transaction.origin(), &msg.carriers, get_epoch_ms())
            .await?;
        if keys.is_empty() {
            return Ok(());
        }
        self.run_effects([CoreEffect::send_report_message(
            ctx,
            Message::RelayCarriersWanted(RelayCarriersWanted { keys }),
        )])
        .await
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<RelayCarriersWanted> for MessageHandler {
    /// Copy to the authenticated origin, a chain neighbour, the carriers it asked for after this
    /// node's offer, as additive repair: the origin admits each as a copy from its neighbour, and
    /// no copy removes anything here.
    async fn handle(&self, ctx: &MessagePayload, msg: &RelayCarriersWanted) -> Result<()> {
        if ctx.should_forward_from(self.dht.did) {
            return self
                .run_effects([CoreEffect::forward_payload(ctx, None)])
                .await;
        }
        let requester = ctx.transaction.origin();
        let carriers = self
            .dht
            .relay_carriers_wanted_by(requester, &msg.keys, get_epoch_ms())
            .await?;
        if carriers.is_empty() {
            return Ok(());
        }
        let copy = PeerRingAction::sync_entries_for_repair(
            StorageSyncDestination::PhysicalOwner(requester),
            carriers
                .into_iter()
                .map(|(key, carrier)| PlacedEntry::new(key, carrier))
                .collect(),
        );
        run_storage_repair_transport_effects(
            &self.transport,
            &self.transport.protocol_egress(),
            copy,
        )
        .await
    }
}
