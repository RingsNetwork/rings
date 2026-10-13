//! Laws of the two link tables, checked over explicit instants: every step takes the instant it
//! is judged at, and no test waits.

use super::AnnouncedDelegations;
use super::Confirmation;
use super::Digests;
use super::ReferencedDelegations;
use super::ResolvedFrame;
use super::ANNOUNCED_TABLE_CAPACITY;
use super::LEASE_GRACE_MS;
use super::LEASE_MARGIN_MS;
use super::LEASE_MS;
use super::LINK_PEER_RESERVE;
use super::REFERENCED_TABLE_CAPACITY;
use crate::delegation::DelegateeKey;
use crate::delegation::Delegation;
use crate::dht::delivery::NextHop;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::DelegationRef;
use crate::message::HopBudget;
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
use crate::tests::splitmix64;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

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

/// A fresh session of a fresh account.
fn session() -> Result<DelegateeKey> {
    DelegateeKey::new_with_seckey(&SecretKey::random())
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

/// The frame a sender in state `sender` puts on the wire for `payload`.
fn sent(
    sender: &mut AnnouncedDelegations,
    payload: &MessagePayload,
    now_ms: u128,
) -> Result<Box<WirePayload<'static>>> {
    let sessions = sender.encode(GENERATION, payload, now_ms)?;
    received(payload, sessions)
}

/// How each slot of the frame a sender in state `sender` sends for `payload` travels.
fn sent_encoding(
    sender: &mut AnnouncedDelegations,
    payload: &MessagePayload,
    now_ms: u128,
) -> Result<PerSlot<SlotEncoding>> {
    Ok(sent(sender, payload, now_ms)?
        .delegation_refs()
        .map(DelegationRef::encoding))
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

/// The origin slot by reference, the hop slot inline.
const ORIGIN_REFERENCED: PerSlot<SlotEncoding> = PerSlot {
    origin: SlotEncoding::Referenced,
    hop: SlotEncoding::Inline,
};

/// Lease `session` to `sender` until `until_ms`, as a confirmation from the peer does.
fn lease(sender: &mut AnnouncedDelegations, session: &DelegateeKey, until_ms: u128) -> Result<()> {
    sender.acknowledge(GENERATION, session.delegation().digest()?, until_ms);
    Ok(())
}

/// Resolve `frame` at `now_ms`, which must resolve.
fn resolved(
    receiver: &mut ReferencedDelegations,
    frame: Box<WirePayload<'static>>,
    now_ms: u128,
) -> ResolvedFrame {
    match receiver.resolve(frame, now_ms) {
        Ok(resolved) => resolved,
        Err(missing) => panic!("expected the frame to resolve, it missed {missing:?}"),
    }
}

/// Resolve, verify and learn from `frame` at `now_ms`, as the shell does: the leases to confirm.
fn arrive(
    receiver: &mut ReferencedDelegations,
    frame: Box<WirePayload<'static>>,
    now_ms: u128,
) -> Result<Vec<Confirmation>> {
    let ResolvedFrame {
        payload,
        encoding,
        renewals,
    } = resolved(receiver, frame, now_ms);
    assert!(payload.verify_transaction_and_payload(TEST_NETWORK_ID));
    let leased = receiver.admit_verified(&payload, encoding, now_ms)?;
    Ok(renewals.into_iter().chain(leased).collect())
}

/// The digests confirmed by `confirmations`.
fn confirmed(confirmations: &[Confirmation]) -> Digests {
    confirmations
        .iter()
        .map(|confirmation| confirmation.digest)
        .collect()
}

/// Law (lease, sending end): a session is referenced only once the peer leased it; until then
/// every frame carries it inline, however many were sent.
#[test]
fn test_sender_references_only_leased_sessions() -> Result<()> {
    let now_ms = get_epoch_ms();
    let (origin, hop) = (session()?, session()?);
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = AnnouncedDelegations::new();

    for _ in 0..3 {
        assert_eq!(sent_encoding(&mut sender, &payload, now_ms)?, BOTH_INLINE);
    }
    lease(&mut sender, &origin, now_ms + LEASE_MS)?;
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        ORIGIN_REFERENCED
    );
    lease(&mut sender, &hop, now_ms + LEASE_MS)?;
    assert_eq!(sender.encode(GENERATION, &payload, now_ms)?, PerSlot {
        origin: by_digest(&origin.delegation())?,
        hop: by_digest(&hop.delegation())?,
    });
    Ok(())
}

