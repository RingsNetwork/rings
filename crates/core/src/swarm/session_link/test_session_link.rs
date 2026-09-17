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

/// The frame a sender in state `sender` puts on the wire for `payload`, committed as accepted.
fn sent(
    sender: &mut AnnouncedSessions,
    payload: &MessagePayload,
    now_ms: u128,
) -> Result<Box<WirePayload<'static>>> {
    let plan = sender.plan(GENERATION, payload, now_ms)?;
    let frame = received(payload, plan.session_refs(payload))?;
    sender.commit(plan, now_ms);
    Ok(frame)
}

/// Which slots of `frame` are inline.
fn inline_slots(frame: &WirePayload<'_>) -> PerSlot<bool> {
    frame
        .session_refs()
        .map(|session| matches!(session, SessionRef::Inline(_)))
}

/// The resolved frame of an arrival that must pass.
fn expect_resolved<F>(arrival: FrameArrival<F>) -> Box<ResolvedFrame<F>> {
    match arrival {
        FrameArrival::Resolved(resolved) => resolved,
        FrameArrival::Held => panic!("expected the frame to resolve, it was held"),
        FrameArrival::Overflow(_) => panic!("expected the frame to resolve, the hold overflowed"),
    }
}

/// The digests a drain that must block asks for.
fn expect_blocked<F>(receiver: &mut ReferencedSessions<F>, now_ms: u128) -> Vec<SessionDigest> {
    assert!(receiver.begin_drain());
    match receiver.release_next(now_ms) {
        Ok(FrameRelease::Blocked(request)) => request,
        Ok(_) => panic!("expected the drain to block on a missing session"),
        Err(error) => panic!("drain failed: {error:?}"),
    }
}

/// The carriers a drain releases, in order, until it ends.
fn drain_resolved(
    receiver: &mut ReferencedSessions<u8>,
    now_ms: u128,
) -> Result<Vec<(u8, MessagePayload)>> {
    let mut released = Vec::new();
    if !receiver.begin_drain() {
        return Ok(released);
    }
    loop {
        match receiver.release_next(now_ms)? {
            FrameRelease::Resolved(resolved) => {
                let ResolvedFrame {
                    payload, carrier, ..
                } = *resolved;
                released.push((carrier, payload));
            }
            FrameRelease::Lapsed(_) => {}
            FrameRelease::Blocked(_) | FrameRelease::Drained => return Ok(released),
        }
    }
}

/// Law (soundness of `plan`): a session is referenced only after a frame carrying it inline was
/// committed; an uncommitted plan announces nothing.
#[test]
fn test_sender_references_only_committed_announcements() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = AnnouncedSessions::new();

    let uncommitted = sender.plan(GENERATION, &payload, now_ms)?;
    assert_eq!(
        uncommitted.session_refs(&payload),
        payload.sessions().map(inline)
    );
    let again = sender.plan(GENERATION, &payload, now_ms)?;
    assert_eq!(again.session_refs(&payload), payload.sessions().map(inline));

    sender.commit(again, now_ms);
    let steady = sender.plan(GENERATION, &payload, now_ms)?;
    assert_eq!(steady.session_refs(&payload), PerSlot {
        origin: by_digest(&origin.session())?,
        hop: by_digest(&hop.session())?,
    });
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

    let first = sender.plan(GENERATION, &payload, now_ms)?;
    let first_size = WirePayload::view(&payload, first.session_refs(&payload)).wire_size()?;
    sender.commit(first, now_ms);
    let steady = sender.plan(GENERATION, &payload, now_ms)?;
    let steady_size = WirePayload::view(&payload, steady.session_refs(&payload)).wire_size()?;

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

