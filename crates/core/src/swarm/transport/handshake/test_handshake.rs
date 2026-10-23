//! The handshake on the dummy transport (#945, #947): crossing offers in every signaling
//! phase, recorded-path replies, answers bound to their offer, and step-failure classification.

use super::IncomingOfferSlot;
use crate::dht::delivery::RouteStage;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::ConnectNodeSend;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;
use crate::message::PayloadSender;
use crate::message::SignalingPath;
use crate::swarm::callback::SwarmTransportCallback;
use crate::swarm::transport::pending::Handshake;
use crate::swarm::transport::pending::OfferOwner;
use crate::swarm::transport::pending::OfferPhase;
use crate::swarm::transport::OfferReply;
use crate::swarm::SwarmBuilder;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_until_result;
use crate::tests::default::Node;
use crate::tests::default::TEST_ICE_SERVERS;
use crate::tests::fixed_secret_keys;
use crate::tests::manually_establish_connection;
use crate::tests::TEST_NETWORK_ID;

/// A ring node with one successor and no fingers, so it links only to the neighbours it is
/// given and stabilization dials no chord across the ring.
fn ring_node(key: SecretKey) -> Result<Node> {
    let session = crate::delegation::DelegateeKey::new_with_seckey(&key)?;
    Ok(Node::build(
        SwarmBuilder::new(
            TEST_NETWORK_ID,
            TEST_ICE_SERVERS,
            Box::new(crate::storage::MemStorage::new()),
            session,
        )
        .dht_succ_max(1)
        .dht_finger_table_size(0)
        .dht_virtual_nodes(0),
    ))
}

/// `N` ring nodes over the fixed identities, in ascending DID order.
fn ring_nodes<const N: usize>() -> Result<[Node; N]> {
    let keys = fixed_secret_keys::<N>()?;
    let nodes = keys.map(ring_node);
    let nodes = nodes.into_iter().collect::<Result<Vec<_>>>()?;
    nodes
        .try_into()
        .map_err(|_| Error::InvalidMessage("ring node count".to_string()))
}

/// Link `pairs` in order, each settling before the next.
async fn link(pairs: &[(&Node, &Node)], nodes: &[&Node]) {
    for (left, right) in pairs {
        manually_establish_connection(&left.swarm, &right.swarm).await;
        wait_for_msgs(nodes.iter().copied()).await;
    }
}

/// Whether `node` holds no record of `peer`, admitted or handshaking.
fn unlinked(node: &Node, peer: Did) -> Result<bool> {
    Ok(node.swarm.transport.get_connection(peer).is_none()
        && !node.swarm.has_unadmitted_connection(peer)?)
}

/// The signaling state of `node`'s pending generation for `peer`, if any.
fn handshake_of(node: &Node, peer: Did) -> Result<Option<Handshake>> {
    let lifecycles = node.swarm.transport.peer_lifecycles()?;
    Ok(lifecycles
        .pending_attempt(peer)
        .and_then(|attempt| lifecycles.handshake(attempt).cloned()))
}

/// The hops that delivered to `node` a `ConnectNodeSend` signed by `origin`, draining its inbox.
async fn offer_hops(node: &Node, origin: Did) -> Vec<Did> {
    let mut hops = Vec::new();
    while let Some(payload) = node.try_listen_once().await {
        let is_offer = payload
            .transaction
            .data::<Message>()
            .is_ok_and(|message| matches!(message, Message::ConnectNodeSend(_)));
        if is_offer && payload.transaction.signer() == origin {
            hops.push(payload.signer());
        }
    }
    hops
}

/// `from`'s offer to `to`, as a manual payload re-addressed to `to` over `path`.
async fn offer_over(from: &Node, to: &Node, path: SignalingPath) -> Result<MessagePayload> {
    let (_, mut payload) = from.swarm.offer_connection(to.did()).await?;
    payload.relay.next_hop = to.did();
    payload.relay = payload.relay.with_signaling_path(path);
    Ok(payload)
}

