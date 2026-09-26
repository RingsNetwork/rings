//! Laws of the two link tables, checked over explicit instants: every step takes the instant it
//! is judged at, and no test waits.

use super::AnnouncedDelegations;
use super::Announcement;
use super::Digests;
use super::FrameArrival;
use super::ReferencedDelegations;
use super::ResolvedFrame;
use super::Swept;
use super::ANNOUNCED_TABLE_CAPACITY;
use super::REFERENCED_TABLE_CAPACITY;
use crate::delegation::DelegateeKey;
use crate::delegation::Delegation;
use crate::delegation::DelegationDigest;
use crate::dht::delivery::NextHop;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::message::DelegationRef;
use crate::message::HopBudget;
use crate::message::LinkControl;
use crate::message::LinkFrame;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageRelay;
use crate::message::MessageSigner;
use crate::message::PerSlot;
use crate::message::SlotEncoding;
use crate::message::Transaction;
use crate::message::WirePayload;
use crate::tests::delegatee_key_with_ttl;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

/// Frames a test hold keeps: far more than any test queues, except the overflow test's own.
const HOLD_CAPACITY: usize = 4;
/// How long a test hold keeps a frame.
const HOLD_TIMEOUT_MS: u128 = 1_000;
/// A session lifetime shorter than a proof lifetime, so a session can expire under a live
/// proof, and long enough that a test cannot outlive it between two statements.
const SHORT_SESSION_TTL_MS: u64 = 60_000;
/// The generation the tests' link runs under.
const GENERATION: u64 = 3;

/// A payload whose transaction is signed by `origin` and whose carrier is signed by `hop`.
fn relayed_payload(
    origin: &DelegateeKey,
    hop: &DelegateeKey,
    sequence: u64,
) -> Result<MessagePayload> {
    let destination: Did = SecretKey::random().address().into();
    let transaction = Transaction::new(
        destination,
        crate::utils::new_uuid(),
        sequence,
        None,
        Message::custom(b"session link")?,
        MessageSigner::new(origin, TEST_NETWORK_ID),
    )?;
    let relay = MessageRelay::new(NextHop::toward(destination), destination, HopBudget::MAX);
    MessagePayload::new(transaction, MessageSigner::new(hop, TEST_NETWORK_ID), relay)
}

/// The frame a receiver decodes when `payload` is sent with `sessions` in its slots.
fn received<'a>(
    payload: &'a MessagePayload,
    sessions: PerSlot<DelegationRef<'a>>,
) -> Result<Box<WirePayload<'static>>> {
    let bytes = WirePayload::view(payload, sessions).to_wire()?;
    match LinkFrame::from_wire(bytes.as_ref())? {
        LinkFrame::Payload(frame) => Ok(frame),
        LinkFrame::Control(control) => panic!("expected a payload frame, decoded {control:?}"),
    }
}

/// `session` by reference.
fn by_digest(session: &Delegation) -> Result<DelegationRef<'static>> {
    session.digest().map(DelegationRef::Digest)
}

/// A frame of `payload` whose origin slot is by reference and whose hop slot is inline.
fn origin_referenced(payload: &MessagePayload) -> Result<Box<WirePayload<'static>>> {
    let sessions = payload.delegations();
    received(payload, PerSlot {
        origin: by_digest(sessions.origin)?,
        hop: DelegationRef::inline(sessions.hop),
    })
}

/// The frame a sender in state `sender` puts on the wire for `payload`.
fn sent(
    sender: &mut AnnouncedDelegations,
    payload: &MessagePayload,
    now_ms: u128,
) -> Result<Box<WirePayload<'static>>> {
    let sessions = sender.encode(GENERATION, payload, now_ms)?;
    received(payload, sessions)
}

/// How each slot of `frame` travelled.
fn encoding(frame: &WirePayload<'_>) -> PerSlot<SlotEncoding> {
    frame.delegation_refs().map(DelegationRef::encoding)
}

/// Both slots inline.
const BOTH_INLINE: PerSlot<SlotEncoding> = PerSlot {
    origin: SlotEncoding::Inline,
    hop: SlotEncoding::Inline,
};

/// Both slots by reference.
const BOTH_REFERENCED: PerSlot<SlotEncoding> = PerSlot {
    origin: SlotEncoding::Referenced,
    hop: SlotEncoding::Referenced,
};

/// A receiver with the test hold bounds.
fn receiver<F>() -> ReferencedDelegations<F> {
    ReferencedDelegations::new(HOLD_CAPACITY, HOLD_TIMEOUT_MS)
}

