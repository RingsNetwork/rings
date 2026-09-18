//! Laws of the two link tables, checked over explicit instants: no test reads a clock after
//! fixing its base instant, and none waits.

use std::borrow::Cow;

use super::AnnouncedSessions;
use super::FrameArrival;
use super::FrameRelease;
use super::ReferencedSessions;
use super::ResolvedFrame;
use super::SESSION_TABLE_CAPACITY;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::message::HopBudget;
use crate::message::LinkFrame;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageRelay;
use crate::message::MessageSigner;
use crate::message::PerSlot;
use crate::message::SessionControl;
use crate::message::SessionRef;
use crate::message::Transaction;
use crate::message::WirePayload;
use crate::session::Session;
use crate::session::SessionDigest;
use crate::session::SessionSk;
use crate::session::SessionSkBuilder;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

/// Frames a test hold keeps: far more than any test queues, except the overflow test's own.
const HOLD_CAPACITY: usize = 4;
/// A session lifetime shorter than a proof lifetime, so a session can expire under a live
/// proof, and long enough that a test cannot outlive it between two statements.
const SHORT_SESSION_TTL_MS: u64 = 60_000;
/// The generation the tests' link runs under.
const GENERATION: u64 = 3;

/// A session key whose delegation lives `ttl_ms` from now.
fn session_sk_with_ttl(ttl_ms: u64) -> Result<SessionSk> {
    let account = SecretKey::random();
    let account_did: Did = account.address().into();
    let builder =
        SessionSkBuilder::new(account_did.to_string(), "secp256k1".to_string()).set_ttl(ttl_ms);
    let sig = account.sign(&builder.unsigned_proof())?.to_vec();
    builder.set_session_sig(sig).build()
}

/// A payload whose transaction is signed by `origin` and whose carrier is signed by `hop`.
fn relayed_payload(origin: &SessionSk, hop: &SessionSk, sequence: u64) -> Result<MessagePayload> {
    let destination: Did = SecretKey::random().address().into();
    let transaction = Transaction::new(
        destination,
        crate::utils::new_uuid(),
        sequence,
        Message::custom(b"session link")?,
        MessageSigner::new(origin, TEST_NETWORK_ID),
    )?;
    let relay = MessageRelay::new(destination, destination, HopBudget::MAX);
    MessagePayload::new(transaction, MessageSigner::new(hop, TEST_NETWORK_ID), relay)
}

/// The frame a receiver decodes when `payload` is sent with `sessions` in its slots.
fn received<'a>(
    payload: &'a MessagePayload,
    sessions: PerSlot<SessionRef<'a>>,
) -> Result<Box<WirePayload<'static>>> {
    let bytes = WirePayload::view(payload, sessions).to_wire()?;
    match LinkFrame::from_wire(bytes.as_ref())? {
        LinkFrame::Payload(frame) => Ok(frame),
        LinkFrame::Control(control) => panic!("expected a payload frame, decoded {control:?}"),
    }
}

/// `session` inline.
fn inline(session: &Session) -> SessionRef<'_> {
    SessionRef::Inline(Cow::Borrowed(session))
}

/// `session` by reference.
fn by_digest(session: &Session) -> Result<SessionRef<'static>> {
    session.digest().map(SessionRef::Digest)
}

/// The frame a sender in state `sender` puts on the wire for `payload`.
fn sent(
    sender: &mut AnnouncedSessions,
    payload: &MessagePayload,
    now_ms: u128,
) -> Result<Box<WirePayload<'static>>> {
    let sessions = sender.encode(GENERATION, payload, now_ms)?;
    received(payload, sessions)
}

/// Which slots of `frame` are inline.
fn inline_slots(frame: &WirePayload<'_>) -> PerSlot<bool> {
    frame
        .session_refs()
        .map(|session| matches!(session, SessionRef::Inline(_)))
}

/// Both slots inline.
const BOTH_INLINE: PerSlot<bool> = PerSlot {
    origin: true,
    hop: true,
};