/// A crossing open whose winning offer is lost still ends with the edge on both ends, and the
/// counter-offer retraces the loser's recorded path rather than the winner's DHT route.
///
/// ```text
/// ring  n1 → n2 → n3 → n4 → n1      (d(n1) < d(n2) < d(n3) < d(n4); links = ring edges)
/// P₀ ≡ quiescent ∧ no record between n1 and n3 on either side
/// n1 sends an overlay offer to n3 that is lost   (n1: Offering(Overlay, Sent))
/// n3 offers to n1 over the DHT, crossing relay r
/// ⊢ ◇(admitted(n1, n3) ∧ admitted(n3, n1)), n1's own generation admitted
/// W ≡ the counter-offer reached n3 from r, and r is not n1's DHT first hop toward n3
/// ```
#[tokio::test]
async fn test_crossing_offers_with_lost_winner_offer_end_admitted() -> Result<()> {
    let [node1, node2, node3, node4] = ring_nodes::<4>()?;
    let nodes = [&node1, &node2, &node3, &node4];
    link(
        &[
            (&node1, &node2),
            (&node2, &node3),
            (&node3, &node4),
            (&node4, &node1),
        ],
        &nodes,
    )
    .await;
    assert!(
        unlinked(&node1, node3.did())? && unlinked(&node3, node1.did())?,
        "P₀: no record between n1 and n3 before the crossing open"
    );

    let (winner, offer) = node1
        .swarm
        .transport
        .prepare_connection_offer_with_attempt(
            node3.did(),
            SwarmTransportCallback::new(node1.swarm.transport.clone()),
        )
        .await?;
    node1
        .swarm
        .transport
        .mark_offer_sent(winner, &offer, uuid::Uuid::new_v4())?;
    node3.swarm.connect(node1.did()).await?;

    wait_until_result("n1 and n3 admit each other", || {
        Ok(node1.swarm.is_peer_admitted(node3.did())?
            && node3.swarm.is_peer_admitted(node1.did())?)
    })
    .await?;
    assert_eq!(
        node1.swarm.transport.active_attempt(node3.did())?,
        Some(winner),
        "the edge is n1's own generation: its offer prevailed"
    );

    let crossed = offer_hops(&node1, node3.did()).await;
    let returned = offer_hops(&node3, node1.did()).await;
    let [relay] = crossed[..] else {
        return Err(Error::InvalidMessage(format!(
            "n3's offer hops {crossed:?}"
        )));
    };
    assert_eq!(
        returned,
        vec![relay],
        "the counter-offer retraced n3's offer"
    );
    let dht_hop = node1
        .dht()
        .delivery_step(node3.did(), RouteStage::TOWARD, |peer| {
            node1.swarm.transport.get_connection(peer).is_some()
        })?
        .map(|hop| hop.peer);
    assert_ne!(
        dht_hop,
        Some(relay),
        "n1's DHT route would have crossed another relay"
    );
    wait_for_msgs(nodes).await;
    Ok(())
}

/// An offer still being created yields to a crossing offer in both DID orders: nothing has
/// crossed yet, so there is no tie. The creating generation is released and the crossing offer
/// is answered.
#[tokio::test]
async fn test_creating_offer_yields_in_both_did_orders() -> Result<()> {
    let [smaller, larger] = ring_nodes::<2>()?;
    for (local, remote) in [(&smaller, &larger), (&larger, &smaller)] {
        let (creating, _) = local
            .swarm
            .transport
            .prepare_connection_offer_with_attempt(
                remote.did(),
                SwarmTransportCallback::new(local.swarm.transport.clone()),
            )
            .await?;
        assert_eq!(
            handshake_of(local, remote.did())?,
            Some(Handshake::offering(OfferOwner::Overlay))
        );
        let (remote_attempt, offer) = remote.swarm.offer_connection(local.did()).await?;
        let reply = local.swarm.answer_offer(offer).await?;
        let OfferReply::Answer(_) = reply else {
            return Err(Error::InvalidMessage("expected an answer".to_string()));
        };
        assert!(!local
            .swarm
            .transport
            .is_current_connection_attempt(creating)?);
        assert_eq!(
            handshake_of(local, remote.did())?,
            Some(Handshake::Answering)
        );
        assert!(
            remote
                .swarm
                .cancel_pending_connection_attempt(remote_attempt)
                .await?
        );
        let answering = local
            .swarm
            .transport
            .pending_attempt(remote.did())?
            .ok_or(Error::SwarmMissTransport(remote.did()))?;
        assert!(
            local
                .swarm
                .transport
                .cancel_pending_connection(answering)
                .await?
        );
    }
    Ok(())
}