/// The digest set `{digests}`.
fn digests<const N: usize>(digests: [DelegationDigest; N]) -> Digests {
    Digests::from(digests)
}

/// The resolved frame of an arrival that must pass.
fn expect_resolved<F>(arrival: FrameArrival<F>) -> Box<ResolvedFrame<F>> {
    match arrival {
        FrameArrival::Resolved(resolved) => resolved,
        FrameArrival::Held { .. } => panic!("expected the frame to resolve, it was held"),
        FrameArrival::Overflow { .. } => {
            panic!("expected the frame to resolve, the hold overflowed")
        }
    }
}

/// The digests an arrival that must be held asks for.
fn expect_held<F>(arrival: FrameArrival<F>) -> Digests {
    match arrival {
        FrameArrival::Held { request } => request,
        FrameArrival::Resolved(_) => panic!("expected the frame to be held, it resolved"),
        FrameArrival::Overflow { .. } => {
            panic!("expected the frame to be held, the hold overflowed")
        }
    }
}

/// Deliver a frame the receiver resolved: verify it and let the link learn from it, as the
/// shell does; the digests to confirm to the peer.
fn deliver(
    receiver: &mut ReferencedDelegations<u8>,
    resolved: Box<ResolvedFrame<u8>>,
    now_ms: u128,
) -> Result<Digests> {
    assert!(resolved
        .payload
        .verify_transaction_and_payload(TEST_NETWORK_ID));
    receiver.admit_verified(&resolved.payload, resolved.encoding, now_ms)
}

/// Arrive and deliver a frame that must resolve; the digests to confirm.
fn arrive_and_deliver(
    receiver: &mut ReferencedDelegations<u8>,
    frame: Box<WirePayload<'static>>,
    carrier: u8,
    now_ms: u128,
) -> Result<Digests> {
    let resolved = expect_resolved(receiver.arrive(frame, carrier, now_ms)?);
    deliver(receiver, resolved, now_ms)
}

/// The carriers and payloads of every held frame that resolves now, in release order.
fn released(
    receiver: &mut ReferencedDelegations<u8>,
    now_ms: u128,
) -> Result<Vec<(u8, MessagePayload)>> {
    let mut out = Vec::new();
    while let Some(resolved) = receiver.release_next(now_ms)? {
        let ResolvedFrame {
            payload, carrier, ..
        } = *resolved;
        out.push((carrier, payload));
    }
    Ok(out)
}

/// Law (soundness): a session is referenced only after the peer confirmed it; until then every
/// frame carries it inline, however many were sent.
#[test]
fn test_sender_references_only_confirmed_sessions() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = AnnouncedDelegations::new();

    for _ in 0..3 {
        assert_eq!(
            encoding(sent(&mut sender, &payload, now_ms)?.as_ref()),
            BOTH_INLINE
        );
    }
    sender.acknowledge(GENERATION, origin.delegation().digest()?);
    assert_eq!(
        encoding(sent(&mut sender, &payload, now_ms)?.as_ref()),
        PerSlot {
            origin: SlotEncoding::Referenced,
            hop: SlotEncoding::Inline,
        }
    );
    sender.acknowledge(GENERATION, hop.delegation().digest()?);
    assert_eq!(sender.encode(GENERATION, &payload, now_ms)?, PerSlot {
        origin: by_digest(&origin.delegation())?,
        hop: by_digest(&hop.delegation())?,
    });
    Ok(())
}

/// Law: a confirmation of a digest this end never announced marks nothing.
#[test]
fn test_unannounced_confirmation_marks_nothing() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let stranger = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();

    sender.acknowledge(GENERATION, stranger.delegation().digest()?);
    sender.acknowledge(GENERATION, node.delegation().digest()?);
    assert_eq!(
        encoding(sent(&mut sender, &payload, now_ms)?.as_ref()),
        BOTH_INLINE
    );
    assert_eq!(
        sender.answer(GENERATION, stranger.delegation().digest()?, now_ms),
        LinkControl::Unknown(stranger.delegation().digest()?)
    );
    Ok(())
}

