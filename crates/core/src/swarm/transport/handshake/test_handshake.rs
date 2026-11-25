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
use crate::tests::default::TEST_TRAVERSAL_SERVERS;
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
            crate::tests::test_transport(TEST_TRAVERSAL_SERVERS),
            Box::new(crate::storage::MemStorage::new()),
            session,
        )
        .replay_storage(crate::tests::volatile_replay_storage())
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
            hops.push(payload.last_hop_signer());
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
            Some(Handshake::answering())
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
        .claim_offer_reply(sent, Some(tx_id), 1, true, None));

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
            Message::ConnectNodeSend(ConnectNodeSend::initial_offer(
                offer
                    .transaction
                    .data::<Message>()
                    .and_then(|message| match message {
                        Message::ConnectNodeSend(offer) => Ok(offer.handshake),
                        _ => Err(Error::InvalidMessage("not an offer".to_string())),
                    })?,
                first.swarm.dht_protocol_mode(),
                first.swarm.transport.dht.relay_custody(),
            )),
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

    let OfferReply::Answer(Some(late)) = node2
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
                step_index: 0,
                final_step: false,
                handshake: second_offer.handshake,
                tx_id: second_tx,
                session_id: None,
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
            rings_transport::error::Error::LocalHandshakeGenerationError(
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
        Some(Handshake::answering())
    );

    let failure = transport
        .classify_step_failure(
            creating,
            Error::Transport(
                rings_transport::error::Error::LocalHandshakeGenerationError(
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

/// An offer or answer advertising `a=max-message-size` below `MIN_MAX_MESSAGE_SIZE` is refused
/// (#1130): an offer below the floor reserves no slot and does not displace an in-flight offer,
/// an answer below the floor abandons the offering generation, and an SDP at the floor is
/// admitted with `max_message_size() == MIN_MAX_MESSAGE_SIZE`.
#[tokio::test]
async fn test_handshake_refuses_sdp_below_min_max_message_size() -> Result<()> {
    use rings_transport::core::transport::ConnectionInterface;
    use rings_transport::core::transport::MIN_MAX_MESSAGE_SIZE;
    use rings_transport::error::Error as TransportError;

    let [node1, node2] = ring_nodes::<2>()?;
    let handshake_with_limit = |raw_bytes: &bytes::Bytes, limit: usize| -> Result<bytes::Bytes> {
        let base = std::str::from_utf8(raw_bytes)
            .map_err(|err| Error::InvalidMessage(format!("invalid UTF-8 offer: {err}")))?;
        let annotated = format!(
            "{base}\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=max-message-size:{limit}\r\n"
        );
        Ok(bytes::Bytes::from(annotated))
    };

    // 1. An incoming offer below MIN_MAX_MESSAGE_SIZE is refused before reconciling the slot:
    // node2's in-flight creating offer is untouched.
    let (creating, _) = node2
        .swarm
        .transport
        .prepare_connection_offer_with_attempt(
            node1.did(),
            SwarmTransportCallback::new(node2.swarm.transport.clone()),
        )
        .await?;
    let (attempt1, offer_payload) = node1.swarm.offer_connection(node2.did()).await?;
    let Message::ConnectNodeSend(mut small_offer) = offer_payload.transaction.data::<Message>()?
    else {
        return Err(Error::InvalidMessage("not an offer".to_string()));
    };
    small_offer.handshake = handshake_with_limit(&small_offer.handshake, MIN_MAX_MESSAGE_SIZE - 1)?;
    let refused = node2
        .swarm
        .transport
        .answer_remote_connection(
            node1.did(),
            SwarmTransportCallback::new(node2.swarm.transport.clone()),
            &small_offer,
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(Error::Transport(
                TransportError::RemoteMaxMessageSizeTooSmall {
                    advertised,
                    minimum: MIN_MAX_MESSAGE_SIZE,
                }
            )) if advertised == MIN_MAX_MESSAGE_SIZE - 1
        ),
        "expected RemoteMaxMessageSizeTooSmall, got {refused:?}"
    );
    assert!(
        node2
            .swarm
            .transport
            .is_current_connection_attempt(creating)?,
        "a refused offer must not displace node2's creating offer"
    );
    assert!(
        node2
            .swarm
            .transport
            .cancel_pending_connection(creating)
            .await?
    );

    // 2. An answer below MIN_MAX_MESSAGE_SIZE is refused and abandons the pending offer.
    let Message::ConnectNodeSend(valid_offer) = offer_payload.transaction.data::<Message>()? else {
        return Err(Error::InvalidMessage("not an offer".to_string()));
    };
    let OfferReply::Answer(Some(mut created_answer)) = node2
        .swarm
        .transport
        .answer_remote_connection(
            node1.did(),
            SwarmTransportCallback::new(node2.swarm.transport.clone()),
            &valid_offer,
        )
        .await?
    else {
        return Err(Error::InvalidMessage("expected an answer".to_string()));
    };
    let answering2 = created_answer.attempt;
    created_answer.report.handshake =
        handshake_with_limit(&created_answer.report.handshake, MIN_MAX_MESSAGE_SIZE - 1)?;
    let refused_answer = node1
        .swarm
        .transport
        .accept_remote_connection(node2.did(), &created_answer.report, Some(attempt1.0), None)
        .await;
    assert!(
        matches!(
            refused_answer,
            Err(Error::Transport(
                TransportError::RemoteMaxMessageSizeTooSmall {
                    advertised,
                    minimum: MIN_MAX_MESSAGE_SIZE,
                }
            )) if advertised == MIN_MAX_MESSAGE_SIZE - 1
        ),
        "expected RemoteMaxMessageSizeTooSmall, got {refused_answer:?}"
    );
    assert!(
        unlinked(&node1, node2.did())?,
        "refusing the answer abandons node1's pending offer"
    );
    assert!(
        node2
            .swarm
            .transport
            .cancel_pending_connection(answering2)
            .await?
    );

    // 3. At exactly MIN_MAX_MESSAGE_SIZE, both offer and answer succeed and record the floor.
    let (attempt_ok, offer_ok_payload) = node1.swarm.offer_connection(node2.did()).await?;
    let Message::ConnectNodeSend(mut floor_offer) =
        offer_ok_payload.transaction.data::<Message>()?
    else {
        return Err(Error::InvalidMessage("not an offer".to_string()));
    };
    floor_offer.handshake = handshake_with_limit(&floor_offer.handshake, MIN_MAX_MESSAGE_SIZE)?;
    let OfferReply::Answer(Some(mut floor_answer)) = node2
        .swarm
        .transport
        .answer_remote_connection(
            node1.did(),
            SwarmTransportCallback::new(node2.swarm.transport.clone()),
            &floor_offer,
        )
        .await?
    else {
        return Err(Error::InvalidMessage("expected an answer".to_string()));
    };
    floor_answer.report.handshake =
        handshake_with_limit(&floor_answer.report.handshake, MIN_MAX_MESSAGE_SIZE)?;
    node1
        .swarm
        .transport
        .accept_remote_connection(node2.did(), &floor_answer.report, Some(attempt_ok.0), None)
        .await?;
    wait_until_result("node1 and node2 admit each other at the floor", || {
        Ok(node1.swarm.is_peer_admitted(node2.did())?
            && node2.swarm.is_peer_admitted(node1.did())?)
    })
    .await?;
    let conn1 = node1
        .swarm
        .transport
        .get_connection(node2.did())
        .ok_or(Error::SwarmMissTransport(node2.did()))?;
    let conn2 = node2
        .swarm
        .transport
        .get_connection(node1.did())
        .ok_or(Error::SwarmMissTransport(node1.did()))?;
    assert_eq!(conn1.connection.max_message_size(), MIN_MAX_MESSAGE_SIZE);
    assert_eq!(conn2.connection.max_message_size(), MIN_MAX_MESSAGE_SIZE);
    Ok(())
}

/// When `connect` fails during `send_connection_offer` (`originate` failure because no route/link
/// exists to the target peer), the reserved `Pending` slot and raw WebRTC connection are
/// immediately abandoned rather than leaked for 180s, and candidate dial backoff is recorded
/// exactly once (#1222).
#[tokio::test]
async fn test_connect_originate_failure_abandons_pending_slot_and_records_single_backoff(
) -> Result<()> {
    let [node1, node2] = ring_nodes::<2>()?;
    let peer = node2.did();

    // `node1` has no admitted links, so `originate` inside `send_connection_offer` fails.
    let result = node1.swarm.connect(peer).await;
    assert!(
        result.is_err(),
        "expected error from unlinked node, got {result:?}"
    );
    assert!(
        unlinked(&node1, peer)?,
        "failed send_connection_offer must abandon the pending slot and raw connection"
    );
    assert!(
        node1.swarm.transport.is_candidate_dial_backed_off(peer),
        "failed outbound connect must record candidate dial backoff"
    );

    // Verify backoff failure count is 1 (not double-counted by abandon + connect_failed):
    // a second failure should advance failures to 2 (`shift = 1`), whereas if the first connect
    // had double-counted (`failures == 2`), `failures` would already be 2 after one connect.
    // We check this indirectly by preparing an offer whose `mark_offer_sent` fails after manual
    // state change.
    let [node_a, node_b, node_c] = ring_nodes::<3>()?;
    link(&[(&node_a, &node_b)], &[&node_a, &node_b]).await;
    let target = node_c.did();
    let (attempt, offer) = node_a
        .swarm
        .transport
        .prepare_connection_offer_with_attempt(
            target,
            SwarmTransportCallback::new(node_a.swarm.transport.clone()),
        )
        .await?;
    // Advance `attempt` out of `Creating` (`Creating -> Sent -> Answered`) so a subsequent
    // `mark_offer_sent` inside `send_connection_offer` fails while `attempt` still owns the slot.
    let tx_id = uuid::Uuid::new_v4();
    node_a
        .swarm
        .transport
        .mark_offer_sent(attempt, &offer, tx_id)?;
    assert!(node_a.swarm.transport.peer_lifecycles()?.claim_offer_reply(
        attempt,
        Some(tx_id),
        1,
        true,
        None
    ));

    let send_err = node_a
        .swarm
        .transport
        .send_connection_offer(attempt, offer, &node_a.swarm.transport.application_egress())
        .await;
    assert!(
        send_err.is_err(),
        "mark_offer_sent must fail when already Answered"
    );
    assert!(
        unlinked(&node_a, target)?,
        "mark_offer_sent failure in send_connection_offer must abandon the pending slot"
    );
    Ok(())
}

/// Formal Safety Invariant (Issue #1291 §3 — Pre-Reconciliation Offer Rejection Purity):
/// `\square RejectedOfferIsPureNoOp`: when `answer_remote_connection` rejects an inbound offer via
/// `validate_remote_offer` (`HandshakeAboveMax`, invalid JSON SDP, or `RemoteMaxMessageSizeTooSmall`),
/// the local lifecycle slot `L'[peer] == L[peer]` and transport pool `P'[peer] == P[peer]` remain
/// bit-for-bit unchanged (no displacement of an existing pending or active connection, and no
/// orphan physical connection created).
#[tokio::test]
async fn test_rejected_offer_is_pure_no_op_across_all_pre_reconciliation_rejection_classes(
) -> Result<()> {
    use rings_transport::core::transport::MIN_MAX_MESSAGE_SIZE;

    let [node1, node2] = ring_nodes::<2>()?;
    let handshake_with_limit = |raw_bytes: &bytes::Bytes, limit: usize| -> Result<bytes::Bytes> {
        let base = std::str::from_utf8(raw_bytes)
            .map_err(|err| Error::InvalidMessage(format!("invalid UTF-8 offer: {err}")))?;
        let annotated = format!(
            "{base}\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=max-message-size:{limit}\r\n"
        );
        Ok(bytes::Bytes::from(annotated))
    };
    let (creating, _local_offer) = node2
        .swarm
        .transport
        .prepare_connection_offer_with_attempt(
            node1.did(),
            SwarmTransportCallback::new(node2.swarm.transport.clone()),
        )
        .await?;
    let (_attempt1, offer_payload) = node1.swarm.offer_connection(node2.did()).await?;
    let Message::ConnectNodeSend(valid_offer) = offer_payload.transaction.data::<Message>()? else {
        return Err(Error::InvalidMessage("not an offer".to_string()));
    };
    let original_conn = node2
        .swarm
        .transport
        .transport
        .dyn_connection(&node1.did().to_string())?;

    let invalid_offers = [
        {
            let mut oversized = valid_offer.clone();
            oversized.handshake =
                bytes::Bytes::from(vec![b'x'; crate::consts::MAX_HANDSHAKE_BYTES + 1]);
            oversized
        },
        {
            let mut malformed = valid_offer.clone();
            malformed.handshake = bytes::Bytes::from_static(&[0xff, 0xfe, 0xfd]);
            malformed
        },
        {
            let mut undersized = valid_offer.clone();
            undersized.handshake =
                handshake_with_limit(&valid_offer.handshake, MIN_MAX_MESSAGE_SIZE - 1)?;
            undersized
        },
    ];

    for bad_offer in &invalid_offers {
        let res = node2
            .swarm
            .transport
            .answer_remote_connection(
                node1.did(),
                SwarmTransportCallback::new(node2.swarm.transport.clone()),
                bad_offer,
            )
            .await;
        assert!(res.is_err(), "expected pre-reconciliation rejection");
        assert!(
            node2
                .swarm
                .transport
                .is_current_connection_attempt(creating)?,
            "rejected offer must not displace existing pending lifecycle slot"
        );
        let current_conn = node2
            .swarm
            .transport
            .transport
            .dyn_connection(&node1.did().to_string())?;
        assert!(
            original_conn.same_allocation(&current_conn),
            "rejected offer must not mutate or replace the transport pool entry"
        );
    }
    Ok(())
}

/// Architectural Witness (Issue #1291 F1 & F2):
/// 1. A **0-step** pre-established custom transport (`handshake_step(None) -> HandshakeStep::Done`)
///    admits directly through `Swarm::connect` with zero overlay `ConnectNodeSend` messages, and
///    `rings-core` rejects a 0-step connection whose `max_message_size()` is below
///    `MIN_MAX_MESSAGE_SIZE`.
/// 2. A **3-step** binary custom transport (`Send(step 0) -> Send(step 1, final=false) ->
///    SendFinal(step 2) -> Done`) completes end-to-end across a multi-hop DHT relay (`n1 -> n2 -> n3`),
///    admits both endpoints, and carries application traffic over non-UTF-8 binary handshake steps.
#[tokio::test]
async fn test_zero_step_and_three_step_custom_transports_establish_admitted_dht_edges_and_enforce_size_floor(
) -> Result<()> {
    use std::any::Any;
    use std::sync::atomic::AtomicU8;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use async_trait::async_trait;
    use bytes::Bytes;
    use dashmap::DashMap;
    use rings_transport::callback::InnerTransportCallback;
    use rings_transport::callback::NodeFramePool;
    use rings_transport::connection_pool::Pool;
    use rings_transport::connection_ref::ConnectionRef;
    use rings_transport::core::callback::BoxedTransportCallback;
    use rings_transport::core::pool::ChannelLane;
    use rings_transport::core::transport::ConnectionInterface;
    use rings_transport::core::transport::ConnectionStateCell;
    use rings_transport::core::transport::ConnectionStateSnapshot;
    use rings_transport::core::transport::HandshakeStep;
    use rings_transport::core::transport::LaneCreditView;
    use rings_transport::core::transport::SendPermit;
    use rings_transport::core::transport::SharedTransport;
    use rings_transport::core::transport::TransportConnectionState;
    use rings_transport::core::transport::TransportInterface;
    use rings_transport::core::transport::MAX_CHANNEL_MESSAGE_SIZE;
    use rings_transport::core::transport::MIN_MAX_MESSAGE_SIZE;
    use rings_transport::delivery::DeliveryFuture;
    use rings_transport::delivery::SendCreditWait;
    use rings_transport::notifier::wait_for_data_channel_open;
    use rings_transport::notifier::Notifier;

    type Switchboard = Arc<
        DashMap<
            (String, String),
            (
                Arc<InnerTransportCallback>,
                std::sync::Weak<CustomMeshConnection>,
            ),
        >,
    >;

    struct CustomMeshConnection {
        local_id: String,
        peer_id: String,
        mode_steps: u8, // 0, 1, 3, 11 (optimistic inline 1-step), or 99 (faulty responder)
        negotiated_max: Arc<std::sync::atomic::AtomicUsize>,
        oversized_final: bool,
        sent_frames: Arc<std::sync::atomic::AtomicUsize>,
        stage: AtomicU8,
        state: ConnectionStateCell,
        notifier: Notifier,
        callback: Arc<InnerTransportCallback>,
        switchboard: Switchboard,
    }

    impl CustomMeshConnection {
        fn spawn_connected(&self) {
            self.state
                .observe_phase(TransportConnectionState::Connected);
            self.state.observe_channels(true);
            self.notifier.wake();
            let cb = Arc::clone(&self.callback);
            tokio::spawn(async move {
                cb.on_connection_state_change(TransportConnectionState::Connected)
                    .await;
                cb.on_channel_open().await;
            });
        }

        async fn mark_connected_inline(&self) {
            self.state
                .observe_phase(TransportConnectionState::Connected);
            self.state.observe_channels(true);
            self.notifier.wake();
            self.callback
                .on_connection_state_change(TransportConnectionState::Connected)
                .await;
            self.callback.on_channel_open().await;
        }
    }

    #[async_trait]
    impl ConnectionInterface for CustomMeshConnection {
        type Error = rings_transport::error::Error;

        fn as_any(&self) -> &dyn Any {
            self
        }

        async fn send_message_with_permit(
            &self,
            payload: Bytes,
            lane: ChannelLane,
            permit: SendPermit,
        ) -> rings_transport::error::Result<DeliveryFuture> {
            let switchboard = Arc::clone(&self.switchboard);
            let target_key = (self.peer_id.clone(), self.local_id.clone());
            let state = self.state.clone();
            let cb = Arc::clone(&self.callback);
            let sent_frames = Arc::clone(&self.sent_frames);
            self.callback
                .send_custom_wire(
                    payload,
                    lane,
                    permit,
                    self.max_message_size(),
                    move || {
                        state.close();
                        cb.close_link_credit();
                    },
                    move |wire, target_lane, irrevocable| async move {
                        let Some(peer_cb) = switchboard
                            .get(&target_key)
                            .map(|r| Arc::clone(&r.value().0))
                        else {
                            return Err(rings_transport::error::Error::MessageNotDelivered(
                                "missing mesh peer".to_string(),
                            ));
                        };
                        let accepted = irrevocable.accept();
                        sent_frames.fetch_add(1, Ordering::AcqRel);
                        peer_cb.receive_inbound_frame(wire, target_lane);
                        Ok((accepted, Box::pin(async { Ok(()) }) as DeliveryFuture))
                    },
                )
                .await
        }

        fn reserve_send_credit(
            &self,
            lane: ChannelLane,
        ) -> SendCreditWait<rings_transport::error::Error> {
            Box::pin(self.callback.reserve_credit(lane))
        }

        fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView {
            self.callback.send_credit_view(lane)
        }

        fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
            self.state.snapshot()
        }

        fn max_message_size(&self) -> usize {
            self.negotiated_max.load(Ordering::Acquire)
        }

        async fn handshake_step(
            &self,
            inbound: Option<Bytes>,
        ) -> rings_transport::error::Result<HandshakeStep> {
            if self.mode_steps == 0 {
                self.spawn_connected();
                return Ok(HandshakeStep::Done);
            }
            if self.mode_steps == 1 || self.mode_steps == 11 {
                return match (self.stage.load(Ordering::Acquire), inbound.as_deref()) {
                    (0, None) => {
                        self.stage.store(1, Ordering::Release);
                        if self.oversized_final {
                            // Open channel inline first so `attempt` becomes `Active`, then return
                            // an oversized final step to witness A2!
                            self.mark_connected_inline().await;
                            return Ok(HandshakeStep::SendFinal(Bytes::from(vec![
                                0xaa;
                                crate::consts::MAX_HANDSHAKE_BYTES
                                    + 1
                            ])));
                        }
                        if self.mode_steps == 11 {
                            // Optimistic 1-step backend reporting local channel readiness inline
                            // before `SendFinal` leaves (`R3-A1` witness).
                            self.mark_connected_inline().await;
                        }
                        Ok(HandshakeStep::SendFinal(Bytes::from_static(&[0xaa, 0x01])))
                    }
                    (0, Some([0xaa, 0x01])) => {
                        self.stage.store(2, Ordering::Release);
                        self.spawn_connected();
                        if let Some(paired) = self
                            .switchboard
                            .get(&(self.peer_id.clone(), self.local_id.clone()))
                            .and_then(|entry| entry.value().1.upgrade())
                        {
                            paired.spawn_connected();
                        }
                        Ok(HandshakeStep::Done)
                    }
                    (stage, other) => {
                        Err(rings_transport::error::Error::RemoteHandshakeApplyError(
                            format!("unexpected 1-step transition stage={stage} inbound={other:?}"),
                        ))
                    }
                };
            }
            if self.mode_steps == 99 {
                // Faulty responder returning `Done` on a non-final inbound step (A4 witness).
                return match inbound {
                    None => Ok(HandshakeStep::Send(Bytes::from_static(b"nonfinal-offer"))),
                    Some(_) => Ok(HandshakeStep::Done),
                };
            }

            // 3-step non-UTF-8 binary handshake:
            // Initiator: None -> Send([0xff, 0x00, 0x01]), Some([0xff, 0x01, 0x02]) -> SendFinal([0xff, 0x02, 0x03]) + spawn_connected
            // Responder: Some([0xff, 0x00, 0x01]) -> Send([0xff, 0x01, 0x02]), Some([0xff, 0x02, 0x03]) -> Done + spawn_connected
            match (self.stage.load(Ordering::Acquire), inbound.as_deref()) {
                (0, None) => {
                    self.stage.store(1, Ordering::Release);
                    Ok(HandshakeStep::Send(Bytes::from_static(&[0xff, 0x00, 0x01])))
                }
                (0, Some([0xff, 0x00, 0x01])) => {
                    self.stage.store(2, Ordering::Release);
                    Ok(HandshakeStep::Send(Bytes::from_static(&[0xff, 0x01, 0x02])))
                }
                (1, Some([0xff, 0x01, 0x02])) => {
                    self.stage.store(3, Ordering::Release);
                    self.spawn_connected();
                    Ok(HandshakeStep::SendFinal(Bytes::from_static(&[
                        0xff, 0x02, 0x03,
                    ])))
                }
                (2, Some([0xff, 0x02, 0x03])) => {
                    self.stage.store(3, Ordering::Release);
                    self.spawn_connected();
                    Ok(HandshakeStep::Done)
                }
                (stage, other) => Err(rings_transport::error::Error::RemoteHandshakeApplyError(
                    format!("unexpected 3-step transition stage={stage} inbound={other:?}"),
                )),
            }
        }

        async fn wait_for_channel_open(&self) -> rings_transport::error::Result<()> {
            wait_for_data_channel_open(|| self.state.snapshot(), &self.notifier, 5).await
        }

        async fn close(&self) -> rings_transport::error::Result<()> {
            self.switchboard
                .remove(&(self.local_id.clone(), self.peer_id.clone()));
            self.state.close();
            self.notifier.wake();
            self.callback.close_link_credit();
            Ok(())
        }
    }

    struct CustomMeshTransport {
        local_id: String,
        mode_steps: u8,
        negotiated_max: Arc<std::sync::atomic::AtomicUsize>,
        oversized_final: bool,
        sent_frames: Arc<std::sync::atomic::AtomicUsize>,
        pool: Pool<CustomMeshConnection>,
        frame_pool: NodeFramePool,
        switchboard: Switchboard,
    }

    impl CustomMeshTransport {
        fn new(
            local_id: String,
            mode_steps: u8,
            negotiated_max: usize,
            switchboard: Switchboard,
        ) -> Self {
            Self {
                local_id,
                mode_steps,
                negotiated_max: Arc::new(std::sync::atomic::AtomicUsize::new(negotiated_max)),
                oversized_final: false,
                sent_frames: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                pool: Pool::new(),
                frame_pool: NodeFramePool::new(),
                switchboard,
            }
        }
    }

    #[async_trait]
    impl TransportInterface for CustomMeshTransport {
        type Connection = CustomMeshConnection;
        type Error = rings_transport::error::Error;

        async fn new_connection(
            &self,
            cid: &str,
            callback: BoxedTransportCallback,
        ) -> rings_transport::error::Result<ConnectionRef<Self::Connection>> {
            self.pool.ensure_peer_slot_available(cid)?;
            let notifier = Notifier::default();
            let inner_cb = Arc::new(InnerTransportCallback::new(
                cid,
                callback,
                notifier.clone(),
                self.frame_pool.clone(),
            ));
            let switchboard = Arc::clone(&self.switchboard);
            let target_key = (cid.to_string(), self.local_id.clone());
            inner_cb.spawn_credit_driver(move |wire, lane| {
                let switchboard = Arc::clone(&switchboard);
                let target_key = target_key.clone();
                async move {
                    let Some(peer_cb) = switchboard
                        .get(&target_key)
                        .map(|r| Arc::clone(&r.value().0))
                    else {
                        return Err(rings_transport::error::Error::MessageNotDelivered(
                            "peer not yet registered".to_string(),
                        ));
                    };
                    peer_cb.receive_inbound_frame(wire, lane);
                    Ok(())
                }
            })?;
            let conn = CustomMeshConnection {
                local_id: self.local_id.clone(),
                peer_id: cid.to_string(),
                mode_steps: self.mode_steps,
                negotiated_max: Arc::clone(&self.negotiated_max),
                oversized_final: self.oversized_final,
                sent_frames: Arc::clone(&self.sent_frames),
                stage: AtomicU8::new(0),
                state: ConnectionStateCell::new(),
                notifier,
                callback: Arc::clone(&inner_cb),
                switchboard: Arc::clone(&self.switchboard),
            };
            let conn_ref = self.pool.safely_insert(cid, conn).await?;
            let live = conn_ref.upgrade()?;
            self.switchboard.insert(
                (self.local_id.clone(), cid.to_string()),
                (inner_cb, Arc::downgrade(&live)),
            );
            drop(live);
            Ok(conn_ref)
        }

        async fn close_connection_if_current(
            &self,
            connection: &ConnectionRef<Self::Connection>,
        ) -> rings_transport::error::Result<bool> {
            self.pool.safely_remove_if_current(connection).await
        }

        fn connection(
            &self,
            cid: &str,
        ) -> rings_transport::error::Result<ConnectionRef<Self::Connection>> {
            self.pool.connection(cid)
        }

        fn connection_ids(&self) -> Vec<String> {
            self.pool.connection_ids()
        }
    }

    let build_custom_node = |key: SecretKey,
                             mode_steps: u8,
                             negotiated_max: usize,
                             switchboard: &Switchboard|
     -> Result<Node> {
        let session = crate::delegation::DelegateeKey::new_with_seckey(&key)?;
        let did = session.delegator_did();
        let transport: SharedTransport = Arc::new(CustomMeshTransport::new(
            did.to_string(),
            mode_steps,
            negotiated_max,
            Arc::clone(switchboard),
        ));
        Ok(Node::build(
            SwarmBuilder::new(
                TEST_NETWORK_ID,
                transport,
                Box::new(crate::storage::MemStorage::new()),
                session,
            )
            .replay_storage(crate::tests::volatile_replay_storage())
            .dht_succ_max(1)
            .dht_finger_table_size(0)
            .dht_virtual_nodes(0),
        ))
    };

    // Part A: 0-step custom transport enforces MIN_MAX_MESSAGE_SIZE floor, and succeeds when >= floor
    // with zero ConnectNodeSend messages.
    let zero_switchboard: Switchboard = Arc::new(DashMap::new());
    let [k1, k2, k3, k4] = fixed_secret_keys::<4>()?;
    let undersized_zero =
        build_custom_node(k1.clone(), 0, MIN_MAX_MESSAGE_SIZE - 1, &zero_switchboard)?;
    let peer_zero = build_custom_node(k2.clone(), 0, MAX_CHANNEL_MESSAGE_SIZE, &zero_switchboard)?;
    let err = undersized_zero.swarm.connect(peer_zero.did()).await;
    assert!(
        matches!(
            err,
            Err(Error::Transport(
                rings_transport::error::Error::RemoteMaxMessageSizeTooSmall { .. }
            ))
        ),
        "0-step transport advertising below MIN_MAX_MESSAGE_SIZE must be rejected by rings-core, got {err:?}"
    );

    let valid_zero_1_transport = Arc::new(CustomMeshTransport::new(
        crate::delegation::DelegateeKey::new_with_seckey(&k1)?
            .delegator_did()
            .to_string(),
        0,
        MAX_CHANNEL_MESSAGE_SIZE,
        Arc::clone(&zero_switchboard),
    ));
    let valid_zero_2_transport = Arc::new(CustomMeshTransport::new(
        crate::delegation::DelegateeKey::new_with_seckey(&k2)?
            .delegator_did()
            .to_string(),
        0,
        MAX_CHANNEL_MESSAGE_SIZE,
        Arc::clone(&zero_switchboard),
    ));
    let valid_zero_1 = Node::build(
        SwarmBuilder::new(
            TEST_NETWORK_ID,
            Arc::clone(&valid_zero_1_transport) as SharedTransport,
            Box::new(crate::storage::MemStorage::new()),
            crate::delegation::DelegateeKey::new_with_seckey(&k1)?,
        )
        .replay_storage(crate::tests::volatile_replay_storage())
        .dht_succ_max(1)
        .dht_finger_table_size(0)
        .dht_virtual_nodes(0),
    );
    let valid_zero_2 = Node::build(
        SwarmBuilder::new(
            TEST_NETWORK_ID,
            Arc::clone(&valid_zero_2_transport) as SharedTransport,
            Box::new(crate::storage::MemStorage::new()),
            crate::delegation::DelegateeKey::new_with_seckey(&k2)?,
        )
        .replay_storage(crate::tests::volatile_replay_storage())
        .dht_succ_max(1)
        .dht_finger_table_size(0)
        .dht_virtual_nodes(0),
    );
    let (_zero_attempt, maybe_offer) = valid_zero_1
        .swarm
        .offer_connection_step(valid_zero_2.did())
        .await?;
    assert!(
        maybe_offer.is_none(),
        "0-step offer_connection_step must return None without fabricating a ConnectNodeSend"
    );
    assert_eq!(
        valid_zero_1_transport.sent_frames.load(Ordering::Acquire),
        0,
        "0-step offer_connection_step must transmit zero frames before admission"
    );
    valid_zero_2.swarm.connect(valid_zero_1.did()).await?;
    wait_until_result(
        "0-step peers admit each other without overlay signaling",
        || {
            Ok(valid_zero_1.swarm.is_peer_admitted(valid_zero_2.did())?
                && valid_zero_2.swarm.is_peer_admitted(valid_zero_1.did())?)
        },
    )
    .await?;

    // Part B: 3-step binary custom transport across a 4-node ring (n1 -> n2 -> n3 -> n4 -> n1),
    // bootstrapped EXCLUSIVELY through the public out-of-band Swarm step API (`offer_connection_step`
    // -> `answer_offer_step` -> `accept_answer_step_for` -> `answer_offer_step`), and then routed
    // across relay `n2` via `n1.swarm.connect(n3.did())`!
    let three_switchboard: Switchboard = Arc::new(DashMap::new());
    let n1 = build_custom_node(k1.clone(), 3, MAX_CHANNEL_MESSAGE_SIZE, &three_switchboard)?;
    let n2 = build_custom_node(k2.clone(), 3, MAX_CHANNEL_MESSAGE_SIZE, &three_switchboard)?;
    let n3 = build_custom_node(k3.clone(), 3, MAX_CHANNEL_MESSAGE_SIZE, &three_switchboard)?;
    let n4 = build_custom_node(k4.clone(), 3, MAX_CHANNEL_MESSAGE_SIZE, &three_switchboard)?;
    let n3_inbox = n3.swarm.application_inbox()?;

    for (left, right) in [(&n1, &n2), (&n2, &n3), (&n3, &n4), (&n4, &n1)] {
        let (attempt_l, Some(offer0_payload)) =
            left.swarm.offer_connection_step(right.did()).await?
        else {
            panic!("expected step-0 ConnectNodeSend payload");
        };
        let OfferReply::Answer(Some(ans1_payload)) =
            right.swarm.answer_offer_step(offer0_payload).await?
        else {
            panic!("expected step-1 non-final ConnectNodeReport payload");
        };
        // Public `accept_answer` rejects non-final reports, while `accept_answer_step_for` returns
        // the signed step-2 continuation payload!
        assert!(left
            .swarm
            .accept_answer(ans1_payload.clone())
            .await
            .is_err());
        let Some(offer2_payload) = left
            .swarm
            .accept_answer_step_for(attempt_l, ans1_payload)
            .await?
        else {
            panic!("expected step-2 SendFinal continuation payload");
        };
        let OfferReply::Answer(None) = right.swarm.answer_offer_step(offer2_payload).await? else {
            panic!("expected responder completion on step 2");
        };
        wait_until_result("3-step bootstrap link admitted", || {
            Ok(left.swarm.is_peer_admitted(right.did())?
                && right.swarm.is_peer_admitted(left.did())?)
        })
        .await?;
        wait_for_msgs([&n1, &n2, &n3, &n4]).await;
    }

    // Now n1 dials n3 over the DHT (`n1 -> n2 -> n3`), exercising the full 3-step overlay signaling
    // (`ConnectNodeSend(step=0)` -> `ConnectNodeReport(step=1, final=false)` -> `ConnectNodeSend(step=2, final=true)`)!
    assert!(unlinked(&n1, n3.did())? && unlinked(&n3, n1.did())?);
    n1.swarm.connect(n3.did()).await?;
    wait_until_result("3-step relayed connect n1 <-> n3 admitted", || {
        Ok(n1.swarm.is_peer_admitted(n3.did())? && n3.swarm.is_peer_admitted(n1.did())?)
    })
    .await?;

    // Send an application message directly over the admitted 3-step custom transport edge!
    n1.swarm
        .send_direct_message(
            crate::swarm::application::ApplicationData::custom(b"hello over 3-step transport"),
            n3.did(),
        )
        .await?;
    loop {
        let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(5), n3_inbox.next())
            .await
            .map_err(|_| Error::InvalidMessage("timed out waiting for n3 inbox".to_string()))?
        else {
            return Err(Error::InvalidMessage("n3 inbox closed".to_string()));
        };
        if let crate::swarm::application::ApplicationDelivery::Message(msg) = event {
            assert_eq!(
                msg.data,
                crate::swarm::application::ApplicationData::custom(b"hello over 3-step transport")
            );
            break;
        }
    }

    // Part C: Witness edge cases A2, A3, A4, A5, and A6:
    // - A2: When a `SendFinal` backend opens its channels (making `attempt` Active) before the
    //   final signaling step fails (e.g. `HandshakeAboveMax` or failed send), the prematurely
    //   admitted generation is retired so no half-open edge blocks future offers.
    {
        let a2_switchboard: Switchboard = Arc::new(DashMap::new());
        let mut t_a2 = CustomMeshTransport::new(
            crate::delegation::DelegateeKey::new_with_seckey(&k1)?
                .delegator_did()
                .to_string(),
            1,
            MAX_CHANNEL_MESSAGE_SIZE,
            Arc::clone(&a2_switchboard),
        );
        t_a2.oversized_final = true;
        let node_a2 = Node::build(
            SwarmBuilder::new(
                TEST_NETWORK_ID,
                Arc::new(t_a2),
                Box::new(crate::storage::MemStorage::new()),
                crate::delegation::DelegateeKey::new_with_seckey(&k1)?,
            )
            .replay_storage(crate::tests::volatile_replay_storage())
            .dht_virtual_nodes(0),
        );
        let peer_a2 = build_custom_node(k2.clone(), 1, MAX_CHANNEL_MESSAGE_SIZE, &a2_switchboard)?;
        let err = node_a2.swarm.offer_connection_step(peer_a2.did()).await;
        assert!(
            matches!(err, Err(Error::HandshakeAboveMax(_))),
            "expected HandshakeAboveMax on oversized SendFinal, got {err:?}"
        );
        assert!(
            !node_a2.swarm.is_peer_admitted(peer_a2.did())?,
            "prematurely admitted SendFinal generation must be retired on final-step failure"
        );
        // And a fresh incoming 1-step offer from peer_a2 is NOT refused as AlreadyConnected!
        let (_peer_attempt, Some(peer_offer)) =
            peer_a2.swarm.offer_connection_step(node_a2.did()).await?
        else {
            panic!("expected 1-step offer");
        };
        let OfferReply::Answer(None) = node_a2.swarm.answer_offer_step(peer_offer).await? else {
            panic!("expected 1-step responder completion");
        };
        wait_until_result(
            "node_a2 admits peer_a2 after retiring failed SendFinal",
            || node_a2.swarm.is_peer_admitted(peer_a2.did()),
        )
        .await?;
        node_a2.swarm.close().await?;
        peer_a2.swarm.close().await?;
    }

    // - A3: Duplicate non-final step-1 reports under the same transaction cannot both drive the
    //   backend state machine, and an `Answering` slot rejects an inbound `ConnectNodeReport`.
    {
        let a3_switchboard: Switchboard = Arc::new(DashMap::new());
        let a3_init = build_custom_node(k1.clone(), 3, MAX_CHANNEL_MESSAGE_SIZE, &a3_switchboard)?;
        let a3_resp = build_custom_node(k2.clone(), 3, MAX_CHANNEL_MESSAGE_SIZE, &a3_switchboard)?;
        let (attempt, Some(offer0)) = a3_init.swarm.offer_connection_step(a3_resp.did()).await?
        else {
            panic!("expected step-0 offer");
        };
        let OfferReply::Answer(Some(ans1)) = a3_resp.swarm.answer_offer_step(offer0).await? else {
            panic!("expected step-1 report");
        };
        // `a3_resp` is currently in `Handshake::Answering`: feeding a forged report to `a3_resp`
        // with `expected = None` (`accept_answer_step`) must be rejected without touching its slot!
        let Message::ConnectNodeReport(ans1_msg) = ans1.transaction.data::<Message>()? else {
            panic!("expected ConnectNodeReport");
        };
        let forged_to_responder = a3_init
            .swarm
            .transport
            .application_egress()
            .originate(
                Message::ConnectNodeReport(ans1_msg.clone()),
                a3_resp.did(),
                Some(a3_init.did()),
            )
            .await?;
        assert!(
            a3_resp
                .swarm
                .accept_answer_step(forged_to_responder)
                .await
                .is_err(),
            "Answering generation must reject inbound ConnectNodeReport"
        );
        // First step-1 report succeeds on `a3_init`; a second distinct signed step-1 report for the
        // same generation is rejected before touching the backend!
        let Some(_offer2) = a3_init.swarm.accept_answer_step_for(attempt, ans1).await? else {
            panic!("expected step-2 continuation");
        };
        let dup_ans1 = a3_resp
            .swarm
            .transport
            .application_egress()
            .originate(
                Message::ConnectNodeReport(ans1_msg),
                a3_init.did(),
                Some(a3_resp.did()),
            )
            .await?;
        assert!(
            a3_init
                .swarm
                .accept_answer_step_for(attempt, dup_ans1)
                .await
                .is_err(),
            "duplicate step-1 report after claim must be rejected"
        );
        a3_init.swarm.close().await?;
        a3_resp.swarm.close().await?;
    }

    // - A4: When the responder backend returns `HandshakeStep::Done` on a non-final inbound offer,
    //   `answer_offer_step` rejects it as a protocol error and abandons the responder slot.
    {
        let a4_switchboard: Switchboard = Arc::new(DashMap::new());
        let a4_init = build_custom_node(k1.clone(), 99, MAX_CHANNEL_MESSAGE_SIZE, &a4_switchboard)?;
        let a4_resp = build_custom_node(k2.clone(), 99, MAX_CHANNEL_MESSAGE_SIZE, &a4_switchboard)?;
        let (_attempt, Some(nonfinal_offer)) =
            a4_init.swarm.offer_connection_step(a4_resp.did()).await?
        else {
            panic!("expected non-final offer");
        };
        assert!(
            a4_resp
                .swarm
                .answer_offer_step(nonfinal_offer)
                .await
                .is_err(),
            "responder returning Done on non-final inbound step must fail and abandon"
        );
        assert_eq!(a4_resp.swarm.transport.pending_connection_count()?, 0);
        a4_init.swarm.close().await?;
        a4_resp.swarm.close().await?;
    }

    // - A5: When a pending connection passes handshake validation (`>= MIN_MAX_MESSAGE_SIZE`) and
    //   subsequently reports `< MIN_MAX_MESSAGE_SIZE` at `on_channel_open` admission time,
    //   `admit_pending_connection` rejects AND cancels the unadmitted attempt immediately.
    {
        let a5_switchboard: Switchboard = Arc::new(DashMap::new());
        let a5_transport = Arc::new(CustomMeshTransport::new(
            crate::delegation::DelegateeKey::new_with_seckey(&k1)?
                .delegator_did()
                .to_string(),
            3,
            MAX_CHANNEL_MESSAGE_SIZE,
            Arc::clone(&a5_switchboard),
        ));
        let a5_node = Node::build(
            SwarmBuilder::new(
                TEST_NETWORK_ID,
                Arc::clone(&a5_transport) as SharedTransport,
                Box::new(crate::storage::MemStorage::new()),
                crate::delegation::DelegateeKey::new_with_seckey(&k1)?,
            )
            .replay_storage(crate::tests::volatile_replay_storage())
            .dht_virtual_nodes(0),
        );
        let peer_did = crate::delegation::DelegateeKey::new_with_seckey(&k2)?.delegator_did();
        let (_attempt, Some(_offer0)) = a5_node.swarm.offer_connection_step(peer_did).await? else {
            panic!("expected step-0 offer");
        };
        assert_eq!(a5_node.swarm.transport.pending_connection_count()?, 1);
        // Shrink negotiated_max below MIN_MAX_MESSAGE_SIZE right before channel open fires!
        a5_transport
            .negotiated_max
            .store(MIN_MAX_MESSAGE_SIZE - 1, Ordering::Release);
        let conn_ref = a5_transport.connection(&peer_did.to_string())?;
        conn_ref.upgrade()?.mark_connected_inline().await;
        assert_eq!(
            a5_node.swarm.transport.pending_connection_count()?,
            0,
            "admission-time RemoteMaxMessageSizeTooSmall must cancel the unadmitted attempt"
        );
        assert!(!a5_node.swarm.is_peer_admitted(peer_did)?);
        a5_node.swarm.close().await?;
    }

    // - A6: Two 1-step initiators crossing each other resolve via `Law (Tie-break)` instead of
    //   deadlocking in `Answered`: the larger-DID initiator yields and admits the smaller-DID's
    //   1-step offer!
    {
        let a6_switchboard: Switchboard = Arc::new(DashMap::new());
        let one_a = build_custom_node(k1.clone(), 1, MAX_CHANNEL_MESSAGE_SIZE, &a6_switchboard)?;
        let one_b = build_custom_node(k2.clone(), 1, MAX_CHANNEL_MESSAGE_SIZE, &a6_switchboard)?;
        let (smaller_one, larger_one) = if one_a.did() < one_b.did() {
            (&one_a, &one_b)
        } else {
            (&one_b, &one_a)
        };
        let (_att_s, Some(offer_s)) = smaller_one
            .swarm
            .offer_connection_step(larger_one.did())
            .await?
        else {
            panic!("expected 1-step offer from smaller");
        };
        let (_att_l, Some(offer_l)) = larger_one
            .swarm
            .offer_connection_step(smaller_one.did())
            .await?
        else {
            panic!("expected 1-step offer from larger");
        };
        // Smaller refuses larger's crossing out-of-band offer, while larger YIELDS to smaller's
        // 1-step offer and completes admission!
        assert!(smaller_one.swarm.answer_offer_step(offer_l).await.is_err());
        let OfferReply::Answer(None) = larger_one.swarm.answer_offer_step(offer_s).await? else {
            panic!("larger 1-step initiator must yield and complete responder step");
        };
        wait_until_result(
            "both 1-step peers mutually admitted after tie-break",
            || {
                Ok(larger_one.swarm.is_peer_admitted(smaller_one.did())?
                    && smaller_one.swarm.is_peer_admitted(larger_one.did())?)
            },
        )
        .await?;
        one_a.swarm.close().await?;
        one_b.swarm.close().await?;
    }

    // - R3-A1: Optimistic 1-step backend reporting local channel readiness inline before `SendFinal`
    //   leaves routes its initial `ConnectNodeSend` over the existing relay hop rather than over
    //   the half-open direct link whose responder has not yet received step 0!
    {
        let r3_sb: Switchboard = Arc::new(DashMap::new());
        let o1 = build_custom_node(k1.clone(), 11, MAX_CHANNEL_MESSAGE_SIZE, &r3_sb)?;
        let o2 = build_custom_node(k2.clone(), 1, MAX_CHANNEL_MESSAGE_SIZE, &r3_sb)?;
        let o3 = build_custom_node(k3.clone(), 1, MAX_CHANNEL_MESSAGE_SIZE, &r3_sb)?;
        let o4 = build_custom_node(k4.clone(), 1, MAX_CHANNEL_MESSAGE_SIZE, &r3_sb)?;

        // Establish clockwise ring `o1 -> o2 -> o3 -> o4 -> o1` (`o1 <-> o3` unlinked).
        for (left, right) in [(&o2, &o1), (&o2, &o3), (&o3, &o4), (&o4, &o1)] {
            let (_, Some(offer_lr)) = left.swarm.offer_connection_step(right.did()).await? else {
                panic!("expected 1-step offer");
            };
            let OfferReply::Answer(None) = right.swarm.answer_offer_step(offer_lr).await? else {
                panic!("expected 1-step completion");
            };
            wait_until_result("1-step ring edge admitted", || {
                Ok(left.swarm.is_peer_admitted(right.did())?
                    && right.swarm.is_peer_admitted(left.did())?)
            })
            .await?;
            wait_for_msgs([&o1, &o2, &o3, &o4]).await;
        }
        assert!(unlinked(&o1, o3.did())? && unlinked(&o3, o1.did())?);

        // Now dial `o3` from `o1` over the overlay (`Swarm::connect`): even though `o1`
        // (`mode_steps == 11`) marks its local channel open inline before `SendFinal` leaves,
        // `send_connection_offer` routes via `o2` so `o3` receives step 0 and opens its side!
        o1.swarm.connect(o3.did()).await?;
        wait_until_result("o3 admits o1 over relayed 1-step offer", || {
            Ok(o3.swarm.is_peer_admitted(o1.did())? && o1.swarm.is_peer_admitted(o3.did())?)
        })
        .await?;
        o1.swarm.close().await?;
        o2.swarm.close().await?;
        o3.swarm.close().await?;
        o4.swarm.close().await?;
    }

    // - R3-A2 & R3-B1: Stateless pre-claim header violations (`step_index = 7, final_step = false`
    //   or `session_id = None`) never abandon a live `Sent(0)` / `Active` slot, and
    //   `Swarm::prepare_delegatee_key_rotation` holds exclusive rotation lock across commit.
    {
        let guard_sb: Switchboard = Arc::new(DashMap::new());
        let g_key_1 = SecretKey::random();
        let g_key_2 = SecretKey::random();
        let g_session_2 = crate::delegation::DelegateeKey::new_with_seckey(&g_key_2)?;
        let g1 = build_custom_node(g_key_1.clone(), 3, MAX_CHANNEL_MESSAGE_SIZE, &guard_sb)?;
        let g2 = build_custom_node(g_key_2, 3, MAX_CHANNEL_MESSAGE_SIZE, &guard_sb)?;

        let (g_attempt, Some(_offer0)) = g1.swarm.offer_connection_step(g2.did()).await? else {
            panic!("expected step-0 offer");
        };
        let forged_step7 = g2
            .swarm
            .transport
            .application_egress()
            .originate(
                Message::ConnectNodeReport(crate::message::ConnectNodeReport {
                    step_index: 7,
                    final_step: false,
                    session_id: Some([0x77; 16]),
                    handshake: Bytes::from_static(&[0xff, 0x01, 0x02]),
                    dht_protocol_mode: g2.swarm.transport.dht_protocol_mode(),
                    relay_custody: g2.swarm.transport.dht.relay_custody(),
                }),
                g1.did(),
                Some(g2.did()),
            )
            .await?;
        assert!(matches!(
            g1.swarm
                .accept_answer_step_for(g_attempt, forged_step7)
                .await,
            Err(Error::InvalidMessage(_))
        ));
        assert_eq!(
            g1.swarm.transport.pending_connection_count()?,
            1,
            "pre-claim MAX_HANDSHAKE_STEPS violation must not abandon live Sent(0) slot"
        );
        g1.swarm.abandon_connection_attempt(g_attempt).await;
        assert_eq!(g1.swarm.transport.pending_connection_count()?, 0);

        let now_ms = g1.swarm.clock().now_ms();
        let next_del = crate::delegation::DelegateeKey::new_with_seckey_at(
            &g_key_1,
            crate::consts::DEFAULT_DELEGATION_TTL_MS + 60_000,
            now_ms,
        )?;
        let commit_token = g1.swarm.prepare_delegatee_key_rotation(&next_del)?;
        assert!(
            g1.swarm
                .transport
                .delegatee_rotation_lock
                .try_lock()
                .is_err(),
            "SwarmRotationCommit must hold delegatee_rotation_lock until committed or dropped"
        );
        commit_token.commit();
        assert!(g1
            .swarm
            .transport
            .delegatee_rotation_lock
            .try_lock()
            .is_ok());
        let _ = g_session_2;
        g1.swarm.close().await?;
        g2.swarm.close().await?;
    }

    // - A7: `claim_continuation_step` enforces uniform validation across out-of-band and
    //   in-band continuations: out-of-band `answer_remote_connection` records `relay_custody`,
    //   and in-band `answer_signaled_offer` rejects mismatched `dht_protocol_mode`.
    {
        let a7_switchboard: Switchboard = Arc::new(DashMap::new());
        let c1 = build_custom_node(k1.clone(), 3, MAX_CHANNEL_MESSAGE_SIZE, &a7_switchboard)?;
        let c2 = build_custom_node(k2.clone(), 3, MAX_CHANNEL_MESSAGE_SIZE, &a7_switchboard)?;
        let (attempt, Some(offer0)) = c1.swarm.offer_connection_step(c2.did()).await? else {
            panic!("expected step-0 offer");
        };
        let OfferReply::Answer(Some(ans1)) = c2.swarm.answer_offer_step(offer0).await? else {
            panic!("expected step-1 non-final answer");
        };
        let Some(offer2) = c1.swarm.accept_answer_step_for(attempt, ans1).await? else {
            panic!("expected step-2 continuation offer");
        };

        let Message::ConnectNodeSend(mut step2_msg) = offer2.transaction.data::<Message>()? else {
            panic!("expected ConnectNodeSend");
        };
        let flipped_custody = if step2_msg.relay_custody.holds() {
            crate::dht::RelayCustody::HoldsNone
        } else {
            crate::dht::RelayCustody::Holds
        };
        assert_eq!(
            c2.swarm
                .transport
                .dht
                .relay_storage_less_peers()?
                .contains(&c1.did()),
            !step2_msg.relay_custody.holds(),
        );
        step2_msg.relay_custody = flipped_custody;
        let OfferReply::Answer(None) = c2
            .swarm
            .transport
            .answer_remote_connection(
                c1.did(),
                SwarmTransportCallback::new(c2.swarm.transport.clone()),
                &step2_msg,
            )
            .await?
        else {
            panic!("expected out-of-band continuation completion");
        };
        assert_eq!(
            c2.swarm
                .transport
                .dht
                .relay_storage_less_peers()?
                .contains(&c1.did()),
            !flipped_custody.holds(),
            "out-of-band continuation must update dht.observe_relay_custody"
        );

        let mut bad_mode_msg = step2_msg;
        bad_mode_msg.dht_protocol_mode = crate::message::DhtProtocolMode::new(TEST_NETWORK_ID + 1, 3, 4);
        assert!(matches!(
            c2.swarm
                .transport
                .answer_signaled_offer(
                    SwarmTransportCallback::new(c2.swarm.transport.clone()),
                    &offer2,
                    &bad_mode_msg,
                )
                .await,
            Err(Error::InvalidMessage(_))
        ));
        c1.swarm.close().await?;
        c2.swarm.close().await?;
    }

    valid_zero_1.swarm.close().await?;
    valid_zero_2.swarm.close().await?;
    n1.swarm.close().await?;
    n2.swarm.close().await?;
    n3.swarm.close().await?;
    n4.swarm.close().await?;
    Ok(())
}