/// Law (admission, sending end): a lease of a digest this end never announced marks nothing.
#[test]
fn test_unannounced_lease_marks_nothing() -> Result<()> {
    let now_ms = get_epoch_ms();
    let (node, stranger) = (session()?, session()?);
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();

    lease(&mut sender, &stranger, now_ms + LEASE_MS)?;
    lease(&mut sender, &node, now_ms + LEASE_MS)?;
    assert_eq!(sent_encoding(&mut sender, &payload, now_ms)?, BOTH_INLINE);
    Ok(())
}

/// Acceptance: a steady-state frame carries two 20-byte digests where it carried two sessions,
/// and the saving is the two delegations.
#[test]
fn test_steady_state_frame_carries_digests_instead_of_sessions() -> Result<()> {
    let now_ms = get_epoch_ms();
    let (origin, hop) = (session()?, session()?);
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = AnnouncedDelegations::new();

    let first = sender.encode(GENERATION, &payload, now_ms)?;
    let first_size = WirePayload::view(&payload, first).wire_size()?;
    lease(&mut sender, &origin, now_ms + LEASE_MS)?;
    lease(&mut sender, &hop, now_ms + LEASE_MS)?;
    let steady = sender.encode(GENERATION, &payload, now_ms)?;
    let steady_size = WirePayload::view(&payload, steady).wire_size()?;

    let session_bytes = |session: &Delegation| -> Result<usize> {
        Ok(rings_codec::serialize(session)
            .map_err(Error::CodecSerialize)?
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
    let node = session()?;
    let digest = node.delegation().digest()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();
    sent(&mut sender, &payload, now_ms)?;
    lease(&mut sender, &node, now_ms + LEASE_MS)?;
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        BOTH_REFERENCED
    );

    let next = sender.encode(GENERATION + 1, &payload, now_ms)?;
    assert_eq!(next, payload.delegations().map(DelegationRef::inline));
    sender.acknowledge(GENERATION, digest, now_ms + LEASE_MS);
    let stale = sender.encode(GENERATION, &payload, now_ms)?;
    assert_eq!(stale, payload.delegations().map(DelegationRef::inline));
    assert_eq!(sent_encoding(&mut sender, &payload, now_ms)?, BOTH_INLINE);
    Ok(())
}

/// Law (bound, sending end): the sender remembers at most `ANNOUNCED_TABLE_CAPACITY` sessions,
/// and the one referenced least recently is the one it forgets: a lease of it then marks
/// nothing, and it travels inline.
#[test]
fn test_sender_table_is_bounded_and_forgets_least_recently_referenced() -> Result<()> {
    let now_ms = get_epoch_ms();
    let (hop, first_origin) = (session()?, session()?);
    let first = relayed_payload(&first_origin, &hop, 0)?;
    let mut sender = AnnouncedDelegations::new();
    sent(&mut sender, &first, now_ms)?;

    // `hop` is referenced by every frame, so it stays; `first_origin` is never referenced again.
    for _ in 0..ANNOUNCED_TABLE_CAPACITY {
        sent(&mut sender, &relayed_payload(&session()?, &hop, 0)?, now_ms)?;
    }
    lease(&mut sender, &first_origin, now_ms + LEASE_MS)?;
    lease(&mut sender, &hop, now_ms + LEASE_MS)?;
    assert_eq!(sent_encoding(&mut sender, &first, now_ms)?, PerSlot {
        origin: SlotEncoding::Inline,
        hop: SlotEncoding::Referenced,
    });
    Ok(())
}

/// Law (lease, margin): the sender stops referencing a session `Δ` before its lease ends, and a
/// stale lease never shortens the one it holds.
#[test]
fn test_sender_references_only_until_a_margin_before_the_lease_ends() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = session()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();
    sent(&mut sender, &payload, now_ms)?;
    let until_ms = now_ms + LEASE_MS;
    lease(&mut sender, &node, until_ms)?;
    // A stale, shorter lease changes nothing.
    lease(&mut sender, &node, now_ms + LEASE_MARGIN_MS)?;

    let last_reference_ms = until_ms - LEASE_MARGIN_MS - 1;
    assert_eq!(
        sent_encoding(&mut sender, &payload, last_reference_ms)?,
        BOTH_REFERENCED
    );
    assert_eq!(
        sent_encoding(&mut sender, &payload, last_reference_ms + 1)?,
        BOTH_INLINE
    );
    Ok(())
}