/// Acceptance: a steady-state frame carries two 20-byte digests where it carried two sessions,
/// and the saving is the two delegations.
#[test]
fn test_steady_state_frame_carries_digests_instead_of_sessions() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = AnnouncedDelegations::new();

    let first = sender.encode(GENERATION, &payload, now_ms)?;
    let first_size = WirePayload::view(&payload, first).wire_size()?;
    sender.acknowledge(GENERATION, origin.delegation().digest()?);
    sender.acknowledge(GENERATION, hop.delegation().digest()?);
    let steady = sender.encode(GENERATION, &payload, now_ms)?;
    let steady_size = WirePayload::view(&payload, steady).wire_size()?;

    let session_bytes = |session: &Delegation| -> Result<usize> {
        Ok(rings_codec::serialize(session)
            .map_err(crate::error::Error::CodecSerialize)?
            .len())
    };
    let digest_bytes = origin.delegation().digest()?.into_bytes().len();
    assert_eq!(digest_bytes, 20);
    let saved = session_bytes(&origin.delegation())? + session_bytes(&hop.delegation())?;
    assert_eq!(first_size, payload.wire_size()?);
    assert_eq!(first_size - steady_size, saved - 2 * digest_bytes);
    Ok(())
}

/// Law: the table of one generation says nothing about another; an older generation's frames
/// are self-contained, and a newer generation starts empty.
#[test]
fn test_sender_table_is_scoped_to_its_generation() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let digest = node.delegation().digest()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();
    sent(&mut sender, &payload, now_ms)?;
    sender.acknowledge(GENERATION, digest);
    assert_eq!(
        encoding(sent(&mut sender, &payload, now_ms)?.as_ref()),
        BOTH_REFERENCED
    );

    let next = sender.encode(GENERATION + 1, &payload, now_ms)?;
    assert_eq!(next, payload.delegations().map(DelegationRef::inline));
    assert_eq!(
        sender.answer(GENERATION, digest, now_ms),
        LinkControl::Unknown(digest)
    );
    sender.acknowledge(GENERATION, digest);
    let stale = sender.encode(GENERATION, &payload, now_ms)?;
    assert_eq!(stale, payload.delegations().map(DelegationRef::inline));
    assert_eq!(
        encoding(sent(&mut sender, &payload, now_ms)?.as_ref()),
        BOTH_INLINE
    );
    Ok(())
}

/// Acceptance (expiry, sending end): an expired session is sent inline again rather than
/// referenced, and is no longer offered to a peer that asks for it.
#[test]
fn test_sender_expiry_forces_reannouncement() -> Result<()> {
    // The session is stamped before `now_ms`, so `now_ms + ttl + 1` is past its expiry.
    let node = delegatee_key_with_ttl(SHORT_SESSION_TTL_MS)?;
    let now_ms = get_epoch_ms();
    let digest = node.delegation().digest()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();
    sent(&mut sender, &payload, now_ms)?;
    sender.acknowledge(GENERATION, digest);
    assert_eq!(
        encoding(sent(&mut sender, &payload, now_ms)?.as_ref()),
        BOTH_REFERENCED
    );
    assert_eq!(
        sender.answer(GENERATION, digest, now_ms),
        LinkControl::Announce(node.delegation())
    );

    let expired_ms = now_ms + u128::from(SHORT_SESSION_TTL_MS) + 1;
    assert_eq!(
        encoding(sent(&mut sender, &payload, expired_ms)?.as_ref()),
        BOTH_INLINE
    );
    assert_eq!(
        sender.answer(GENERATION, digest, expired_ms),
        LinkControl::Unknown(digest)
    );
    Ok(())
}

/// Law (bound): the sender remembers at most `ANNOUNCED_TABLE_CAPACITY` sessions, and the one
/// referenced least recently is the one it forgets.
#[test]
fn test_sender_table_is_bounded_and_forgets_least_recently_referenced() -> Result<()> {
    let now_ms = get_epoch_ms();
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let first_origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let mut sender = AnnouncedDelegations::new();
    sent(
        &mut sender,
        &relayed_payload(&first_origin, &hop, 0)?,
        now_ms,
    )?;

    // `hop` is referenced by every frame, so it stays; `first_origin` is never referenced again.
    for _ in 0..ANNOUNCED_TABLE_CAPACITY {
        let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        sent(&mut sender, &relayed_payload(&origin, &hop, 0)?, now_ms)?;
    }

    assert_eq!(
        sender.answer(GENERATION, first_origin.delegation().digest()?, now_ms),
        LinkControl::Unknown(first_origin.delegation().digest()?)
    );
    assert_eq!(
        sender.answer(GENERATION, hop.delegation().digest()?, now_ms),
        LinkControl::Announce(hop.delegation())
    );
    Ok(())
}