/// An offer carried out of band prevails over a sent overlay offer in both DID orders: a node
/// that dialed a peer over the DHT, with an offer that was never delivered, still answers the
/// peer's HTTP handshake instead of sending that offer back, which the peer could only answer
/// through the seed's gated `acceptAnswer`. The joining peer, meeting the overlay offer while
/// its own is in flight, refuses it, so exactly one side yields (`Law (Tie-break)`).
///
/// ```text
/// seed s, joining peer j, either DID order; no record between them
/// s dialed j over the DHT and the offer was lost   (s: Offering(Overlay, Sent))
/// j offers to s out of band, an HTTP join          (j: Offering(External, Sent))
/// ⊢ j refuses s's overlay offer; s answers j's offer, releasing its own generation;
///   ◇(admitted(s, j) ∧ admitted(j, s)), the edge being j's generation
/// ```
#[tokio::test]
async fn test_out_of_band_offer_prevails_over_a_sent_overlay_offer() -> Result<()> {
    for seed_is_smaller in [true, false] {
        let [smaller, larger] = ring_nodes::<2>()?;
        let (seed, joining) = if seed_is_smaller {
            (&smaller, &larger)
        } else {
            (&larger, &smaller)
        };
        let (lost, lost_offer) = seed
            .swarm
            .transport
            .prepare_connection_offer_with_attempt(
                joining.did(),
                SwarmTransportCallback::new(seed.swarm.transport.clone()),
            )
            .await?;
        seed.swarm
            .transport
            .mark_offer_sent(lost, &lost_offer, uuid::Uuid::new_v4())?;

        let (join, offer) = joining.swarm.offer_connection(seed.did()).await?;
        assert!(
            matches!(
                joining
                    .swarm
                    .transport
                    .reconcile_incoming_offer_peer(seed.did(), OfferOwner::Overlay)
                    .await,
                Err(Error::AlreadyConnected)
            ),
            "the joining peer refuses the overlay offer while its own is in flight"
        );
        let OfferReply::Answer(answer) = seed.swarm.answer_offer(offer).await? else {
            return Err(Error::InvalidMessage(
                "the seed sent its lost overlay offer back".to_string(),
            ));
        };
        assert!(
            !seed.swarm.transport.is_current_connection_attempt(lost)?,
            "the seed released its own generation"
        );
        joining.swarm.accept_answer_for(join, answer).await?;
        wait_until_result("the seed and the joining peer admit each other", || {
            Ok(seed.swarm.is_peer_admitted(joining.did())?
                && joining.swarm.is_peer_admitted(seed.did())?)
        })
        .await?;
        assert_eq!(
            joining.swarm.transport.active_attempt(seed.did())?,
            Some(join.0),
            "the edge is the joining peer's generation"
        );
    }
    Ok(())
}