/// Law (repair, sending end): a session the peer disclaims travels inline again, until the
/// peer leases it anew.
#[test]
fn test_a_disclaimed_session_travels_inline_until_leased_again() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = session()?;
    let digest = node.delegation().digest()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();
    sent(&mut sender, &payload, now_ms)?;
    lease(&mut sender, &node, now_ms + LEASE_MS)?;
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        BOTH_REFERENCED
    );

    sender.unknown(GENERATION + 1, digest);
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        BOTH_REFERENCED
    );
    sender.unknown(GENERATION, digest);
    assert_eq!(sent_encoding(&mut sender, &payload, now_ms)?, BOTH_INLINE);
    lease(&mut sender, &node, now_ms + LEASE_MS)?;
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        BOTH_REFERENCED
    );
    Ok(())
}

/// Law (expiry, sending end): a frame is never sent with a session that expires within `Δ`,
/// and an expired session is absent from the table.
#[test]
fn test_sender_refuses_a_session_that_expires_within_the_margin() -> Result<()> {
    // The session is stamped before `now_ms`, so `now_ms + ttl - Δ` is within `Δ` of its expiry.
    let node = delegatee_key_with_ttl(SHORT_SESSION_TTL_MS)?;
    let now_ms = get_epoch_ms();
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();
    sent(&mut sender, &payload, now_ms)?;
    lease(&mut sender, &node, now_ms + LEASE_MS)?;
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        BOTH_REFERENCED
    );

    let expiring_ms = now_ms + u128::from(SHORT_SESSION_TTL_MS) - LEASE_MARGIN_MS;
    assert!(matches!(
        sender.encode(GENERATION, &payload, expiring_ms),
        Err(Error::DelegationExpired)
    ));
    Ok(())
}

/// Law (lease, receiving end): the receiver leases every inline session of a verified frame,
/// until `min(expiry, now + LEASE)`, confirms it again on every inline arrival, and resolves a
/// reference to it.
#[test]
fn test_receiver_leases_inline_sessions_and_resolves_references() -> Result<()> {
    let now_ms = get_epoch_ms();
    let (origin, hop) = (session()?, session()?);
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = AnnouncedDelegations::new();
    let mut receiver = ReferencedDelegations::new();

    let leases = arrive(&mut receiver, sent(&mut sender, &payload, now_ms)?, now_ms)?;
    assert_eq!(
        confirmed(&leases),
        Digests::from([origin.delegation().digest()?, hop.delegation().digest()?])
    );
    assert!(leases
        .iter()
        .all(|confirmation| confirmation.until_ms == now_ms + LEASE_MS));
    // Sent inline again (the confirmation still in flight), it is confirmed again, once.
    let again = arrive(&mut receiver, sent(&mut sender, &payload, now_ms)?, now_ms)?;
    assert_eq!(confirmed(&again), confirmed(&leases));
    assert_eq!(receiver.known_len(), 2);

    for confirmation in leases {
        sender.acknowledge(GENERATION, confirmation.digest, confirmation.until_ms);
    }
    let referenced = sent(&mut sender, &payload, now_ms)?;
    assert_eq!(arrive(&mut receiver, referenced, now_ms)?, Vec::new());
    Ok(())
}

/// Law (lease): a peer's own frame carries one delegation in both slots, and it is confirmed
/// once, so one frame spends one link-control send on it.
#[test]
fn test_a_peer_own_frame_confirms_its_delegation_once() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = session()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();
    let mut receiver = ReferencedDelegations::new();

    let leases = arrive(&mut receiver, sent(&mut sender, &payload, now_ms)?, now_ms)?;
    assert_eq!(leases, vec![Confirmation {
        digest: node.delegation().digest()?,
        until_ms: now_ms + LEASE_MS,
    }]);
    Ok(())
}