/// Law (superset, under loss): a frame lost on the link touches the sender's order and not the
/// receiver's, so the receiver may evict a session the sender still references; that miss is
/// answered from the sender's table, which still holds the session.
#[test]
fn test_loss_drifts_the_orders_and_the_sender_still_answers_the_miss() -> Result<()> {
    let now_ms = get_epoch_ms();
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let origin_digest = origin.delegation().digest()?;
    let mut sender = AnnouncedDelegations::new();
    let mut receiver = receiver();
    let first = sent(&mut sender, &relayed_payload(&origin, &hop, 0)?, now_ms)?;
    for digest in arrive_and_deliver(&mut receiver, first, 0, now_ms)? {
        sender.acknowledge(GENERATION, digest);
    }

    // Each round delivers one other origin to both ends, then loses a frame that references
    // `origin`: the sender keeps `origin` most recent, the receiver never sees it touched.
    for _ in 0..REFERENCED_TABLE_CAPACITY {
        let other = DelegateeKey::new_with_seckey(&SecretKey::random())?;
        let delivered = sent(&mut sender, &relayed_payload(&other, &hop, 0)?, now_ms)?;
        for digest in arrive_and_deliver(&mut receiver, delivered, 1, now_ms)? {
            sender.acknowledge(GENERATION, digest);
        }
        let lost = sent(&mut sender, &relayed_payload(&origin, &hop, 1)?, now_ms)?;
        assert_eq!(encoding(lost.as_ref()).origin, SlotEncoding::Referenced);
    }

    // The receiver forgot `origin`; the sender still references it and still answers for it.
    let late = sent(&mut sender, &relayed_payload(&origin, &hop, 2)?, now_ms)?;
    assert_eq!(encoding(late.as_ref()).origin, SlotEncoding::Referenced);
    assert_eq!(
        expect_held(receiver.arrive(late, 2, now_ms)?),
        digests([origin_digest])
    );
    assert_eq!(
        sender.answer(GENERATION, origin_digest, now_ms),
        LinkControl::Announce(origin.delegation())
    );
    Ok(())
}

/// Law (bound and superset): the receiver remembers at most `REFERENCED_TABLE_CAPACITY`
/// sessions, twice the sender's, and forgets the one referenced least recently; over the same
/// frames the sender forgets a session first, so it goes back to inline before the receiver
/// could miss, and a reference the receiver resolves refreshes the session on its side.
#[test]
fn test_receiver_table_is_bounded_and_outlasts_the_sender_table() -> Result<()> {
    let now_ms = get_epoch_ms();
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let first_origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let first_digest = first_origin.delegation().digest()?;
    let mut sender = AnnouncedDelegations::new();
    let mut receiver = receiver();
    let first = sent(
        &mut sender,
        &relayed_payload(&first_origin, &hop, 0)?,
        now_ms,
    )?;
    for digest in arrive_and_deliver(&mut receiver, first, 0, now_ms)? {
        sender.acknowledge(GENERATION, digest);
    }

    // `hop` is referenced by every frame, so both tables keep it; each origin is seen once.
    let relay_others = |sender: &mut AnnouncedDelegations,
                        receiver: &mut ReferencedDelegations<u8>,
                        count: usize|
     -> Result<()> {
        for _ in 0..count {
            let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
            let frame = sent(sender, &relayed_payload(&origin, &hop, 0)?, now_ms)?;
            assert_eq!(encoding(frame.as_ref()).hop, SlotEncoding::Referenced);
            for digest in arrive_and_deliver(receiver, frame, 1, now_ms)? {
                sender.acknowledge(GENERATION, digest);
            }
        }
        Ok(())
    };

    // After the sender's capacity of other sessions, the sender forgot `first_origin` and
    // will send it inline again, while the receiver still resolves a reference to it.
    relay_others(&mut sender, &mut receiver, ANNOUNCED_TABLE_CAPACITY)?;
    assert_eq!(
        sender.answer(GENERATION, first_digest, now_ms),
        LinkControl::Unknown(first_digest)
    );
    let late = relayed_payload(&first_origin, &hop, 1)?;
    expect_resolved(receiver.arrive(origin_referenced(&late)?, 2, now_ms)?);

    // That reference refreshed it: the receiver forgets it only once its whole capacity of
    // other sessions was referenced after it (the hop's session is always among them).
    relay_others(&mut sender, &mut receiver, REFERENCED_TABLE_CAPACITY)?;
    assert_eq!(receiver.known_len(), REFERENCED_TABLE_CAPACITY);
    let stale = relayed_payload(&first_origin, &hop, 2)?;
    assert_eq!(
        expect_held(receiver.arrive(origin_referenced(&stale)?, 3, now_ms)?),
        digests([first_digest])
    );
    // `hop`, referenced throughout, is kept by both.
    let hop_only = relayed_payload(&hop, &hop, 0)?;
    let frame = sent(&mut sender, &hop_only, now_ms)?;
    assert_eq!(encoding(frame.as_ref()), BOTH_REFERENCED);
    expect_resolved(receiver.arrive(frame, 4, now_ms)?);
    Ok(())
}