/// An answered offer, and an answering generation, refuse a delayed copy of a crossing offer
/// and never counter-offer.
#[tokio::test]
async fn test_answered_and_answering_generations_refuse_a_delayed_offer() -> Result<()> {
    let [smaller, larger] = ring_nodes::<2>()?;
    let (sent, offer) = smaller
        .swarm
        .transport
        .prepare_connection_offer_with_attempt(
            larger.did(),
            SwarmTransportCallback::new(smaller.swarm.transport.clone()),
        )
        .await?;
    let tx_id = uuid::Uuid::new_v4();
    smaller
        .swarm
        .transport
        .mark_offer_sent(sent, &offer, tx_id)?;
    assert!(smaller
        .swarm
        .transport
        .peer_lifecycles()?
        .mark_offer_answered(sent, Some(tx_id)));

    let (_, delayed) = larger.swarm.offer_connection(smaller.did()).await?;
    assert!(matches!(
        smaller.swarm.answer_offer(delayed).await,
        Err(Error::AlreadyConnected)
    ));
    assert!(smaller
        .swarm
        .transport
        .is_current_connection_attempt(sent)?);

    let [first, second] = ring_nodes::<2>()?;
    let (_, offer) = first.swarm.offer_connection(second.did()).await?;
    let duplicate = first
        .swarm
        .transport
        .protocol_egress()
        .originate(
            Message::ConnectNodeSend(ConnectNodeSend {
                sdp: offer
                    .transaction
                    .data::<Message>()
                    .and_then(|message| match message {
                        Message::ConnectNodeSend(offer) => Ok(offer.sdp),
                        _ => Err(Error::InvalidMessage("not an offer".to_string())),
                    })?,
                dht_protocol_mode: first.swarm.dht_protocol_mode(),
                relay_custody: first.swarm.transport.dht.relay_custody(),
            }),
            second.did(),
            Some(second.did()),
        )
        .await?;
    assert!(second.swarm.answer_offer(offer).await?.answer().is_some());
    assert!(matches!(
        second.swarm.answer_offer(duplicate).await,
        Err(Error::AlreadyConnected)
    ));
    Ok(())
}

/// An offer whose recorded path cannot be retraced from here reserves nothing, so the peer's
/// next offer over a retraceable path is answered and completes the edge.
#[tokio::test]
async fn test_unretraceable_offer_reserves_nothing_and_a_retry_succeeds() -> Result<()> {
    let [node1, node2, node3] = ring_nodes::<3>()?;
    let nodes = [&node1, &node2, &node3];
    link(&[(&node1, &node2), (&node2, &node3)], &nodes).await;
    let stranger: Did = SecretKey::random().address().into();

    let broken = offer_over(
        &node3,
        &node1,
        SignalingPath::outbound().forwarded(stranger)?,
    )
    .await?;
    let Message::ConnectNodeSend(offer) = broken.transaction.data::<Message>()? else {
        return Err(Error::InvalidMessage("not an offer".to_string()));
    };
    assert!(matches!(
        node1
            .swarm
            .transport
            .answer_signaled_offer(SwarmTransportCallback::new(node1.swarm.transport.clone()), &broken, &offer)
            .await,
        Err(Error::RelayPathHopUnlinked { hop }) if hop == stranger
    ));
    assert!(
        unlinked(&node1, node3.did())?,
        "the refused offer reserved nothing"
    );
    let pending = node3
        .swarm
        .transport
        .pending_attempt(node1.did())?
        .ok_or(Error::SwarmMissTransport(node1.did()))?;
    assert!(
        node3
            .swarm
            .transport
            .cancel_pending_connection(pending)
            .await?
    );

    let retry = offer_over(
        &node3,
        &node1,
        SignalingPath::outbound().forwarded(node2.did())?,
    )
    .await?;
    let Message::ConnectNodeSend(offer) = retry.transaction.data::<Message>()? else {
        return Err(Error::InvalidMessage("not an offer".to_string()));
    };
    node1
        .swarm
        .transport
        .answer_signaled_offer(
            SwarmTransportCallback::new(node1.swarm.transport.clone()),
            &retry,
            &offer,
        )
        .await?;
    wait_until_result("n1 and n3 admit each other", || {
        Ok(node1.swarm.is_peer_admitted(node3.did())?
            && node3.swarm.is_peer_admitted(node1.did())?)
    })
    .await?;
    wait_for_msgs(nodes).await;
    Ok(())
}