/// Law: the table of one generation says nothing about another.
#[test]
fn test_sender_table_is_scoped_to_its_generation() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = SessionSk::new_with_seckey(&SecretKey::random())?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedSessions::new();
    let plan = sender.plan(GENERATION, &payload, now_ms)?;
    sender.commit(plan, now_ms);

    let next = sender.plan(GENERATION + 1, &payload, now_ms)?;
    assert_eq!(next.session_refs(&payload), payload.sessions().map(inline));
    assert_eq!(
        sender.answer(GENERATION + 1, node.session().digest()?, now_ms),
        SessionControl::Unknown(node.session().digest()?)
    );

    sender.commit(next, now_ms);
    let stale = sender.plan(GENERATION, &payload, now_ms)?;
    assert_eq!(stale.session_refs(&payload), payload.sessions().map(inline));
    sender.commit(stale, now_ms);
    let current = sender.plan(GENERATION + 1, &payload, now_ms)?;
    assert_eq!(current.session_refs(&payload), PerSlot {
        origin: by_digest(&node.session())?,
        hop: by_digest(&node.session())?,
    });
    Ok(())
}

/// Acceptance (expiry, sending end): an expired session is announced again rather than
/// referenced, and is no longer offered to a peer that asks for it.
#[test]
fn test_sender_expiry_forces_reannouncement() -> Result<()> {
    // The session is stamped before `now_ms`, so `now_ms + ttl + 1` is past its expiry.
    let node = session_sk_with_ttl(SHORT_SESSION_TTL_MS)?;
    let now_ms = get_epoch_ms();
    let digest = node.session().digest()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedSessions::new();
    let plan = sender.plan(GENERATION, &payload, now_ms)?;
    sender.commit(plan, now_ms);
    assert_eq!(
        sender.answer(GENERATION, digest, now_ms),
        SessionControl::Announce(node.session())
    );

    let expired_ms = now_ms + u128::from(SHORT_SESSION_TTL_MS) + 1;
    let plan = sender.plan(GENERATION, &payload, expired_ms)?;
    assert_eq!(plan.session_refs(&payload), payload.sessions().map(inline));
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

/// Law (round trip): what the receiver resolves is the payload the sender encoded, whether a
/// slot travelled inline or by reference; and the resolved payload verifies as the original.
#[test]
fn test_resolved_payload_is_the_sent_payload() -> Result<()> {
    let now_ms = get_epoch_ms();
    let origin = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut sender = AnnouncedSessions::new();
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    for sequence in 0..3 {
        let payload = relayed_payload(&origin, &hop, sequence)?;
        let frame = sent(&mut sender, &payload, now_ms)?;
        let expected_inline = PerSlot {
            origin: sequence == 0,
            hop: sequence == 0,
        };
        assert_eq!(inline_slots(&frame), expected_inline);

        let resolved = expect_resolved(receiver.arrive(frame, (), now_ms)?);
        assert_eq!(resolved.payload, payload);
        assert_eq!(
            resolved.payload.transaction.digest()?,
            payload.transaction.digest()?
        );
        assert!(resolved
            .payload
            .verify_transaction_and_payload(TEST_NETWORK_ID));
        receiver.admit_verified(&resolved.payload, resolved.inline, now_ms)?;
    }
    assert_eq!(receiver.known_len(), 2);
    Ok(())
}

/// Law (idempotence): a sender that lost its table (a new generation, a restart) announces a
/// session its peer already knows; the receiver resolves the frame and knows the session once.
#[test]
fn test_reannouncing_a_known_session_is_idempotent() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    for sequence in 0..2 {
        let payload = relayed_payload(&node, &node, sequence)?;
        let frame = sent(&mut AnnouncedSessions::new(), &payload, now_ms)?;
        assert_eq!(inline_slots(&frame), PerSlot {
            origin: true,
            hop: true
        });
        let resolved = expect_resolved(receiver.arrive(frame, (), now_ms)?);
        assert_eq!(resolved.payload, payload);
        receiver.admit_verified(&resolved.payload, resolved.inline, now_ms)?;
        assert_eq!(receiver.known_len(), 1);
    }
    Ok(())
}