/// Law (round trip and confirmation): what the receiver resolves is the payload the sender
/// encoded, inline or by reference; the receiver confirms every inline session it verified;
/// once both ends have gone through the exchange, frames carry references and resolve.
#[test]
fn test_confirmation_exchange_reaches_references_and_resolves() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let mut sender = AnnouncedDelegations::new();
    let mut receiver = receiver();

    let payload = relayed_payload(&origin, &hop, 0)?;
    let frame = sent(&mut sender, &payload, now_ms)?;
    assert_eq!(encoding(frame.as_ref()), BOTH_INLINE);
    let resolved = expect_resolved(receiver.arrive(frame, 0, now_ms)?);
    assert_eq!(resolved.payload, payload);
    let confirm = deliver(&mut receiver, resolved, now_ms)?;
    assert_eq!(
        confirm,
        digests([origin.delegation().digest()?, hop.delegation().digest()?])
    );
    for digest in confirm {
        sender.acknowledge(GENERATION, digest);
    }

    let payload = relayed_payload(&origin, &hop, 1)?;
    let frame = sent(&mut sender, &payload, now_ms)?;
    assert_eq!(encoding(frame.as_ref()), BOTH_REFERENCED);
    let resolved = expect_resolved(receiver.arrive(frame, 1, now_ms)?);
    assert_eq!(resolved.payload, payload);
    assert_eq!(
        resolved.payload.transaction.digest()?,
        payload.transaction.digest()?
    );
    assert!(deliver(&mut receiver, resolved, now_ms)?.is_empty());
    assert_eq!(receiver.known_len(), 2);
    Ok(())
}

/// Law (datagram link): with confirmations, loss and reordering of inline frames cost inline
/// frames, never a miss. The first inline frame is lost; the sender keeps announcing; the
/// confirmation of a later one switches it; references sent after that resolve, whatever
/// order they arrive in.
#[test]
fn test_loss_and_reordering_before_confirmation_never_miss() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let digest = node.delegation().digest()?;
    let mut sender = AnnouncedDelegations::new();
    let mut receiver = receiver();

    let lost = sent(&mut sender, &relayed_payload(&node, &node, 0)?, now_ms)?;
    assert_eq!(encoding(lost.as_ref()), BOTH_INLINE);
    drop(lost);
    let second = sent(&mut sender, &relayed_payload(&node, &node, 1)?, now_ms)?;
    let third = sent(&mut sender, &relayed_payload(&node, &node, 2)?, now_ms)?;
    assert_eq!(encoding(second.as_ref()), BOTH_INLINE);
    assert_eq!(encoding(third.as_ref()), BOTH_INLINE);

    // The third arrives before the second and is confirmed; the sender switches.
    let confirm = arrive_and_deliver(&mut receiver, third, 2, now_ms)?;
    assert_eq!(confirm, digests([digest]));
    sender.acknowledge(GENERATION, digest);
    let fourth = sent(&mut sender, &relayed_payload(&node, &node, 3)?, now_ms)?;
    assert_eq!(encoding(fourth.as_ref()), BOTH_REFERENCED);

    // The reference arrives before the late inline frame, and both resolve.
    expect_resolved(receiver.arrive(fourth, 3, now_ms)?);
    let confirm_again = arrive_and_deliver(&mut receiver, second, 1, now_ms)?;
    assert_eq!(confirm_again, digests([digest]));
    assert_eq!(receiver.held_len(), 0);
    Ok(())
}

/// Acceptance (origin miss): a hop whose table does not hold the origin's session holds the
/// frame, asks for exactly that digest, and releases the frame on the announcement.
#[test]
fn test_origin_session_miss_is_repaired_by_announcement() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut receiver = receiver();

    assert_eq!(
        expect_held(receiver.arrive(origin_referenced(&payload)?, 7u8, now_ms)?),
        digests([origin.delegation().digest()?])
    );
    assert!(receiver.release_next(now_ms)?.is_none());

    assert!(matches!(
        receiver.announce(origin.delegation(), now_ms)?,
        Announcement::Admitted
    ));
    assert_eq!(released(&mut receiver, now_ms)?, vec![(7, payload)]);
    assert_eq!(receiver.held_len(), 0);
    Ok(())
}

