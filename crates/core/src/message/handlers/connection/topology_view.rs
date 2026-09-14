use async_trait::async_trait;

use crate::dht::successor::SuccessorReader;
use crate::dht::topology;
use crate::dht::topology::StabilizationConnectionPlan;
use crate::dht::topology::StabilizationConnectionStep;
use crate::dht::topology::SuccessorSyncConnectionPlan;
use crate::dht::topology::SuccessorSyncConnectionStep;
use crate::dht::Did;
use crate::dht::PeerRing;
#[cfg(all(test, not(target_family = "wasm")))]
use crate::dht::TopoInfo;
use crate::error::Result;
use crate::message::types::QueryForTopoInfoReport;
use crate::message::types::Then;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;

/// Admit only bounded topology candidates from a correlated report.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<QueryForTopoInfoReport> for MessageHandler {
    async fn handle(&self, ctx: &MessagePayload, msg: &QueryForTopoInfoReport) -> Result<()> {
        match msg.then {
            <QueryForTopoInfoReport as Then>::Then::SyncSuccessor => {
                let reporter = ctx.transaction.origin();
                if !self
                    .dht
                    .claim_successor_sync_report(reporter, msg.request_id)?
                {
                    return Ok(());
                }
                let mut plan = SuccessorSyncConnectionPlan::new(
                    reporter,
                    msg.request_id,
                    msg.info.successors.iter().copied(),
                    self.dht.did,
                    self.dht.successors().capacity(),
                );
                loop {
                    match self.dht.advance_successor_sync_connection_plan(&mut plan)? {
                        SuccessorSyncConnectionStep::Connect(peer) => {
                            if let Err(error) = self.connect_dht_peer(peer).await {
                                self.dht.cancel_successor_sync(reporter, msg.request_id)?;
                                return Err(error);
                            }
                            if self.transport.get_connection(peer).is_some() {
                                if let Err(error) = self.join_dht(peer).await {
                                    self.dht.cancel_successor_sync(reporter, msg.request_id)?;
                                    return Err(error);
                                }
                            }
                        }
                        SuccessorSyncConnectionStep::Complete => {
                            self.dht.cancel_successor_sync(reporter, msg.request_id)?;
                            break;
                        }
                        SuccessorSyncConnectionStep::Stale => return Ok(()),
                    }
                }
            }
            <QueryForTopoInfoReport as Then>::Then::Stabilization => {
                self.handle_stabilization_report(ctx, msg).await?;
            }
        }
        Ok(())
    }
}

impl MessageHandler {
    async fn handle_stabilization_report(
        &self,
        ctx: &MessagePayload,
        msg: &QueryForTopoInfoReport,
    ) -> Result<()> {
        let reporter = ctx.transaction.origin();
        if !self
            .dht
            .claim_stabilization_report(reporter, msg.request_id)?
        {
            return Ok(());
        }
        let candidates = msg
            .info
            .connection_candidates(self.dht.did, self.dht.successors().capacity());
        let candidates = self
            .transport
            .order_dht_candidates_by_quality(candidates)
            .await;
        let mut plan = StabilizationConnectionPlan::new(
            reporter,
            msg.request_id,
            candidates,
            self.dht.did,
            self.dht.successors().capacity(),
        );
        loop {
            match self.dht.advance_stabilization_connection_plan(&mut plan)? {
                StabilizationConnectionStep::Connect { candidate, .. } => {
                    if let Err(error) = self.connect_dht_peer(candidate).await {
                        self.dht.cancel_stabilization(msg.request_id)?;
                        return Err(error);
                    }
                }
                StabilizationConnectionStep::Complete => break,
                StabilizationConnectionStep::Stale => return Ok(()),
            }
        }

        let stabilized =
            self.transport
                .stabilize_routable_topology(reporter, msg.request_id, &msg.info)?;
        self.dht.cancel_stabilization(msg.request_id)?;
        if let Some(event) = stabilized {
            self.handle_dht_events(&event).await?;
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
pub(super) fn confirmed_topology(info: &TopoInfo, is_active: impl Fn(Did) -> bool) -> TopoInfo {
    info.confirmed_by(is_active)
}

#[cfg(all(test, not(target_family = "wasm")))]
pub(super) fn topology_has_confirmed_peer(info: &TopoInfo) -> bool {
    info.has_confirmed_peer()
}

pub(super) fn connect_successor_hint(dht: &PeerRing, requester: Did, reported: Did) -> Result<Did> {
    if reported != requester {
        return Ok(reported);
    }

    let mut candidates = dht.successors().list()?;
    candidates.push(dht.did);
    candidates.retain(|candidate| *candidate != requester);

    Ok(topology::successors(&candidates, requester, 1)
        .into_iter()
        .next()
        .unwrap_or(reported))
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests;