/// Both slots by reference.
const BOTH_REFERENCED: PerSlot<bool> = PerSlot {
    origin: false,
    hop: false,
};

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
fn expect_held<F>(arrival: FrameArrival<F>) -> Vec<SessionDigest> {
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
    receiver: &mut ReferencedSessions<u8>,
    resolved: Box<ResolvedFrame<u8>>,
    now_ms: u128,
) -> Result<Vec<SessionDigest>> {
    assert!(resolved
        .payload
        .verify_transaction_and_payload(TEST_NETWORK_ID));
    receiver.admit_verified(&resolved.payload, resolved.inline, now_ms)
}

/// The carriers of every held frame that can leave now.
fn released(
    receiver: &mut ReferencedSessions<u8>,
    now_ms: u128,
) -> Result<Vec<(u8, MessagePayload)>> {
    let mut out = Vec::new();
    while let Some(release) = receiver.release_next(now_ms)? {
        match release {
            FrameRelease::Resolved(resolved) => {
                let ResolvedFrame {
                    payload, carrier, ..
                } = *resolved;
                out.push((carrier, payload));
            }
            FrameRelease::Lapsed(_) => {}
        }
    }
    Ok(out)
}

/// Law (soundness): a session is referenced only after the peer confirmed it; until then every
/// frame carries it inline, however many were sent.
#[test]
fn test_sender_references_only_confirmed_sessions() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = AnnouncedSessions::new();

    for _ in 0..3 {
        assert_eq!(
            inline_slots(sent(&mut sender, &payload, now_ms)?.as_ref()),
            BOTH_INLINE
        );
    }
    sender.acknowledge(GENERATION, origin.session().digest()?);
    assert_eq!(
        inline_slots(sent(&mut sender, &payload, now_ms)?.as_ref()),
        PerSlot {
            origin: false,
            hop: true,
        }
    );
    sender.acknowledge(GENERATION, hop.session().digest()?);
    assert_eq!(sender.encode(GENERATION, &payload, now_ms)?, PerSlot {
        origin: by_digest(&origin.session())?,
        hop: by_digest(&hop.session())?,
    });
    Ok(())
}

/// Law: a confirmation of a digest this end never announced marks nothing.
#[test]
fn test_unannounced_confirmation_marks_nothing() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = SessionSk::new_with_seckey(&SecretKey::random())?;
    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedSessions::new();

    sender.acknowledge(GENERATION, stranger.session().digest()?);
    sender.acknowledge(GENERATION, node.session().digest()?);
    assert_eq!(
        inline_slots(sent(&mut sender, &payload, now_ms)?.as_ref()),
        BOTH_INLINE
    );
    assert_eq!(
        sender.answer(GENERATION, stranger.session().digest()?, now_ms),
        SessionControl::Unknown(stranger.session().digest()?)
    );
    Ok(())
}

/// Acceptance: a steady-state frame carries two digests where it carried two sessions, and the
/// saving is the two delegations.
#[test]
fn test_steady_state_frame_carries_digests_instead_of_sessions() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = AnnouncedSessions::new();

    let first = sender.encode(GENERATION, &payload, now_ms)?;
    let first_size = WirePayload::view(&payload, first).wire_size()?;
    sender.acknowledge(GENERATION, origin.session().digest()?);
    sender.acknowledge(GENERATION, hop.session().digest()?);
    let steady = sender.encode(GENERATION, &payload, now_ms)?;
    let steady_size = WirePayload::view(&payload, steady).wire_size()?;

    let session_bytes = |session: &Session| -> Result<usize> {
        Ok(rings_codec::serialize(session)
            .map_err(crate::error::Error::CodecSerialize)?
            .len())
    };
    let digest_bytes = origin.session().digest()?.into_bytes().len();
    let saved = session_bytes(&origin.session())? + session_bytes(&hop.session())?;
    assert_eq!(first_size, payload.wire_size()?);
    assert_eq!(first_size - steady_size, saved - 2 * digest_bytes);
    Ok(())
}