/// Law (lease, renewal): a reference that finds less than half a lease left renews it, and the
/// sender learns the longer lease; one that finds more renews nothing.
#[test]
fn test_a_reference_renews_a_lease_with_less_than_half_left() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = session()?;
    let digest = node.delegation().digest()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = AnnouncedDelegations::new();
    let mut receiver = ReferencedDelegations::new();
    for confirmation in arrive(&mut receiver, sent(&mut sender, &payload, now_ms)?, now_ms)? {
        sender.acknowledge(GENERATION, confirmation.digest, confirmation.until_ms);
    }

    let early_ms = now_ms + LEASE_MS / 2 - 1;
    let early = sent(&mut sender, &payload, early_ms)?;
    assert!(resolved(&mut receiver, early, early_ms).renewals.is_empty());
    let late_ms = now_ms + LEASE_MS / 2 + 1;
    let late = sent(&mut sender, &payload, late_ms)?;
    assert_eq!(resolved(&mut receiver, late, late_ms).renewals, vec![
        Confirmation {
            digest,
            until_ms: late_ms + LEASE_MS,
        }
    ]);
    Ok(())
}

/// Law (lease, grace and repair): a reference resolves up to `G` past its lease, and misses
/// after; a miss names what it misses and resolves nothing.
#[test]
fn test_a_reference_resolves_within_the_grace_and_misses_after() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = session()?;
    let digest = node.delegation().digest()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut receiver = ReferencedDelegations::new();
    let inline = received(&payload, payload.delegations().map(DelegationRef::inline))?;
    arrive(&mut receiver, inline, now_ms)?;
    let referenced = || -> Result<Box<WirePayload<'static>>> {
        received(&payload, PerSlot {
            origin: by_digest(&node.delegation())?,
            hop: by_digest(&node.delegation())?,
        })
    };

    let last_ms = now_ms + LEASE_MS + LEASE_GRACE_MS;
    resolved(&mut receiver, referenced()?, last_ms);
    // The reference at `last_ms` found less than half a lease left and renewed it, so judge a
    // fresh table for the miss past the grace.
    let mut fresh = ReferencedDelegations::new();
    let inline = received(&payload, payload.delegations().map(DelegationRef::inline))?;
    arrive(&mut fresh, inline, now_ms)?;
    match fresh.resolve(referenced()?, last_ms + 1) {
        Ok(_) => panic!("a reference past its lease and grace must miss"),
        Err(missing) => assert_eq!(missing, Digests::from([digest])),
    }
    assert_eq!(fresh.known_len(), 0);
    Ok(())
}

/// Law (lease, bound): a table full of live leases refuses to lease, and so to confirm, a new
/// session, never evicting a live one; the origin slot leaves `LINK_PEER_RESERVE` entries to the
/// hop slot; a lease that ended makes room again.
#[test]
fn test_a_full_table_refuses_new_leases_and_keeps_the_peer_reserve() -> Result<()> {
    let now_ms = get_epoch_ms();
    let hop = session()?;
    let mut receiver = ReferencedDelegations::new();
    let inline = |payload: &MessagePayload| {
        received(payload, payload.delegations().map(DelegationRef::inline))
    };
    // The hop takes one entry; origins fill the rest of the origin share.
    for _ in 0..(REFERENCED_TABLE_CAPACITY - LINK_PEER_RESERVE - 1) {
        let payload = relayed_payload(&session()?, &hop, 0)?;
        arrive(&mut receiver, inline(&payload)?, now_ms)?;
    }
    assert_eq!(
        receiver.known_len(),
        REFERENCED_TABLE_CAPACITY - LINK_PEER_RESERVE
    );

    let late_origin = session()?;
    let refused = relayed_payload(&late_origin, &hop, 0)?;
    let leases = arrive(&mut receiver, inline(&refused)?, now_ms)?;
    assert!(!confirmed(&leases).contains(&late_origin.delegation().digest()?));
    // The peer's own sessions still find room in the reserve.
    let own = session()?;
    let leases = arrive(
        &mut receiver,
        inline(&relayed_payload(&own, &own, 0)?)?,
        now_ms,
    )?;
    assert!(confirmed(&leases).contains(&own.delegation().digest()?));

    // Past every lease and its grace, the table makes room again.
    let later_ms = now_ms + LEASE_MS + LEASE_GRACE_MS + 1;
    let leases = arrive(&mut receiver, inline(&refused)?, later_ms)?;
    assert!(confirmed(&leases).contains(&late_origin.delegation().digest()?));
    Ok(())
}