/// Acceptance (origin miss): a hop that never saw the origin's session holds the frame, asks
/// for exactly that digest, and resolves the frame from the announcement.
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

    assert!(matches!(
        receiver.arrive(frame, 7u8, now_ms)?,
        FrameArrival::Held
    ));
    assert_eq!(expect_blocked(&mut receiver, now_ms), vec![origin
        .session()
        .digest()?]);

    assert!(receiver.announce(origin.session(), now_ms)?.is_empty());
    assert_eq!(drain_resolved(&mut receiver, now_ms)?, vec![(7, payload)]);
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

    assert!(matches!(
        receiver.arrive(frame, 7u8, now_ms)?,
        FrameArrival::Held
    ));
    assert_eq!(expect_blocked(&mut receiver, now_ms), vec![hop
        .session()
        .digest()?]);

    assert!(receiver.announce(hop.session(), now_ms)?.is_empty());
    assert_eq!(drain_resolved(&mut receiver, now_ms)?, vec![(7, payload)]);
    Ok(())
}

/// The whole miss exchange between the two pure ends: the receiver's question is answered from
/// the sender's table, and the answer releases the frame.
#[test]
fn test_miss_is_answered_from_the_sender_table() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut sender = AnnouncedSessions::new();
    sent(&mut sender, &relayed_payload(&node, &node, 0)?, now_ms)?;

    // The receiver never learned the first frame (say it failed verification there).
    let payload = relayed_payload(&node, &node, 1)?;
    let frame = sent(&mut sender, &payload, now_ms)?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);
    assert!(matches!(
        receiver.arrive(frame, 1u8, now_ms)?,
        FrameArrival::Held
    ));

    let request = expect_blocked(&mut receiver, now_ms);
    assert_eq!(request, vec![node.session().digest()?]);
    for digest in request {
        match sender.answer(GENERATION, digest, now_ms) {
            SessionControl::Announce(session) => {
                assert!(receiver.announce(session, now_ms)?.is_empty());
            }
            answer => panic!("expected an announcement, got {answer:?}"),
        }
    }
    assert_eq!(drain_resolved(&mut receiver, now_ms)?, vec![(1, payload)]);
    Ok(())
}

/// Law (order): a resolvable frame that arrives behind a held one leaves after it.
#[test]
fn test_frames_leave_in_arrival_order() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let blocked = relayed_payload(&stranger, &hop, 0)?;
    let ready = relayed_payload(&hop, &hop, 0)?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    let blocked_frame = received(&blocked, PerSlot {
        origin: by_digest(&stranger.session())?,
        hop: inline(&hop.session()),
    })?;
    let ready_frame = received(&ready, ready.sessions().map(inline))?;
    assert!(matches!(
        receiver.arrive(blocked_frame, 1u8, now_ms)?,
        FrameArrival::Held
    ));
    assert!(matches!(
        receiver.arrive(ready_frame, 2u8, now_ms)?,
        FrameArrival::Held
    ));

    assert!(receiver.announce(stranger.session(), now_ms)?.is_empty());
    assert_eq!(drain_resolved(&mut receiver, now_ms)?, vec![
        (1, blocked),
        (2, ready)
    ]);
    Ok(())
}