/// Law: the table of one generation says nothing about another; an older generation's frames
/// are self-contained, and a newer generation starts empty.
#[test]
fn test_sender_table_is_scoped_to_its_generation() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = SessionSk::new_with_seckey(&SecretKey::random())?;
    let digest = node.session().digest()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedSessions::new();
    sent(&mut sender, &payload, now_ms)?;
    sender.acknowledge(GENERATION, digest);
    assert_eq!(
        inline_slots(sent(&mut sender, &payload, now_ms)?.as_ref()),
        BOTH_REFERENCED
    );

    let next = sender.encode(GENERATION + 1, &payload, now_ms)?;
    assert_eq!(next, payload.sessions().map(inline));
    assert_eq!(
        sender.answer(GENERATION, digest, now_ms),
        SessionControl::Unknown(digest)
    );
    sender.acknowledge(GENERATION, digest);
    let stale = sender.encode(GENERATION, &payload, now_ms)?;
    assert_eq!(stale, payload.sessions().map(inline));
    assert_eq!(
        inline_slots(sent(&mut sender, &payload, now_ms)?.as_ref()),
        BOTH_INLINE
    );
    Ok(())
}

/// Acceptance (expiry, sending end): an expired session is sent inline again rather than
/// referenced, and is no longer offered to a peer that asks for it.
#[test]
fn test_sender_expiry_forces_reannouncement() -> Result<()> {
    // The session is stamped before `now_ms`, so `now_ms + ttl + 1` is past its expiry.
    let node = session_sk_with_ttl(SHORT_SESSION_TTL_MS)?;
    let now_ms = get_epoch_ms();
    let digest = node.session().digest()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedSessions::new();
    sent(&mut sender, &payload, now_ms)?;
    sender.acknowledge(GENERATION, digest);
    assert_eq!(
        inline_slots(sent(&mut sender, &payload, now_ms)?.as_ref()),
        BOTH_REFERENCED
    );
    assert_eq!(
        sender.answer(GENERATION, digest, now_ms),
        SessionControl::Announce(node.session())
    );

    let expired_ms = now_ms + u128::from(SHORT_SESSION_TTL_MS) + 1;
    assert_eq!(
        inline_slots(sent(&mut sender, &payload, expired_ms)?.as_ref()),
        BOTH_INLINE
    );
    assert_eq!(
        sender.answer(GENERATION, digest, expired_ms),
        SessionControl::Unknown(digest)
    );
    Ok(())
}

/// Law (bound): the sender remembers at most `SESSION_TABLE_CAPACITY` sessions, and the one
/// referenced least recently is the one it forgets.
#[test]
fn test_sender_table_is_bounded_and_forgets_least_recently_referenced() -> Result<()> {
    let now_ms = get_epoch_ms();
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let first_origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut sender = AnnouncedSessions::new();
    sent(
        &mut sender,
        &relayed_payload(&first_origin, &hop, 0)?,
        now_ms,
    )?;

    // `hop` is referenced by every frame, so it stays; `first_origin` is never referenced again.
    for _ in 0..SESSION_TABLE_CAPACITY {
        let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
        sent(&mut sender, &relayed_payload(&origin, &hop, 0)?, now_ms)?;
    }

    assert_eq!(
        sender.answer(GENERATION, first_origin.session().digest()?, now_ms),
        SessionControl::Unknown(first_origin.session().digest()?)
    );
    assert_eq!(
        sender.answer(GENERATION, hop.session().digest()?, now_ms),
        SessionControl::Announce(hop.session())
    );
    Ok(())
}

