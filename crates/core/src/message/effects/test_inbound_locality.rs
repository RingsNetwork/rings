//! The inbound-locality law at the boundary every handler sends through (#913 R8 H1; see
//! `swarm::transport::egress`).

use std::sync::Arc;

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
use crate::swarm::callback::SharedSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::tests::activity::probe_on_activity;
use crate::tests::default::credit_starvation::connected_pair;
use crate::tests::default::credit_starvation::starve_every_lane;
use crate::tests::default::TEST_HANG_GUARD;
use crate::tests::TEST_NETWORK_ID;

/// A callback without behaviour: the interpreter needs one to start connections.
struct NoopCallback;

impl SwarmCallback for NoopCallback {}

/// While the next hop withholds credit on every lane, each sending [`CoreEffect`] returns `Ok`
/// with its payload waiting in the next hop's queue. A handler therefore never holds the
/// inbound event and lane that carried its request, whichever peer its forward, report, query
/// or notification goes to. Under the application discipline the same send waits for its first
/// frame's admission and fails at that deadline instead
/// (`test_an_application_send_waits_for_its_first_frame_while_the_peer_withholds_credit`).
#[tokio::test]
async fn test_every_effect_returns_once_queued_while_its_next_hop_withholds_credit() -> Result<()> {
    let (node1, node2) = connected_pair().await?;
    let peer = node2.did();
    let held = starve_every_lane(&node2, &node1)?;
    let callback: SharedSwarmCallback = Arc::new(NoopCallback);
    let interpreter = CoreEffectInterpreter::new(&node1.swarm.transport, &callback);
    let third = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let carried = || {
        MessagePayload::new_send(
            Message::custom(b"carried by this node")?,
            MessageSigner::new(&third, TEST_NETWORK_ID),
            node1.did(),
            peer,
        )
    };
    let (forwarded, reset) = (carried()?, carried()?);
    let request = node2
        .swarm
        .transport
        .originate(
            Message::custom(b"a request")?,
            node1.did(),
            Some(node1.did()),
        )
        .await?;
    let effects = [
        CoreEffect::forward_payload(&forwarded, None),
        CoreEffect::reset_destination(&reset, peer),
        CoreEffect::send_report_message(&request, Message::custom(b"its report")?),
        CoreEffect::send_message(
            Message::NotifyPredecessorSend(NotifyPredecessorSend { did: node1.did() }),
            peer,
        ),
        CoreEffect::send_direct_message(Message::custom(b"a direct message")?, peer),
        CoreEffect::send_successor_query(QueryForTopoInfoSend::new_for_sync(peer), peer),
    ];

    for (queued, effect) in (1..).zip(effects) {
        let name = format!("{effect:?}");
        timeout(TEST_HANG_GUARD, interpreter.run(effect))
            .await
            .map_err(|_| Error::InvalidMessage(format!("{name} held its handler")))??;
        assert_eq!(
            node1
                .swarm
                .transport
                .outbound_admitted_transfer_count_for_test(peer),
            Some(queued),
            "{name} waits in the next hop's queue, apart from its handler"
        );
    }
    drop(held);
    Ok(())
}

/// A protocol send whose first frame is never admitted is cancelled at its deadline, apart from
/// its sender, and returns its outbound capacity: a next hop that withholds credit pins this
/// end's capacity no longer than `DETACHED_FIRST_FRAME_TIMEOUT` per send. (The worker may hold
/// one credit reserved before the lane was starved; a first send spends it, so the witness is
/// the second.)
#[tokio::test]
async fn test_a_starved_protocol_send_returns_its_capacity_at_its_deadline() -> Result<()> {
    let (node1, node2) = connected_pair().await?;
    let peer = node2.did();
    let held = starve_every_lane(&node2, &node1)?;
    let transport = &node1.swarm.transport;
    let egress = transport.protocol_egress();
    let notify = || Message::NotifyPredecessorSend(NotifyPredecessorSend { did: node1.did() });
    let free = || {
        transport
            .hold_control_capacity_for_test(peer)
            .map(|permits| permits.len())
    };
    let queued = || transport.outbound_admitted_transfer_count_for_test(peer);
    egress.send_direct_message(notify(), peer).await?;
    let (queued_before, free_before) = (queued(), free()?);

    egress.send_direct_message(notify(), peer).await?;
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
