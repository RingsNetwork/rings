//! Topology-report handlers that separate report authorization, candidate
//! connection, and final DHT mutation.
//!
//! A `QueryForTopoInfoReport` is useful only if it spends a matching in-flight
//! request id. This module keeps that correlation near the bounded connection
//! plans so stale reports cannot trigger background connection fan-out.
//!
//! Algorithm flow:
//!
//! ```text
//! [authenticated topology report]
//!                 |
//!                 v
//!       [select report purpose]
//!          /                 \
//!         v                   v
//! [claim successor-sync] [claim stabilization]
//!         |                   |
//!    stale? -> [stop]    stale? -> [stop]
//!         |                   |
//!         v                   v
//! [bounded successors] [bounded predecessor + successors]
//!         |                   |
//!         +--------+----------+
//!                  |
//!                  v
//!       [revalidate token before each candidate]
//!                  |
//!         stale? -> [stop without more effects]
//!                  |
//!                  v
//!          [connect one candidate]
//!                  |
//!          more? --+-- yes --> [revalidate again]
//!                  |
//!                 no
//!                  v
//!       [sync: join connected candidates]
//!       [stab: revalidate routability and commit]
//!                  |
//!                  v
//!             [cancel token]
//! ```

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

/// Admit only bounded topology candidates from a correlated topology report.
///
/// Sync-successor reports connect advertised successors before joining them to
/// the local DHT. Stabilization reports use the same request-id discipline, then
/// commit the reported topology only after the reporter and at least one
/// reported peer are still routable under the transport lifecycle boundary.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl HandleMsg<QueryForTopoInfoReport> for MessageHandler {
    /// Dispatch one report to the exact synchronization flow authorized by its token.
    ///
    /// Successor-sync claims are single-use and revalidated before every
    /// bounded candidate. Each connected candidate is joined before the cursor
    /// advances; any connection or join failure cancels the token. A stale plan
    /// returns without emitting additional effects. Stabilization reports are
    /// delegated to `handle_stabilization_report`.
    ///
    /// # Errors
    ///
    /// Returns an error when DHT claim state cannot be accessed, or when an
    /// authorized candidate cannot be connected or joined. The matching token
    /// is cancelled before propagating an effect failure.
    async fn handle(&self, ctx: &MessagePayload, msg: &QueryForTopoInfoReport) -> Result<()> {
        match msg.then {
            <QueryForTopoInfoReport as Then>::Then::SyncSuccessor => {
                // The transaction origin is the only peer allowed to spend the
                // successor-sync request id registered by `SendSuccessorQuery`.
                let reporter = ctx.transaction.origin();
                if !self
                    .dht
                    .claim_successor_sync_report(reporter, msg.request_id)?
                {
                    return Ok(());
                }
                // The plan owns the bounded candidate cursor. The DHT advances
                // it between async connection attempts so each step can detect
                // cancellation or replacement before doing more work.
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
                                // A failed connection means this report cannot
                                // complete; release the request id immediately.
                                self.dht.cancel_successor_sync(reporter, msg.request_id)?;
                                return Err(error);
                            }
                            if self.transport.get_connection(peer).is_some() {
                                if let Err(error) = self.join_dht(peer).await {
                                    // Joining can fail after the transport is
                                    // ready, so the in-flight successor-sync
                                    // claim still needs explicit cleanup.
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
    /// Handle a stabilization topology report that spends one stabilization request id.
    ///
    /// The report first opens any bounded candidates that might be needed to
    /// validate successor or predecessor evidence. The DHT mutation happens
    /// after those async effects and is still guarded by the transport
    /// lifecycle boundary in `stabilize_routable_topology`.
    ///
    /// Every candidate reservation rechecks that the reporter still owns the
    /// processing token. Completion revalidates transport routability, commits
    /// at most one topology transition, and retires the token before follow-up
    /// DHT effects execute.
    ///
    /// # Errors
    ///
    /// Returns an error when claim/plan state cannot be read, a candidate
    /// connection fails, routable topology cannot be committed, or a resulting
    /// DHT action cannot be interpreted. Connection failures cancel the active
    /// stabilization token before returning.
    async fn handle_stabilization_report(
        &self,
        ctx: &MessagePayload,
        msg: &QueryForTopoInfoReport,
    ) -> Result<()> {
        // Only the peer that received the original stabilization query may
        // answer with this request id.
        let reporter = ctx.transaction.origin();
        if !self
            .dht
            .claim_stabilization_report(reporter, msg.request_id)?
        {
            return Ok(());
        }
        // Candidate order is transport-local quality policy; candidate count is
        // still bounded by successor capacity before any connection attempt.
        let candidates = msg
            .info
            .connection_candidates(self.dht.did, self.dht.successors().capacity());
        let candidates = self
            .transport
            .order_dht_candidates_by_quality(candidates)
            .await;
        // The plan is re-advanced after each await, which lets the DHT reject a
        // stale request before the next advertised candidate is opened.
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
                        // The pending stabilization cannot be completed after a
                        // connection failure, so its request id is released.
                        self.dht.cancel_stabilization(msg.request_id)?;
                        return Err(error);
                    }
                }
                StabilizationConnectionStep::Complete => break,
                StabilizationConnectionStep::Stale => return Ok(()),
            }
        }

        // This is the only point where the reported topology can mutate the
        // local ring. The transport layer revalidates `reporter`, `request_id`,
        // and routability under the lifecycle lock.
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
/// Filter a topology report with the same confirmation predicate used by production code.
pub(super) fn confirmed_topology(info: &TopoInfo, is_active: impl Fn(Did) -> bool) -> TopoInfo {
    info.confirmed_by(is_active)
}

#[cfg(all(test, not(target_family = "wasm")))]
/// Return whether a filtered topology still carries at least one usable peer.
pub(super) fn topology_has_confirmed_peer(info: &TopoInfo) -> bool {
    info.has_confirmed_peer()
}

/// Avoid answering a connect lookup with the requester itself when a better successor is known.
///
/// A self-report would cause the requester to connect to itself. The fallback
/// is the next local successor, or the original report if no alternative exists.
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
/// Regression tests for correlated topology reports and bounded candidate admission.
mod tests;