/// Law (round trip and confirmation): what the receiver resolves is the payload the sender
/// encoded, inline or by reference; the receiver confirms every inline session it verified;
/// once both ends have gone through the exchange, frames carry references and resolve.
#[test]
fn test_confirmation_exchange_reaches_references_and_resolves() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut sender = AnnouncedSessions::new();
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    let payload = relayed_payload(&origin, &hop, 0)?;
    let frame = sent(&mut sender, &payload, now_ms)?;
    assert_eq!(inline_slots(frame.as_ref()), BOTH_INLINE);
    let resolved = expect_resolved(receiver.arrive(frame, 0, now_ms)?);
    assert_eq!(resolved.payload, payload);
    let confirm = deliver(&mut receiver, resolved, now_ms)?;
    assert_eq!(confirm, vec![
        origin.session().digest()?,
        hop.session().digest()?
    ]);
    for digest in confirm {
        sender.acknowledge(GENERATION, digest);
    }

    let payload = relayed_payload(&origin, &hop, 1)?;
    let frame = sent(&mut sender, &payload, now_ms)?;
    assert_eq!(inline_slots(frame.as_ref()), BOTH_REFERENCED);
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
    let node = SessionSk::new_with_seckey(&SecretKey::random())?;
    let digest = node.session().digest()?;
    let mut sender = AnnouncedSessions::new();
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    let lost = sent(&mut sender, &relayed_payload(&node, &node, 0)?, now_ms)?;
    assert_eq!(inline_slots(lost.as_ref()), BOTH_INLINE);
    drop(lost);
    let second = sent(&mut sender, &relayed_payload(&node, &node, 1)?, now_ms)?;
    let third = sent(&mut sender, &relayed_payload(&node, &node, 2)?, now_ms)?;
    assert_eq!(inline_slots(second.as_ref()), BOTH_INLINE);
    assert_eq!(inline_slots(third.as_ref()), BOTH_INLINE);

    // The third arrives before the second and is confirmed; the sender switches.
    let resolved = expect_resolved(receiver.arrive(third, 2, now_ms)?);
    let confirm = deliver(&mut receiver, resolved, now_ms)?;
    assert_eq!(confirm, vec![digest]);
    sender.acknowledge(GENERATION, digest);
    let fourth = sent(&mut sender, &relayed_payload(&node, &node, 3)?, now_ms)?;
    assert_eq!(inline_slots(fourth.as_ref()), BOTH_REFERENCED);

    // The reference arrives before the late inline frame, and both resolve.
    expect_resolved(receiver.arrive(fourth, 3, now_ms)?);
    let resolved = expect_resolved(receiver.arrive(second, 1, now_ms)?);
    let confirm_again = deliver(&mut receiver, resolved, now_ms)?;
    assert_eq!(confirm_again, vec![digest]);
    assert_eq!(receiver.held_len(), 0);
    Ok(())
}

/// Acceptance (origin miss): a hop whose table no longer holds the origin's session holds the
/// frame, asks for exactly that digest, and releases the frame on the announcement.
#[test]
fn test_origin_session_miss_is_repaired_by_announcement() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&origin, &hop, 0)?;
    let frame = received(&payload, PerSlot {
        origin: by_digest(&origin.session())?,
        hop: inline(&hop.session()),
    })?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    assert_eq!(expect_held(receiver.arrive(frame, 7u8, now_ms)?), vec![
        origin.session().digest()?
    ]);
    assert!(receiver.release_next(now_ms)?.is_none());

    assert!(receiver.announce(origin.session(), now_ms)?.is_empty());
    assert_eq!(released(&mut receiver, now_ms)?, vec![(7, payload)]);
    assert_eq!(receiver.held_len(), 0);
    Ok(())
}

/// Acceptance (hop miss): the same repair when the missing session is the forwarding hop's.
#[test]
fn test_hop_session_miss_is_repaired_by_announcement() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&origin, &hop, 0)?;
    let frame = received(&payload, PerSlot {
        origin: inline(&origin.session()),
        hop: by_digest(&hop.session())?,
    })?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    assert_eq!(expect_held(receiver.arrive(frame, 7u8, now_ms)?), vec![hop
        .session()
        .digest()?]);
    assert!(receiver.announce(hop.session(), now_ms)?.is_empty());
    assert_eq!(released(&mut receiver, now_ms)?, vec![(7, payload)]);
    Ok(())
}

/// The whole miss exchange between the two pure ends: the receiver forgot a confirmed
/// session, its question is answered from the sender's table, and the answer releases the
/// frame.
#[test]
fn test_miss_is_answered_from_the_sender_table() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = SessionSk::new_with_seckey(&SecretKey::random())?;
    let digest = node.session().digest()?;
    let mut sender = AnnouncedSessions::new();
    sent(&mut sender, &relayed_payload(&node, &node, 0)?, now_ms)?;
    sender.acknowledge(GENERATION, digest);

    // The receiver's table is fresh: it forgot what it confirmed.
    let payload = relayed_payload(&node, &node, 1)?;
    let frame = sent(&mut sender, &payload, now_ms)?;
    assert_eq!(inline_slots(frame.as_ref()), BOTH_REFERENCED);
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);
    let request = expect_held(receiver.arrive(frame, 1u8, now_ms)?);
    assert_eq!(request, vec![digest]);
    for digest in request {
        match sender.answer(GENERATION, digest, now_ms) {
            SessionControl::Announce(session) => {
                assert!(receiver.announce(session, now_ms)?.is_empty());
            }
            answer => panic!("expected an announcement, got {answer:?}"),
        }
    }
    assert_eq!(released(&mut receiver, now_ms)?, vec![(1, payload)]);
    Ok(())
}