/// Law (lease, soundness) as a seeded walk: a sender and a receiver exchange frames and leases
/// over a link that loses, duplicates and reorders both, under more sessions than either table
/// holds and a clock skew, with every transit within `Δ + G` less the skew. Every reference the
/// sender makes resolves at the receiver: nothing is ever held or missed.
#[test]
fn test_an_honest_reference_never_misses_under_loss_reordering_and_pressure() -> Result<()> {
    /// Sessions in play: more than both tables hold.
    const ORIGINS: usize = REFERENCED_TABLE_CAPACITY + ANNOUNCED_TABLE_CAPACITY;
    /// Steps of the walk.
    const STEPS: usize = 4_000;
    /// The receiver's clock runs this far ahead of the sender's.
    const SKEW_MS: u128 = LEASE_GRACE_MS / 2;
    /// The longest transit, so that transit plus skew stays under `Δ + G`.
    const MAX_TRANSIT_MS: u128 = LEASE_MARGIN_MS + LEASE_GRACE_MS - SKEW_MS - 1;

    let hop = session()?;
    let payloads = (0..ORIGINS)
        .map(|_| relayed_payload(&session()?, &hop, 0))
        .collect::<Result<Vec<_>>>()?;
    let start_ms = get_epoch_ms();
    let mut sender = AnnouncedDelegations::new();
    let mut receiver = ReferencedDelegations::new();
    // In flight, toward the receiver and toward the sender, each with its arrival instant on
    // the sender's clock.
    let mut frames: Vec<(u128, Box<WirePayload<'static>>)> = Vec::new();
    let mut leases: Vec<(u128, Confirmation)> = Vec::new();
    let mut now_ms = start_ms;
    let mut seed = 0x0908_u64;
    let mut draw = |bound: u64| {
        seed = splitmix64(seed);
        seed % bound
    };

    for _ in 0..STEPS {
        now_ms += u128::from(draw(5_000));
        // Mostly a working set, sometimes a cold session, so both tables churn.
        let index = if draw(4) == 0 {
            usize::try_from(draw(ORIGINS as u64)).unwrap_or(0)
        } else {
            usize::try_from(draw(16)).unwrap_or(0)
        };
        if let Some(payload) = payloads.get(index) {
            let transit = u128::from(draw(MAX_TRANSIT_MS as u64 + 1));
            frames.push((now_ms + transit, sent(&mut sender, payload, now_ms)?));
        }
        // Deliver what has arrived, earliest first (varying transits reorder the frames), losing
        // some; the receiver's clock only moves forward.
        while let Some(position) = earliest_arrived(&frames, now_ms) {
            let (at, frame) = frames.swap_remove(position);
            if draw(10) == 0 {
                continue;
            }
            let receiver_ms = at + SKEW_MS;
            let ResolvedFrame {
                payload,
                encoding,
                renewals,
            } = match receiver.resolve(frame, receiver_ms) {
                Ok(resolved) => resolved,
                Err(missing) => panic!("an honest reference missed {missing:?}"),
            };
            let leased = receiver.admit_verified(&payload, encoding, receiver_ms)?;
            for confirmation in renewals.into_iter().chain(leased) {
                // A lease may be lost, duplicated, or delayed past later ones.
                for _ in 0..draw(3) {
                    let delay = u128::from(draw(MAX_TRANSIT_MS as u64 + 1));
                    leases.push((at + delay, confirmation));
                }
            }
        }
        while let Some(position) = earliest_arrived(&leases, now_ms) {
            let (_, confirmation) = leases.swap_remove(position);
            sender.acknowledge(GENERATION, confirmation.digest, confirmation.until_ms);
        }
    }
    assert!(receiver.known_len() <= REFERENCED_TABLE_CAPACITY);
    Ok(())
}

/// The position of the earliest of `in_flight` that has arrived by `now_ms`.
fn earliest_arrived<T>(in_flight: &[(u128, T)], now_ms: u128) -> Option<usize> {
    in_flight
        .iter()
        .enumerate()
        .filter(|(_, (at, _))| *at <= now_ms)
        .min_by_key(|(_, (at, _))| *at)
        .map(|(position, _)| position)
}
