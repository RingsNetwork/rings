//! Chain replication of relay holds (#777, `Law (Chain replication)`; durable-records, Section
//! X): the effects of one replication step of `PeerRing::replicate_relay_hold`.
//!
//! ```text
//! writer ──hold──▶ h₀ ──ReplicateRelayHold──▶ m₁ ──▶ … ──▶ m_{q-1}
//! writer ◀──RelayCarrierHeld── h₀ ◀── m₁ ◀── … ◀── m_{q-1}       each once the rest did
//! ```
//!
//! A forward that is lost costs only time: the writer resends the hold each repair pass until
//! it is acknowledged, and every resend that reaches a member forwards it on, digest first, to
//! the next member; chain repair copies the carriers between neighbours besides.

use std::sync::Arc;

use crate::dht::entry::EntryOperation;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::ReplicationStep;
use crate::error::Result;
use crate::message::effects::CoreEffect;
use crate::message::effects::CoreEffectInterpreter;
use crate::message::FindRelayHolder;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::message::RelayHoldDigest;
use crate::message::ReplicateRelayHold;
use crate::swarm::transport::SwarmTransport;

/// Run one replication step for `hold`, which this node took from `upstream` as the holder or a
/// member of its key: whether `upstream` is acknowledged now, this node ending the chain;
/// otherwise the hold is forwarded to the next member, or the next member is asked for, and
/// `upstream` is acknowledged once the rest of the chain did (`acknowledge_relay_replica`).
pub(crate) async fn replicate_taken_hold(
    transport: &Arc<SwarmTransport>,
    hold: &EntryOperation,
    upstream: Did,
) -> Result<bool> {
    let digest = RelayHoldDigest::of(hold)?;
    match transport
        .dht
        .replicate_relay_hold(digest, hold.clone(), upstream)?
    {
        ReplicationStep::Held => Ok(true),
        ReplicationStep::Forward { to, bytes } => {
            forward(transport, digest, bytes.then(|| hold.clone()), to).await;
            Ok(false)
        }
        ReplicationStep::Ask { key, request_id } => {
            ask_owner(transport, key, request_id).await?;
            Ok(false)
        }
        ReplicationStep::Wait => Ok(false),
    }
}

/// Replicate `hold`, which this node wrote and took itself as the holder of its key: no writer
/// waits for the chain, and chain repair copies it besides.
pub(crate) async fn replicate_own_hold(
    transport: &Arc<SwarmTransport>,
    hold: &EntryOperation,
) -> Result<()> {
    replicate_taken_hold(transport, hold, transport.dht.did)
        .await
        .map(drop)
}

/// Ensure an active or pending direct connection to each remote relay chain neighbour (`p₁` and
/// `n₁`, #1182) separated from this node by a storage-less run, so the chain liveness clock probes
/// them even when neither is in the other's `state.predecessor` or `state.successors`.
pub(crate) async fn connect_relay_chain_neighbours(transport: &Arc<SwarmTransport>) -> Result<()> {
    let (previous, next) = transport.dht.remote_relay_chain_neighbours()?;
    let interpreter = CoreEffectInterpreter::new(transport);
    for peer in previous.into_iter().chain(next) {
        if let Err(error) = interpreter.run(CoreEffect::connect_dht_peer(peer)).await {
            tracing::debug!(
                local = %transport.dht.did,
                %peer,
                ?error,
                "relay chain neighbour connection not started; retried next pass"
            );
        }
    }
    Ok(())
}

/// Resume every hold whose next member was unresolved, now that an answer may have resolved it,
/// ask again for a next holder whose query is outstanding (one query per pass at most), and
/// ensure direct links to the current chain neighbours (#1182).
pub(crate) async fn resume_relay_replications(transport: &Arc<SwarmTransport>) -> Result<()> {
    for (hold, upstream) in transport.dht.unforwarded_relay_holds()? {
        replicate_taken_hold(transport, &hold, upstream).await?;
    }
    if let Some((key, request_id)) = transport.dht.outstanding_next_relay_holder_query()? {
        ask_owner(transport, key, request_id).await?;
    }
    connect_relay_chain_neighbours(transport).await
}

/// Send the hold `digest`, with its bytes `hold` or alone, to `member`, best effort: a send that
/// fails is retried by the writer's resend or by chain repair.
pub(crate) async fn forward(
    transport: &Arc<SwarmTransport>,
    digest: RelayHoldDigest,
    hold: Option<EntryOperation>,
    member: Did,
) {
    let message = Message::ReplicateRelayHold(ReplicateRelayHold { digest, hold });
    if let Err(error) = transport
        .protocol_egress()
        .send_message(message, member)
        .await
    {
        tracing::debug!(
            local = %transport.dht.did,
            %member,
            ?error,
            "relay hold not forwarded to the next member; resent or repaired later"
        );
    }
}

/// Ask the owner of `key` for its holder, the first holder past this node's successor list
/// (`FindRelayHolder`, routed toward the owner as a lookup is), best effort: an unanswered query
/// is asked again on the next pass.
pub(crate) async fn ask_owner(
    transport: &Arc<SwarmTransport>,
    key: Did,
    request_id: uuid::Uuid,
) -> Result<()> {
    let PeerRingAction::RemoteAction(next, _) = transport.dht.find_successor(key)? else {
        return Ok(());
    };
    let query = Message::FindRelayHolder(FindRelayHolder { key, request_id });
    if let Err(error) = transport.protocol_egress().send_message(query, next).await {
        tracing::debug!(
            local = %transport.dht.did,
            %key,
            ?error,
            "next relay holder query not queued; asked again next pass"
        );
    }
    Ok(())
}