/// Acceptance (hop miss): the same repair when the missing session is the forwarding hop's.
#[test]
fn test_hop_session_miss_is_repaired_by_announcement() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&origin, &hop, 0)?;
    let frame = received(&payload, PerSlot {
        origin: DelegationRef::inline(&origin.delegation()),
        hop: by_digest(&hop.delegation())?,
    })?;
    let mut receiver = receiver();

    assert_eq!(
        expect_held(receiver.arrive(frame, 7u8, now_ms)?),
        digests([hop.delegation().digest()?])
    );
    assert!(matches!(
        receiver.announce(hop.delegation(), now_ms)?,
        Announcement::Admitted
    ));
    assert_eq!(released(&mut receiver, now_ms)?, vec![(7, payload)]);
    Ok(())
}

/// The whole miss exchange between the two pure ends: the receiver forgot a confirmed
/// session, its question is answered from the sender's table, and the answer releases the
/// frame.
#[test]
fn test_miss_is_answered_from_the_sender_table() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let digest = node.delegation().digest()?;
    let mut sender = AnnouncedDelegations::new();
    sent(&mut sender, &relayed_payload(&node, &node, 0)?, now_ms)?;
    sender.acknowledge(GENERATION, digest);

    // The receiver's table is fresh: it forgot what it confirmed.
    let payload = relayed_payload(&node, &node, 1)?;
    let frame = sent(&mut sender, &payload, now_ms)?;
    assert_eq!(encoding(frame.as_ref()), BOTH_REFERENCED);
    let mut receiver = receiver();
    let request = expect_held(receiver.arrive(frame, 1u8, now_ms)?);
    assert_eq!(request, digests([digest]));
    for digest in request {
        match sender.answer(GENERATION, digest, now_ms) {
            LinkControl::Announce(session) => {
                assert!(matches!(
                    receiver.announce(session, now_ms)?,
                    Announcement::Admitted
                ));
            }
            answer => panic!("expected an announcement, got {answer:?}"),
        }
    }
    assert_eq!(released(&mut receiver, now_ms)?, vec![(1, payload)]);
    Ok(())
}

/// Law (order): a frame that resolves passes whatever is held, and a held frame never waits
/// for one held before it (the second frame stays while the first and third leave); among the
/// frames resolvable at one instant, the earliest arrival leaves first.
#[test]
fn test_held_frames_never_wait_for_each_other_and_resolvable_ones_leave_earliest_first(
) -> Result<()> {
    let now_ms = get_epoch_ms();
    let first_stranger = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let second_stranger = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let mut receiver = receiver();

    let first = relayed_payload(&first_stranger, &hop, 0)?;
    let second = relayed_payload(&second_stranger, &hop, 0)?;
    let third = relayed_payload(&first_stranger, &hop, 1)?;
    let ready = relayed_payload(&hop, &hop, 0)?;
    assert_eq!(
        expect_held(receiver.arrive(origin_referenced(&first)?, 1u8, now_ms)?),
        digests([first_stranger.delegation().digest()?])
    );
    assert_eq!(
        expect_held(receiver.arrive(origin_referenced(&second)?, 2u8, now_ms)?),
        digests([second_stranger.delegation().digest()?])
    );
    // The third misses what the first already awaits, and asks again all the same.
    assert_eq!(
        expect_held(receiver.arrive(origin_referenced(&third)?, 3u8, now_ms)?),
        digests([first_stranger.delegation().digest()?])
    );
    let ready_frame = received(&ready, ready.delegations().map(DelegationRef::inline))?;
    expect_resolved(receiver.arrive(ready_frame, 4u8, now_ms)?);
    assert_eq!(receiver.held_len(), 3);

    assert!(matches!(
        receiver.announce(first_stranger.delegation(), now_ms)?,
        Announcement::Admitted
    ));
    assert_eq!(released(&mut receiver, now_ms)?, vec![
        (1, first),
        (3, third)
    ]);
    assert_eq!(receiver.held_len(), 1);
    assert!(matches!(
        receiver.announce(second_stranger.delegation(), now_ms)?,
        Announcement::Admitted
    ));
    assert_eq!(released(&mut receiver, now_ms)?, vec![(2, second)]);
    Ok(())
}

