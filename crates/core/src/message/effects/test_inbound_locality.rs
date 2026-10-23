//! The inbound-locality law at the boundary every handler sends through (see
//! `swarm::transport::egress`).

use tokio::time::timeout;

use super::CoreEffect;
use super::CoreEffectInterpreter;
use crate::delegation::DelegateeKey;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::NotifyPredecessorSend;
use crate::message::PayloadSender;
use crate::message::QueryForTopoInfoSend;
use crate::tests::activity::probe_on_activity;
use crate::tests::default::connected_nodes;
use crate::tests::default::credit_starvation::starve_every_lane;
use crate::tests::default::TEST_HANG_GUARD;
use crate::tests::TEST_NETWORK_ID;

/// The number of [`CoreEffect`] variants [`variant`] enumerates.
const VARIANTS: usize = 9;

/// The index of `effect`'s variant. The match is exhaustive, so a new variant fails to compile
/// until it is given an index here, and then fails the witness below until it is exercised.
fn variant(effect: &CoreEffect<'_>) -> usize {
    match effect {
        CoreEffect::ForwardPayload { .. } => 0,
        CoreEffect::SendReportMessage { .. } => 1,
        CoreEffect::ResetDestination { .. } => 2,
        CoreEffect::SendMessage { .. } => 3,
        CoreEffect::SendDirectMessage { .. } => 4,
        CoreEffect::SendSuccessorQuery { .. } => 5,
        CoreEffect::ConnectDhtPeer { .. } => 6,
        CoreEffect::HoldForOfflineDestination { .. } => 7,
        CoreEffect::RequestStorageRepair => 8,
    }
}

/// While the next hop withholds credit on every lane, every [`CoreEffect`] returns `Ok` without
/// waiting on it: a send is queued apart from its handler, a connection's offer is negotiated
/// apart from it, and the local effects touch local state alone. A handler therefore never
/// holds the inbound event and lane that carried its request, whatever it asks for. The set of
/// effects run is checked against every variant, so the witness cannot fall behind the type.
/// Under the application discipline a send waits for its first frame's admission
/// instead (`test_an_application_send_waits_for_its_first_frame_while_the_peer_withholds_credit`).
#[tokio::test]
async fn test_every_effect_returns_without_waiting_while_its_next_hop_withholds_credit(
) -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    // The hold's view places its inbox key, so the hold is sent to the holder, `peer`.
    *node1.dht().lock_predecessor()? = Some(peer);
    let held = starve_every_lane(&node2, &node1)?;
    let interpreter = CoreEffectInterpreter::new(&node1.swarm.transport);
    let third = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let carried = || {
        MessagePayload::new_send(
            Message::custom(b"carried by this node")?,
            MessageSigner::new(&third, TEST_NETWORK_ID),
            node1.did(),
            peer,
        )
    };
    let (forwarded, reset, held_for_offline) = (carried()?, carried()?, carried()?);
    let request = node2
        .swarm
        .transport
        .protocol_egress()
        .originate(
            Message::custom(b"a request")?,
            node1.did(),
            Some(node1.did()),
        )
        .await?;
    let unconnected = SecretKey::random().address().into();
    let effects = [
        CoreEffect::forward_payload(&forwarded, None),
        CoreEffect::send_report_message(&request, Message::custom(b"its report")?),
        CoreEffect::reset_destination(&reset, peer),
        CoreEffect::send_message(
            Message::NotifyPredecessorSend(NotifyPredecessorSend { did: node1.did() }),
            peer,
        ),
        CoreEffect::send_direct_message(Message::custom(b"a direct message")?, peer),
        CoreEffect::send_successor_query(QueryForTopoInfoSend::new_for_sync(peer), peer),
        CoreEffect::connect_dht_peer(unconnected),
        CoreEffect::hold_for_offline_destination(&held_for_offline),
        CoreEffect::RequestStorageRepair,
    ];
    let exercised: std::collections::BTreeSet<usize> = effects.iter().map(variant).collect();
    assert_eq!(
        exercised,
        (0..VARIANTS).collect(),
        "every variant is exercised"
    );

    for effect in effects {
        let name = format!("{effect:?}");
        timeout(TEST_HANG_GUARD, interpreter.run(effect))
            .await
            .map_err(|_| Error::InvalidMessage(format!("{name} held its handler")))??;
    }
    drop(held);
    Ok(())
}

/// A protocol send whose first frame is never admitted is cancelled at its deadline, apart from
/// its sender, and returns its outbound capacity: a next hop that withholds credit pins this
/// end's capacity no longer than `DETACHED_FIRST_FRAME_TIMEOUT` per send. The worker holds no
/// idle credit, so the starved lane admits nothing and the one send is the witness.
#[tokio::test]
async fn test_a_starved_protocol_send_returns_its_capacity_at_its_deadline() -> Result<()> {
    let (node1, node2) = connected_nodes().await?;
    let peer = node2.did();
    // The hold's view places its inbox key, so the hold is sent to the holder, `peer`.
    *node1.dht().lock_predecessor()? = Some(peer);
    let held = starve_every_lane(&node2, &node1)?;
    let transport = &node1.swarm.transport;
    let notify = || Message::NotifyPredecessorSend(NotifyPredecessorSend { did: node1.did() });
    let free = || {
        transport
            .hold_control_capacity_for_test(peer)
            .map(|permits| permits.len())
    };
    let queued = || transport.outbound_admitted_transfer_count_for_test(peer);
    let (free_before, queued_before) = (free()?, queued());

    transport
        .protocol_egress()
        .send_direct_message(notify(), peer)
        .await?;
    assert!(free()? < free_before, "the queued send holds capacity");
    // The queue is read without side effects; taking capacity would record activity itself.
    probe_on_activity("the starved send is cancelled", TEST_HANG_GUARD, || {
        let cancelled = (queued() == queued_before).then_some(());
        async move { Ok(cancelled) }
    })
    .await?;
    assert_eq!(
        free()?,
        free_before,
        "the cancelled send returned its capacity"
    );
    drop(held);
    Ok(())
}