/// Law: while a drain is claimed, an arrival queues behind it even if it would resolve.
#[test]
fn test_arrival_during_drain_queues_behind_it() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let first = relayed_payload(&stranger, &hop, 0)?;
    let second = relayed_payload(&hop, &hop, 0)?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);
    let first_frame = received(&first, PerSlot {
        origin: by_digest(&stranger.session())?,
        hop: inline(&hop.session()),
    })?;
    assert!(matches!(
        receiver.arrive(first_frame, 1u8, now_ms)?,
        FrameArrival::Held
    ));
    assert!(receiver.announce(stranger.session(), now_ms)?.is_empty());

    assert!(receiver.begin_drain());
    assert!(!receiver.begin_drain());
    assert!(matches!(
        receiver.release_next(now_ms)?,
        FrameRelease::Resolved(resolved) if resolved.carrier == 1
    ));
    let second_frame = received(&second, second.sessions().map(inline))?;
    assert!(matches!(
        receiver.arrive(second_frame, 2u8, now_ms)?,
        FrameArrival::Held
    ));
    assert!(matches!(
        receiver.release_next(now_ms)?,
        FrameRelease::Resolved(resolved) if resolved.carrier == 2
    ));
    assert!(matches!(
        receiver.release_next(now_ms)?,
        FrameRelease::Drained
    ));
    assert!(!receiver.begin_drain());
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
    assert!(matches!(
        receiver.arrive(frame, 1u8, now_ms)?,
        FrameArrival::Held
    ));

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
    receiver.admit_verified(&resolved.payload, resolved.inline, now_ms)?;
    let steady = received(&relayed_payload(&node, &node, 1)?, PerSlot {
        origin: SessionRef::Digest(digest),
        hop: SessionRef::Digest(digest),
    })?;
    expect_resolved(receiver.arrive(steady, 1u8, now_ms)?);

    let expired_ms = now_ms + u128::from(SHORT_SESSION_TTL_MS) + 1;
    let payload = relayed_payload(&node, &node, 2)?;
    assert!(payload.verification.is_live_at(expired_ms));
    let stale = received(&payload, PerSlot {
        origin: SessionRef::Digest(digest),
        hop: SessionRef::Digest(digest),
    })?;
    assert!(matches!(
        receiver.arrive(stale, 2u8, expired_ms)?,
        FrameArrival::Held
    ));
    assert_eq!(expect_blocked(&mut receiver, expired_ms), vec![digest]);

    assert_eq!(receiver.announce(node.session(), expired_ms)?, vec![2]);
    assert_eq!(receiver.held_len(), 0);

    // A fresh delegation is a different value, hence a different digest: it travels inline.
    let renewed = SessionSk::new_with_seckey(&SecretKey::random())?;
    let renewed_payload = relayed_payload(&renewed, &renewed, 0)?;
    let frame = sent(&mut sender, &renewed_payload, expired_ms)?;
    assert_eq!(inline_slots(&frame), PerSlot {
        origin: true,
        hop: true
    });
    let resolved = expect_resolved(receiver.arrive(frame, 3u8, expired_ms)?);
    receiver.admit_verified(&resolved.payload, resolved.inline, expired_ms)?;
    assert_eq!(receiver.known_len(), 1);
    Ok(())
}

/// Law (bound): the hold refuses the frame that would exceed its capacity, and a head whose
/// proof lifetime lapsed is dropped instead of asked about.
#[test]
fn test_hold_is_bounded_and_drops_lapsed_heads() -> Result<()> {
    let now_ms = get_epoch_ms();
    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let hop = SessionSk::new_with_seckey(&SecretKey::random())?;
    let mut receiver = ReferencedSessions::new(HOLD_CAPACITY);

    for carrier in 0..HOLD_CAPACITY {
        let payload = relayed_payload(&stranger, &hop, 0)?;
        let frame = received(&payload, PerSlot {
            origin: by_digest(&stranger.session())?,
            hop: inline(&hop.session()),
        })?;
        assert!(matches!(
            receiver.arrive(frame, carrier, now_ms)?,
            FrameArrival::Held
        ));
    }
    let payload = relayed_payload(&stranger, &hop, 0)?;
    let frame = received(&payload, payload.sessions().map(inline))?;
    assert!(matches!(
        receiver.arrive(frame, HOLD_CAPACITY, now_ms)?,
        FrameArrival::Overflow(carrier) if carrier == HOLD_CAPACITY
    ));

    // Every held proof was stamped no later than `payload`'s, so all have lapsed by then.
    let lapsed_ms = payload.verification.ts_ms + u128::from(payload.verification.ttl_ms) + 1;
    assert!(receiver.begin_drain());
    for carrier in 0..HOLD_CAPACITY {
        assert!(matches!(
            receiver.release_next(lapsed_ms)?,
            FrameRelease::Lapsed(dropped) if dropped == carrier
        ));
    }
    assert!(matches!(
        receiver.release_next(lapsed_ms)?,
        FrameRelease::Drained
    ));
    Ok(())
}