/// Law (questions): a frame that finds the hold full is dropped and the oldest held frame's
/// question is asked again, never the newcomer's.
#[test]
fn test_overflow_drops_the_newcomer_and_asks_the_oldest_question_again() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let newcomer_stranger = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let mut receiver = receiver();
    let digest = stranger.delegation().digest()?;

    for carrier in 0..HOLD_CAPACITY {
        let payload = relayed_payload(&stranger, &hop, 0)?;
        assert_eq!(
            expect_held(receiver.arrive(origin_referenced(&payload)?, carrier, now_ms)?),
            digests([digest])
        );
    }
    let newcomer = relayed_payload(&newcomer_stranger, &hop, 0)?;
    assert!(matches!(
        receiver.arrive(origin_referenced(&newcomer)?, HOLD_CAPACITY, now_ms)?,
        FrameArrival::Overflow { carrier, request }
            if carrier == HOLD_CAPACITY && request == digests([digest])
    ));
    assert_eq!(receiver.held_len(), HOLD_CAPACITY);
    Ok(())
}

/// Law (admission): nothing unsolicited is cached, and a disclaimer fails exactly the frames
/// that await the disclaimed digest.
#[test]
fn test_unsolicited_announcement_is_ignored_and_disclaimer_fails_awaiting_frames() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let other = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let mut receiver = receiver();

    assert!(matches!(
        receiver.announce(stranger.delegation(), now_ms)?,
        Announcement::Ignored
    ));
    assert_eq!(receiver.known_len(), 0);

    let payload = relayed_payload(&stranger, &hop, 0)?;
    expect_held(receiver.arrive(origin_referenced(&payload)?, 1u8, now_ms)?);

    assert!(receiver
        .unknown(other.delegation().digest()?, now_ms)
        .is_empty());
    assert_eq!(receiver.held_len(), 1);
    assert_eq!(
        receiver.unknown(stranger.delegation().digest()?, now_ms),
        vec![1]
    );
    assert_eq!(receiver.held_len(), 0);
    Ok(())
}

/// Acceptance (expiry, receiving end): expiry evicts the session, a reference to it becomes a
/// miss that must be re-announced, and re-announcing the expired delegation is refused, so
/// only a fresh delegation restores the link. The proof is still live throughout, so the
/// refusal is the session's, as it would be for the same session inline.
#[test]
fn test_receiver_expiry_evicts_and_refuses_the_expired_delegation() -> Result<()> {
    // The session is stamped before `now_ms`, so `now_ms + ttl + 1` is past its expiry.
    let node = delegatee_key_with_ttl(SHORT_SESSION_TTL_MS)?;
    let now_ms = get_epoch_ms();
    let digest = node.delegation().digest()?;
    let mut sender = AnnouncedDelegations::new();
    let mut receiver = receiver();

    let first = sent(&mut sender, &relayed_payload(&node, &node, 0)?, now_ms)?;
    for digest in arrive_and_deliver(&mut receiver, first, 0u8, now_ms)? {
        sender.acknowledge(GENERATION, digest);
    }
    let steady = sent(&mut sender, &relayed_payload(&node, &node, 1)?, now_ms)?;
    assert_eq!(encoding(steady.as_ref()), BOTH_REFERENCED);
    expect_resolved(receiver.arrive(steady, 1u8, now_ms)?);

    let expired_ms = now_ms + u128::from(SHORT_SESSION_TTL_MS) + 1;
    let payload = relayed_payload(&node, &node, 2)?;
    assert!(payload.verification.is_live_at(expired_ms));
    let stale = received(&payload, PerSlot {
        origin: DelegationRef::Digest(digest),
        hop: DelegationRef::Digest(digest),
    })?;
    assert_eq!(
        expect_held(receiver.arrive(stale, 2u8, expired_ms)?),
        digests([digest])
    );
    assert!(matches!(
        receiver.announce(node.delegation(), expired_ms)?,
        Announcement::Refused(ref refused) if refused == &vec![2]
    ));
    assert_eq!(receiver.held_len(), 0);

    // A fresh delegation is a different value, hence a different digest: it travels inline.
    let renewed = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let renewed_payload = relayed_payload(&renewed, &renewed, 0)?;
    let frame = sent(&mut sender, &renewed_payload, expired_ms)?;
    assert_eq!(encoding(frame.as_ref()), BOTH_INLINE);
    arrive_and_deliver(&mut receiver, frame, 3u8, expired_ms)?;
    assert_eq!(receiver.known_len(), 1);
    Ok(())
}