/// An answer applies only to the generation whose offer left under the transaction it echoes:
/// a late answer to a released generation is dropped and leaves the newer one untouched.
#[tokio::test]
async fn test_late_answer_is_not_applied_to_a_newer_generation() -> Result<()> {
    let [node1, node2] = ring_nodes::<2>()?;
    let transport = &node1.swarm.transport;
    let (released, first_offer) = transport
        .prepare_connection_offer_with_attempt(
            node2.did(),
            SwarmTransportCallback::new(node1.swarm.transport.clone()),
        )
        .await?;
    let first_tx = uuid::Uuid::new_v4();
    transport.mark_offer_sent(released, &first_offer, first_tx)?;
    assert!(transport.cancel_unadmitted_connection(released).await?);
    let (newer, second_offer) = transport
        .prepare_connection_offer_with_attempt(
            node2.did(),
            SwarmTransportCallback::new(node1.swarm.transport.clone()),
        )
        .await?;
    let second_tx = uuid::Uuid::new_v4();
    transport.mark_offer_sent(newer, &second_offer, second_tx)?;

    let OfferReply::Answer(late) = node2
        .swarm
        .transport
        .answer_remote_connection(
            node1.did(),
            SwarmTransportCallback::new(node2.swarm.transport.clone()),
            &first_offer,
        )
        .await?
    else {
        return Err(Error::InvalidMessage("expected an answer".to_string()));
    };
    assert!(
        !transport
            .accept_signaled_answer(node2.did(), first_tx, &late.report)
            .await?
    );
    assert_eq!(
        handshake_of(&node1, node2.did())?,
        Some(Handshake::Offering {
            owner: OfferOwner::Overlay,
            phase: OfferPhase::Sent {
                sdp: second_offer.sdp,
                tx_id: second_tx,
            },
        }),
        "the newer generation is untouched"
    );
    Ok(())
}

/// Releasing a generation drops its signaling state with it, so a later generation for the
/// same peer starts afresh: no stale sent offer can be countered (`Invariant (Signaling)`).
#[tokio::test]
async fn test_released_generation_drops_its_signaling_state() -> Result<()> {
    let [node1, node2] = ring_nodes::<2>()?;
    let transport = &node1.swarm.transport;
    let (released, offer) = transport
        .prepare_connection_offer_with_attempt(
            node2.did(),
            SwarmTransportCallback::new(node1.swarm.transport.clone()),
        )
        .await?;
    transport.mark_offer_sent(released, &offer, uuid::Uuid::new_v4())?;
    assert!(transport.cancel_unadmitted_connection(released).await?);
    assert_eq!(handshake_of(&node1, node2.did())?, None);

    let (fresh, _) = transport
        .prepare_connection_offer_with_attempt(
            node2.did(),
            SwarmTransportCallback::new(node1.swarm.transport.clone()),
        )
        .await?;
    assert_ne!(fresh, released);
    assert_eq!(
        handshake_of(&node1, node2.did())?,
        Some(Handshake::offering(OfferOwner::Overlay))
    );
    Ok(())
}

