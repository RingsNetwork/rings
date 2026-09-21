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
//!    [claim dropped: token released on every exit path]
//! ```

use async_trait::async_trait;

use crate::dht::topology;
use crate::dht::topology::stabilization_connection_budget;
use crate::dht::topology::ConnectionPlan;
use crate::dht::topology::ConnectionStep;
use crate::dht::Did;
use crate::dht::PeerRing;
#[cfg(all(test, not(target_family = "wasm")))]
use crate::dht::TopoInfo;
use crate::error::Result;
use crate::message::types::QueryFor;
use crate::message::types::QueryForTopoInfoReport;
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
    /// advances. The claim is released when it drops, so a connection or join
    /// failure releases the token by leaving the handler. A stale plan returns
    /// without emitting additional effects. Stabilization reports are
    /// delegated to `handle_stabilization_report`.
    ///
    /// # Errors
    ///
    /// Returns an error when DHT claim state cannot be accessed, or when an
    /// authorized candidate cannot be connected or joined. The matching token
    /// is cancelled before propagating an effect failure.
    async fn handle(&self, ctx: &MessagePayload, msg: &QueryForTopoInfoReport) -> Result<()> {
        match msg.then {
            QueryFor::SyncSuccessor => {
                // The transaction origin is the only peer allowed to spend the
                // successor-sync request id registered by `SendSuccessorQuery`.
                let reporter = ctx.transaction.origin();
                let Some(_claim) = self
                    .dht
                    .claim_successor_sync_report(reporter, msg.request_id)?
                else {
                    return Ok(());
                };
                // The candidate budget is bounded before any per-candidate
                // work, including the quality ordering below, so an untrusted
                // report length cannot cost more than the budget.
                let capacity = self.dht.successors().capacity();
                let candidates = topology::bounded_connection_candidates(
                    self.dht.did,
                    capacity,
                    msg.info.successors.iter().copied(),
                );
                let candidates = self
                    .transport
                    .order_dht_candidates_by_quality(candidates)
                    .await;
                // The plan owns the bounded candidate cursor. The DHT advances
                // it between async connection attempts so each step can detect
                // cancellation or replacement before doing more work.
                let mut plan = ConnectionPlan::new(
                    reporter,
                    msg.request_id,
                    candidates,
                    self.dht.did,
                    capacity,
                );
                // `Complete` and `Stale` both end the loop; the claim drops
                // with the handler either way. A connection or join failure
                // leaves the handler, and leaving releases the claim.
                while let ConnectionStep::Connect(peer) =
                    self.dht.advance_successor_sync_connection_plan(&mut plan)?
                {
                    self.connect_dht_peer(peer).await?;
                    if self.transport.get_connection(peer).is_some() {
                        self.join_dht(peer).await?;
                    }
                }
            }
            QueryFor::Stabilization => {
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
    /// at most one topology transition, and releases the claim before
    /// follow-up DHT effects execute. Leaving on any error path releases the
    /// claim as well, so a failed handler never leaves the head's token in
    /// `Processing`.
    ///
    /// # Errors
    ///
    /// Returns an error when claim/plan state cannot be read, a candidate
    /// connection fails, routable topology cannot be committed, or a resulting
    /// DHT action cannot be interpreted.
    async fn handle_stabilization_report(
        &self,
        ctx: &MessagePayload,
        msg: &QueryForTopoInfoReport,
    ) -> Result<()> {
        // Only the peer that received the original stabilization query may
        // answer with this request id.
        let reporter = ctx.transaction.origin();
        let Some(claim) = self
            .dht
            .claim_stabilization_report(reporter, msg.request_id)?
        else {
            return Ok(());
        };
        // The candidate budget (successor capacity plus the predecessor) is
        // bounded before the per-candidate quality ordering, so an untrusted
        // report length cannot cost more than the budget.
        let capacity = self.dht.successors().capacity();
        let candidates = topology::bounded_connection_candidates(
            self.dht.did,
            stabilization_connection_budget(capacity),
            msg.info
                .predecessor
                .into_iter()
                .chain(msg.info.successors.iter().copied()),
        );
        let candidates = self
            .transport
            .order_dht_candidates_by_quality(candidates)
            .await;
        // The plan is re-advanced after each await, which lets the DHT reject a
        // stale request before the next advertised candidate is opened.
        let mut plan = ConnectionPlan::new(
            reporter,
            claim.request_id(),
            candidates,
            self.dht.did,
            stabilization_connection_budget(capacity),
        );
        loop {
            match self.dht.advance_stabilization_connection_plan(&mut plan)? {
                ConnectionStep::Connect(candidate) => {
                    // A connection failure leaves the handler, and leaving
                    // releases the claim.
                    self.connect_dht_peer(candidate).await?;
                }
                ConnectionStep::Complete => break,
                ConnectionStep::Stale => return Ok(()),
            }
        }

        // This is the only point where the reported topology can mutate the
        // local ring. The transport layer revalidates `reporter`, `request_id`,
        // and routability under the lifecycle lock; applying the report
        // retires the token, and dropping the claim afterwards is a no-op.
        let stabilized =
            self.transport
                .stabilize_routable_topology(reporter, claim.request_id(), &msg.info)?;
        drop(claim);
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