/// Law (idempotence): a sender that lost its table (a new generation, a restart) sends a
/// session its peer already knows inline; the receiver resolves the frame, knows the session
/// once, and confirms it again so the sender can switch.
#[test]
fn test_reannouncing_a_known_session_is_idempotent() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let mut receiver = receiver();

    for sequence in 0..2 {
        let payload = relayed_payload(&node, &node, sequence)?;
        let frame = sent(&mut AnnouncedDelegations::new(), &payload, now_ms)?;
        assert_eq!(encoding(frame.as_ref()), BOTH_INLINE);
        let resolved = expect_resolved(receiver.arrive(frame, 0u8, now_ms)?);
        assert_eq!(resolved.payload, payload);
        assert_eq!(
            deliver(&mut receiver, resolved, now_ms)?,
            digests([node.delegation().digest()?])
        );
        assert_eq!(receiver.known_len(), 1);
    }
    Ok(())
}

/// The carriers a sweep dropped, charged and uncharged.
fn swept(swept: Swept<u8>) -> (Vec<u8>, Vec<u8>) {
    (swept.unanswered, swept.unasked)
}

/// Law (bound in time): the sweep drops a held frame once it waited past the hold timeout,
/// whatever lifetime its proof claims, and one whose proof lapsed sooner; a frame within both
/// bounds stays. A frame whose question was sent is dropped as unanswered, to be charged.
#[test]
fn test_sweep_drops_frames_past_the_hold_timeout_or_their_proof() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let mut receiver = receiver();

    let early = relayed_payload(&stranger, &hop, 0)?;
    let question = expect_held(receiver.arrive(origin_referenced(&early)?, 0u8, now_ms)?);
    receiver.note_asked(question);
    let later_ms = now_ms + HOLD_TIMEOUT_MS;
    let late = relayed_payload(&stranger, &hop, 1)?;
    expect_held(receiver.arrive(origin_referenced(&late)?, 1u8, later_ms)?);

    assert_eq!(swept(receiver.sweep(later_ms)), (vec![], vec![]));
    assert_eq!(swept(receiver.sweep(later_ms + 1)), (vec![0], vec![]));
    assert_eq!(receiver.held_len(), 1);

    // A hold timeout longer than the proof's lifetime: the proof lapses first.
    let lapsed_ms = late.verification.ts_ms + u128::from(late.verification.ttl_ms) + 1;
    let hold_outlasting_the_proof_ms = u128::from(late.verification.ttl_ms).saturating_mul(2);
    let mut lapsed_receiver: ReferencedDelegations<u8> =
        ReferencedDelegations::new(HOLD_CAPACITY, hold_outlasting_the_proof_ms);
    let question = expect_held(lapsed_receiver.arrive(origin_referenced(&late)?, 2u8, now_ms)?);
    lapsed_receiver.note_asked(question);
    assert_eq!(
        swept(lapsed_receiver.sweep(lapsed_ms - 1)),
        (vec![], vec![])
    );
    assert_eq!(swept(lapsed_receiver.sweep(lapsed_ms)), (vec![2], vec![]));
    Ok(())
}

/// Law (charging): a held frame whose question this end never sent is swept uncharged, since
/// the peer never had its round trip; once the question is noted as asked, the same frame is
/// swept as unanswered. Learning the session clears the question.
#[test]
fn test_sweep_tells_unasked_frames_from_unanswered_ones() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let hop = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let mut receiver = receiver();
    let stale_ms = now_ms + HOLD_TIMEOUT_MS + 1;

    let unasked = relayed_payload(&stranger, &hop, 0)?;
    expect_held(receiver.arrive(origin_referenced(&unasked)?, 0u8, now_ms)?);
    assert_eq!(swept(receiver.sweep(stale_ms)), (vec![], vec![0]));

    let asked = relayed_payload(&stranger, &hop, 1)?;
    let question = expect_held(receiver.arrive(origin_referenced(&asked)?, 1u8, now_ms)?);
    receiver.note_asked(question);
    assert_eq!(swept(receiver.sweep(stale_ms)), (vec![1], vec![]));

    // Once the session is learned the question is spent: a later miss on it starts unasked.
    let again = relayed_payload(&stranger, &hop, 2)?;
    let question = expect_held(receiver.arrive(origin_referenced(&again)?, 2u8, now_ms)?);
    receiver.note_asked(question);
    assert!(matches!(
        receiver.announce(stranger.delegation(), now_ms)?,
        Announcement::Admitted
    ));
    assert_eq!(released(&mut receiver, now_ms)?.len(), 1);
    Ok(())
}