/// Law (no order): a frame that resolves passes whatever is held; held frames leave as their
/// sessions arrive, each independently of the others.
#[test]
fn test_held_frames_do_not_block_resolvable_ones() -> Result<()> {
    let now_ms = get_epoch_ms();
    let first_stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let second_stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    let first = relayed_payload(&first_stranger, &hop, 0)?;
    let second = relayed_payload(&second_stranger, &hop, 0)?;
    let ready = relayed_payload(&hop, &hop, 0)?;
    let first_frame = received(&first, PerSlot {
        origin: by_digest(&first_stranger.session())?,
        hop: inline(&hop.session()),
    })?;
    let second_frame = received(&second, PerSlot {
        origin: by_digest(&second_stranger.session())?,
        hop: inline(&hop.session()),
    })?;
    assert_eq!(
        expect_held(receiver.arrive(first_frame, 1u8, now_ms)?),
        vec![first_stranger.session().digest()?]
    );
    assert_eq!(
        expect_held(receiver.arrive(second_frame, 2u8, now_ms)?),
        vec![second_stranger.session().digest()?]
    );
    let ready_frame = received(&ready, ready.sessions().map(inline))?;
    expect_resolved(receiver.arrive(ready_frame, 3u8, now_ms)?);
    assert_eq!(receiver.held_len(), 2);

    assert!(receiver
        .announce(second_stranger.session(), now_ms)?
        .is_empty());
    assert_eq!(released(&mut receiver, now_ms)?, vec![(2, second)]);
    assert_eq!(receiver.held_len(), 1);
    assert!(receiver
        .announce(first_stranger.session(), now_ms)?
        .is_empty());
    assert_eq!(released(&mut receiver, now_ms)?, vec![(1, first)]);
    Ok(())
}

/// Law (questions): a digest is asked for when the first frame awaiting it is held, not for
/// every frame that awaits it; an overflow asks for every awaited digest again.
#[test]
fn test_questions_are_asked_once_per_awaited_digest_and_again_on_overflow() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);
    let digest = stranger.session().digest()?;

    for carrier in 0..HOLD_CAPACITY {
        let payload = relayed_payload(&stranger, &hop, 0)?;
        let frame = received(&payload, PerSlot {
            origin: by_digest(&stranger.session())?,
            hop: inline(&hop.session()),
        })?;
        let request = expect_held(receiver.arrive(frame, carrier, now_ms)?);
        assert_eq!(request, if carrier == 0 { vec![digest] } else { vec![] });
    }
    let payload = relayed_payload(&stranger, &hop, 0)?;
    let frame = received(&payload, PerSlot {
        origin: by_digest(&stranger.session())?,
        hop: inline(&hop.session()),
    })?;
    assert!(matches!(
        receiver.arrive(frame, HOLD_CAPACITY, now_ms)?,
        FrameArrival::Overflow { request } if request == vec![digest]
    ));
    Ok(())
}