/// A step failure is `ConnectionAttemptSuperseded` only when the peer's own handshake took the
/// slot, and `ConnectionAttemptCancelled` when anything else released it.
///
/// On native WebRTC a crossing offer closes a creating offer while its ICE gathering is in
/// flight, and the gathering fails with `Local connection closed` (#945); the dummy transport
/// completes an offer at once, so the release is applied here between the step and its
/// classification.
#[tokio::test]
async fn test_step_failure_classification() -> Result<()> {
    let [node1, node2] = ring_nodes::<2>()?;
    let closed = || {
        Error::Transport(
            rings_transport::error::Error::WebrtcLocalSdpGenerationError(
                "Local connection closed".to_string(),
            ),
        )
    };

    let (cancelled, _) = node1.swarm.offer_connection(node2.did()).await?;
    assert!(
        node1
            .swarm
            .cancel_pending_connection_attempt(cancelled)
            .await?
    );
    let failure = node1
        .swarm
        .transport
        .classify_step_failure(cancelled.0, closed(), "creating connection offer")
        .await?;
    assert!(
        matches!(failure, Error::ConnectionAttemptCancelled { peer, .. } if peer == node2.did()),
        "{failure:?}"
    );

    let (creating, _) = node1
        .swarm
        .transport
        .prepare_connection_offer_with_attempt(
            node2.did(),
            SwarmTransportCallback::new(node1.swarm.transport.clone()),
        )
        .await?;
    let (_, crossing) = node2.swarm.offer_connection(node1.did()).await?;
    assert!(node1.swarm.answer_offer(crossing).await?.answer().is_some());
    let failure = node1
        .swarm
        .transport
        .classify_step_failure(creating, closed(), "creating connection offer")
        .await?;
    assert!(
        matches!(
            failure,
            Error::ConnectionAttemptSuperseded { peer, generation }
                if peer == node2.did() && generation == creating.generation
        ),
        "{failure:?}"
    );
    Ok(())
}

/// A creating offer that yields to a crossing offer, and whose SDP step then fails, reports
/// `ConnectionAttemptSuperseded` as soon as the yield is applied, before the answer is
/// created: the yield hands the slot to the answering generation in one transition, so the
/// classification never reads a vacant slot (`Law (Atomic yield)`, #954).
#[tokio::test]
async fn test_yielded_offer_is_superseded_before_the_answer_is_created() -> Result<()> {
    let [node1, node2] = ring_nodes::<2>()?;
    let transport = &node1.swarm.transport;
    let (creating, _) = transport
        .prepare_connection_offer_with_attempt(
            node2.did(),
            SwarmTransportCallback::new(node1.swarm.transport.clone()),
        )
        .await?;

    let IncomingOfferSlot::Yielded(answering) = transport
        .reconcile_incoming_offer_peer(node2.did(), OfferOwner::Overlay)
        .await?
    else {
        return Err(Error::InvalidMessage(
            "a creating offer yields to a crossing offer".to_string(),
        ));
    };
    assert_ne!(answering, creating);
    assert_eq!(
        handshake_of(&node1, node2.did())?,
        Some(Handshake::Answering)
    );

    let failure = transport
        .classify_step_failure(
            creating,
            Error::Transport(
                rings_transport::error::Error::WebrtcLocalSdpGenerationError(
                    "Local connection closed".to_string(),
                ),
            ),
            "creating connection offer",
        )
        .await?;
    assert!(
        matches!(
            failure,
            Error::ConnectionAttemptSuperseded { peer, generation }
                if peer == node2.did() && generation == creating.generation
        ),
        "{failure:?}"
    );
    Ok(())
}

/// A carrier whose path does not match its message is refused at inbound validation: a path
/// on a message that is not connect signaling, and a relayed offer without one.
#[tokio::test]
async fn test_inbound_refuses_a_path_on_the_wrong_message() -> Result<()> {
    let [node1, node2] = ring_nodes::<2>()?;
    let mut custom = node1
        .swarm
        .transport
        .protocol_egress()
        .originate(
            Message::custom(b"pathless only")?,
            node2.did(),
            Some(node2.did()),
        )
        .await?;
    custom.relay = custom.relay.with_signaling_path(SignalingPath::outbound());
    let (_, mut offer) = node1.swarm.offer_connection(node2.did()).await?;
    offer.relay.next_hop = node2.did();
    for refused in [custom, offer] {
        let wire = refused.to_wire()?;
        assert!(matches!(
            crate::swarm::callback::prepare_transport_frame_lane_for_test(TEST_NETWORK_ID, &wire),
            Err(Error::RelaySignalingPathMismatch)
        ));
    }
    Ok(())
}