/// Law (admission): nothing unsolicited is cached, and a disclaimer fails exactly the frames
/// that await the disclaimed digest.
#[test]
fn test_unsolicited_announcement_is_ignored_and_disclaimer_fails_awaiting_frames() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let other = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    assert!(receiver.announce(stranger.session(), now_ms)?.is_empty());
    assert_eq!(receiver.known_len(), 0);

    let payload = relayed_payload(&stranger, &hop, 0)?;
    let frame = received(&payload, PerSlot {
        origin: by_digest(&stranger.session())?,
        hop: inline(&hop.session()),
    })?;
    expect_held(receiver.arrive(frame, 1u8, now_ms)?);

    assert!(receiver
        .unknown(other.session().digest()?, now_ms)
        .is_empty());
    assert_eq!(receiver.held_len(), 1);
    assert_eq!(
        receiver.unknown(stranger.session().digest()?, now_ms),
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
    let node = session_sk_with_ttl(SHORT_SESSION_TTL_MS)?;
    let now_ms = get_epoch_ms();
    let digest = node.session().digest()?;
    let mut sender = AnnouncedSessions::new();
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    let first = sent(&mut sender, &relayed_payload(&node, &node, 0)?, now_ms)?;
    let resolved = expect_resolved(receiver.arrive(first, 0u8, now_ms)?);
    let confirm = deliver(&mut receiver, resolved, now_ms)?;
    for digest in confirm {
        sender.acknowledge(GENERATION, digest);
    }
    let steady = sent(&mut sender, &relayed_payload(&node, &node, 1)?, now_ms)?;
    assert_eq!(inline_slots(steady.as_ref()), BOTH_REFERENCED);
    expect_resolved(receiver.arrive(steady, 1u8, now_ms)?);

    let expired_ms = now_ms + u128::from(SHORT_SESSION_TTL_MS) + 1;
    let payload = relayed_payload(&node, &node, 2)?;
    assert!(payload.verification.is_live_at(expired_ms));
    let stale = received(&payload, PerSlot {
        origin: SessionRef::Digest(digest),
        hop: SessionRef::Digest(digest),
    })?;
    assert_eq!(expect_held(receiver.arrive(stale, 2u8, expired_ms)?), vec![
        digest
    ]);
    assert_eq!(receiver.announce(node.session(), expired_ms)?, vec![2]);
    assert_eq!(receiver.held_len(), 0);

    // A fresh delegation is a different value, hence a different digest: it travels inline.
    let renewed = SessionSk::new_with_seckey(&SecretKey::random())?;
    let renewed_payload = relayed_payload(&renewed, &renewed, 0)?;
    let frame = sent(&mut sender, &renewed_payload, expired_ms)?;
    assert_eq!(inline_slots(frame.as_ref()), BOTH_INLINE);
    let resolved = expect_resolved(receiver.arrive(frame, 3u8, expired_ms)?);
    deliver(&mut receiver, resolved, expired_ms)?;
    assert_eq!(receiver.known_len(), 1);
    Ok(())
}

/// Law (idempotence): a sender that lost its table (a new generation, a restart) sends a
/// session its peer already knows inline; the receiver resolves the frame, knows the session
/// once, and confirms it again so the sender can switch.
#[test]
fn test_reannouncing_a_known_session_is_idempotent() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    for sequence in 0..2 {
        let payload = relayed_payload(&node, &node, sequence)?;
        let frame = sent(&mut AnnouncedSessions::new(), &payload, now_ms)?;
        assert_eq!(inline_slots(frame.as_ref()), BOTH_INLINE);
        let resolved = expect_resolved(receiver.arrive(frame, 0u8, now_ms)?);
        assert_eq!(resolved.payload, payload);
        assert_eq!(deliver(&mut receiver, resolved, now_ms)?, vec![node
            .session()
            .digest()?]);
        assert_eq!(receiver.known_len(), 1);
    }
    Ok(())
}

/// Law (bound): a held frame whose proof lifetime lapsed is dropped instead of waiting on.
#[test]
fn test_lapsed_held_frames_are_dropped() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    let mut latest = relayed_payload(&stranger, &hop, 0)?;
    for carrier in 0..HOLD_CAPACITY {
        latest = relayed_payload(&stranger, &hop, 0)?;
        let frame = received(&latest, PerSlot {
            origin: by_digest(&stranger.session())?,
            hop: inline(&hop.session()),
        })?;
        expect_held(receiver.arrive(frame, carrier, now_ms)?);
    }
    // Every held proof was stamped no later than `latest`'s, so all have lapsed by then.
    let lapsed_ms = latest.verification.ts_ms + u128::from(latest.verification.ttl_ms) + 1;
    let mut dropped = Vec::new();
    while let Some(release) = receiver.release_next(lapsed_ms)? {
        match release {
            FrameRelease::Lapsed(carrier) => dropped.push(carrier),
            FrameRelease::Resolved(_) => panic!("a lapsed frame must not resolve"),
        }
    }
    dropped.sort_unstable();
    assert_eq!(dropped, (0..HOLD_CAPACITY).collect::<Vec<_>>());
    assert_eq!(receiver.held_len(), 0);
    Ok(())
}
