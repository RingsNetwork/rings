//! Laws of the two link tables, checked over explicit instants: every step takes the instant it
//! is judged at, and no test waits. Each end reads its own clock; a test that relates the two
//! names the real instant and derives each end's reading from it.

use std::collections::BTreeMap;

use super::past_grace;
use super::signing_horizon_ms;
use super::within_margin;
use super::Confirmation;
use super::DelegationLessee;
use super::DelegationLessor;
use super::Digests;
use super::Resolution;
use super::ResolvedFrame;
use super::LEASE_GRACE_MS;
use super::LEASE_MARGIN_MS;
use super::LEASE_MS;
use super::LESSEE_TABLE_CAPACITY;
use super::LESSOR_TABLE_CAPACITY;
use super::LINK_PEER_RESERVE;
use crate::delegation::DelegateeKey;
use crate::delegation::Delegation;
use crate::dht::delivery::NextHop;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
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
use crate::tests::splitmix64;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

/// A session lifetime shorter than the hop's, so one slot can near its expiry alone.
const SHORT_SESSION_TTL_MS: u64 = 60_000;
/// The generation the tests' link runs under.
const GENERATION: u64 = 3;
/// The leases the origin slot may fill in the receiver's table.
const ORIGIN_ROOM: usize = LESSOR_TABLE_CAPACITY - LINK_PEER_RESERVE;
/// A clock offset far beyond any lease: ten minutes, either way.
const CLOCK_OFFSET_MS: i128 = 10 * 60 * 1000;

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

/// The frame a receiver decodes when `payload` is sent with its slots as `encoding` says.
fn received(
    payload: &MessagePayload,
    encoding: PerSlot<SlotEncoding>,
) -> Result<Box<WirePayload<'static>>> {
    let bytes = WirePayload::encoded(payload, encoding)?.to_wire()?;
    match LinkFrame::from_wire(bytes.as_ref())? {
        LinkFrame::Payload(frame) => Ok(frame),
        LinkFrame::Control(control) => panic!("expected a payload frame, decoded {control:?}"),
    }
}

/// `session` by reference.
fn by_digest(session: &Delegation) -> Result<DelegationRef<'static>> {
    session.digest().map(DelegationRef::Digest)
}

/// How each slot of the frame a sender in state `sender` sends at `now_ms` for `payload`
/// travels.
fn sent_encoding(
    sender: &mut DelegationLessee,
    payload: &MessagePayload,
    now_ms: u128,
) -> Result<PerSlot<SlotEncoding>> {
    Ok(sender
        .encode(GENERATION, payload, now_ms)?
        .map(|slot| slot.encoding()))
}

/// The frame a sender in state `sender` puts on the wire at `now_ms` for `payload`.
fn sent(
    sender: &mut DelegationLessee,
    payload: &MessagePayload,
    now_ms: u128,
) -> Result<Box<WirePayload<'static>>> {
    let encoding = sent_encoding(sender, payload, now_ms)?;
    received(payload, encoding)
}

/// The confirmation of a lease of `session` with `remaining_ms` left.
fn lease_of(session: &DelegateeKey, remaining_ms: u128) -> Result<Confirmation> {
    Ok(Confirmation {
        digest: session.delegation().digest()?,
        remaining_ms,
    })
}

/// Deliver `confirmation` to `sender` at `now_ms` on its clock, as `Known` on the current link.
fn grant(sender: &mut DelegationLessee, confirmation: Confirmation, now_ms: u128) {
    sender.observe(GENERATION, &LinkControl::from(confirmation), now_ms);
}

/// Resolve `frame`, which must resolve.
fn resolved(receiver: &DelegationLessor, frame: Box<WirePayload<'static>>) -> ResolvedFrame {
    match receiver.resolve(frame) {
        Resolution::Resolved(resolved) => *resolved,
        Resolution::Missed(missing) => panic!("expected the frame to resolve, missed {missing:?}"),
    }
}

/// The digests `frame` misses at `receiver`, which must miss.
fn missed(receiver: &DelegationLessor, frame: Box<WirePayload<'static>>) -> Digests {
    match receiver.resolve(frame) {
        Resolution::Resolved(_) => panic!("expected the frame to miss, it resolved"),
        Resolution::Missed(missing) => missing,
    }
}

/// Verify a resolved frame and learn from it at `now_ms`, as the shell does: the leases to
/// confirm.
fn admit(
    receiver: &mut DelegationLessor,
    resolved: ResolvedFrame,
    now_ms: u128,
) -> Result<Vec<Confirmation>> {
    let ResolvedFrame { payload, encoding } = resolved;
    assert!(payload.verify_transaction_and_payload(TEST_NETWORK_ID));
    receiver.admit_verified(&payload, encoding, now_ms)
}

/// Resolve, verify and learn from `frame` at `now_ms`, which must resolve: the leases to
/// confirm.
fn arrive(
    receiver: &mut DelegationLessor,
    frame: Box<WirePayload<'static>>,
    now_ms: u128,
) -> Result<Vec<Confirmation>> {
    let resolved = resolved(receiver, frame);
    admit(receiver, resolved, now_ms)
}

/// Deliver a fresh origin relayed by `hop`, both inline, at `now_ms`: the leases it earned.
fn press(
    receiver: &mut DelegationLessor,
    hop: &DelegateeKey,
    now_ms: u128,
) -> Result<Vec<Confirmation>> {
    let payload = relayed_payload(&session()?, hop, 0)?;
    arrive(receiver, received(&payload, BOTH_INLINE)?, now_ms)
}

/// Fill the origin share of `receiver`'s table at `now_ms` with fresh origins relayed by `hop`.
/// Post: `leased_len = ORIGIN_ROOM`.
fn fill_origin_share(
    receiver: &mut DelegationLessor,
    hop: &DelegateeKey,
    now_ms: u128,
) -> Result<()> {
    while receiver.leased_len() < ORIGIN_ROOM {
        let before = receiver.leased_len();
        press(receiver, hop, now_ms)?;
        assert!(
            receiver.leased_len() > before,
            "a table with room refused a lease"
        );
    }
    assert_eq!(receiver.leased_len(), ORIGIN_ROOM);
    Ok(())
}

/// The digests confirmed by `confirmations`.
fn confirmed(confirmations: &[Confirmation]) -> Digests {
    confirmations
        .iter()
        .map(|confirmation| confirmation.digest)
        .collect()
}

/// The reading at real instant `real_ms` of a clock `offset_ms` off real time.
fn reading(real_ms: u128, offset_ms: i128) -> u128 {
    real_ms.saturating_add_signed(offset_ms)
}

/// Law (lease, sending end): a session is referenced only once the peer leased it; until then
/// every frame carries it inline, however many were sent.
#[test]
fn test_sender_references_only_leased_sessions() -> Result<()> {
    let now_ms = get_epoch_ms();
    let (origin, hop) = (session()?, session()?);
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = DelegationLessee::new();

    for _ in 0..3 {
        assert_eq!(sent_encoding(&mut sender, &payload, now_ms)?, BOTH_INLINE);
    }
    grant(&mut sender, lease_of(&origin, LEASE_MS)?, now_ms);
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        ORIGIN_REFERENCED
    );
    grant(&mut sender, lease_of(&hop, LEASE_MS)?, now_ms);
    assert_eq!(sender.encode(GENERATION, &payload, now_ms)?, PerSlot {
        origin: by_digest(&origin.delegation())?,
        hop: by_digest(&hop.delegation())?,
    });
    Ok(())
}

/// Law (admission, sending end): a lease of a digest this end never sent inline marks nothing.
#[test]
fn test_a_lease_of_an_unsent_session_marks_nothing() -> Result<()> {
    let now_ms = get_epoch_ms();
    let (node, stranger) = (session()?, session()?);
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = DelegationLessee::new();

    grant(&mut sender, lease_of(&stranger, LEASE_MS)?, now_ms);
    grant(&mut sender, lease_of(&node, LEASE_MS)?, now_ms);
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
    let mut sender = DelegationLessee::new();

    let first = sender.encode(GENERATION, &payload, now_ms)?;
    let first_size = WirePayload::view(&payload, first).wire_size()?;
    grant(&mut sender, lease_of(&origin, LEASE_MS)?, now_ms);
    grant(&mut sender, lease_of(&hop, LEASE_MS)?, now_ms);
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
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = DelegationLessee::new();
    sent(&mut sender, &payload, now_ms)?;
    grant(&mut sender, lease_of(&node, LEASE_MS)?, now_ms);
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        BOTH_REFERENCED
    );

    let next = sender.encode(GENERATION + 1, &payload, now_ms)?;
    assert_eq!(next, payload.delegations().map(DelegationRef::inline));
    grant(&mut sender, lease_of(&node, LEASE_MS)?, now_ms);
    let stale = sender.encode(GENERATION, &payload, now_ms)?;
    assert_eq!(stale, payload.delegations().map(DelegationRef::inline));
    assert_eq!(sent_encoding(&mut sender, &payload, now_ms)?, BOTH_INLINE);
    Ok(())
}

/// Law (bound, sending end): the sender remembers at most `LESSEE_TABLE_CAPACITY` sessions,
/// and the one referenced least recently is the one it forgets: a lease of it then marks
/// nothing, and it travels inline.
#[test]
fn test_sender_table_is_bounded_and_forgets_least_recently_referenced() -> Result<()> {
    let now_ms = get_epoch_ms();
    let (hop, first_origin) = (session()?, session()?);
    let first = relayed_payload(&first_origin, &hop, 0)?;
    let mut sender = DelegationLessee::new();
    sent(&mut sender, &first, now_ms)?;

    // `hop` is referenced by every frame, so it stays; `first_origin` is never referenced again.
    for _ in 0..LESSEE_TABLE_CAPACITY {
        sent(&mut sender, &relayed_payload(&session()?, &hop, 0)?, now_ms)?;
    }
    grant(&mut sender, lease_of(&first_origin, LEASE_MS)?, now_ms);
    grant(&mut sender, lease_of(&hop, LEASE_MS)?, now_ms);
    assert_eq!(sent_encoding(&mut sender, &first, now_ms)?, PerSlot {
        origin: SlotEncoding::Inline,
        hop: SlotEncoding::Referenced,
    });
    Ok(())
}

/// Law (lease, margin): a lease received at `t` with `r` left runs until `t + r` on the
/// sender's clock; the sender stops referencing it `Δ` before then, and a stale, shorter grant
/// never shortens it.
#[test]
fn test_sender_references_only_until_a_margin_before_the_lease_ends() -> Result<()> {
    let receipt_ms = get_epoch_ms();
    let node = session()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = DelegationLessee::new();
    sent(&mut sender, &payload, receipt_ms)?;
    grant(&mut sender, lease_of(&node, LEASE_MS)?, receipt_ms);
    grant(&mut sender, lease_of(&node, LEASE_MARGIN_MS)?, receipt_ms);

    let last_reference_ms = receipt_ms + LEASE_MS - LEASE_MARGIN_MS - 1;
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

/// Law (repair, sending end): a session the peer disclaims on the current link travels inline
/// again, until the peer leases it anew; a disclaimer about another generation changes nothing.
#[test]
fn test_a_disclaimed_session_travels_inline_until_leased_again() -> Result<()> {
    let now_ms = get_epoch_ms();
    let node = session()?;
    let unknown = LinkControl::Unknown(node.delegation().digest()?);
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = DelegationLessee::new();
    sent(&mut sender, &payload, now_ms)?;
    grant(&mut sender, lease_of(&node, LEASE_MS)?, now_ms);

    sender.observe(GENERATION + 1, &unknown, now_ms);
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        BOTH_REFERENCED
    );
    sender.observe(GENERATION, &unknown, now_ms);
    assert_eq!(sent_encoding(&mut sender, &payload, now_ms)?, BOTH_INLINE);
    grant(&mut sender, lease_of(&node, LEASE_MS)?, now_ms);
    assert_eq!(
        sent_encoding(&mut sender, &payload, now_ms)?,
        BOTH_REFERENCED
    );
    Ok(())
}

/// Law (expiry, sending end): a frame is never sent with a session that expires within `Δ`,
/// leased or not, whichever slot it fills. Every instant derives from the session's own expiry.
#[test]
fn test_sender_refuses_a_session_that_expires_within_the_margin() -> Result<()> {
    let (origin, hop) = (delegatee_key_with_ttl(SHORT_SESSION_TTL_MS)?, session()?);
    let expires_at_ms = origin.delegation().expires_at_ms();
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = DelegationLessee::new();
    let last_ms = expires_at_ms - LEASE_MARGIN_MS - 1;
    sent(&mut sender, &payload, last_ms)?;
    grant(&mut sender, lease_of(&origin, LEASE_MS)?, last_ms);
    assert_eq!(
        sent_encoding(&mut sender, &payload, last_ms)?,
        ORIGIN_REFERENCED
    );

    assert!(matches!(
        sender.encode(GENERATION, &payload, last_ms + 1),
        Err(Error::DelegationExpiresWithinMargin)
    ));
    Ok(())
}

/// Law (lease, receiving end): the receiver leases every inline session of a verified frame
/// for `min(expiry − now, LEASE)`, confirms it again on every inline arrival, and resolves a
/// reference to it.
#[test]
fn test_receiver_leases_inline_sessions_and_resolves_references() -> Result<()> {
    let now_ms = get_epoch_ms();
    let (origin, hop) = (session()?, session()?);
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut sender = DelegationLessee::new();
    let mut receiver = DelegationLessor::new();

    let leases = arrive(&mut receiver, sent(&mut sender, &payload, now_ms)?, now_ms)?;
    assert_eq!(leases, vec![
        lease_of(&origin, LEASE_MS)?,
        lease_of(&hop, LEASE_MS)?
    ]);
    // Sent inline again (the confirmation still in flight), it is confirmed again, once.
    let again = arrive(&mut receiver, sent(&mut sender, &payload, now_ms)?, now_ms)?;
    assert_eq!(again, leases);
    assert_eq!(receiver.leased_len(), 2);

    for confirmation in leases {
        grant(&mut sender, confirmation, now_ms);
    }
    let referenced = sent(&mut sender, &payload, now_ms)?;
    assert_eq!(arrive(&mut receiver, referenced, now_ms)?, Vec::new());
    Ok(())
}

/// Law (lease, bound): a peer's own frame carries one delegation in both slots; it is leased
/// and confirmed once, as the hop's, so it finds room in the peer reserve when relayed origins
/// fill the rest of the table.
#[test]
fn test_a_diagonal_frame_is_leased_once_as_the_hop_within_the_reserve() -> Result<()> {
    let now_ms = get_epoch_ms();
    let mut receiver = DelegationLessor::new();
    fill_origin_share(&mut receiver, &session()?, now_ms)?;
    let refused = relayed_payload(&session()?, &session()?, 0)?;
    assert_eq!(
        arrive(&mut receiver, received(&refused, BOTH_INLINE)?, now_ms)?.len(),
        1
    );

    let node = session()?;
    let own = relayed_payload(&node, &node, 0)?;
    let leases = arrive(&mut receiver, received(&own, BOTH_INLINE)?, now_ms)?;
    assert_eq!(leases, vec![lease_of(&node, LEASE_MS)?]);
    assert_eq!(receiver.leased_len(), ORIGIN_ROOM + 2);
    Ok(())
}

/// Law (lease, relative): a lease is a duration, so the lessee holds it until its receipt plus
/// what was left, on its own clock, and no clock offset between the two ends, ten minutes
/// either way, makes an honest reference miss. A confirmation that crosses in `d` and a
/// reference that crosses in `T` with `d + T < Δ + G` resolve even under table pressure at
/// arrival; two milliseconds later, pressure reclaims the lease and the reference misses.
#[test]
fn test_a_lease_runs_from_its_receipt_on_the_lessee_clock() -> Result<()> {
    let real_ms = get_epoch_ms();
    for offset_ms in [-CLOCK_OFFSET_MS, 0, CLOCK_OFFSET_MS] {
        // The sender's clock reads real time; the receiver's is `offset_ms` off it.
        let receiver_clock = |real_ms: u128| reading(real_ms, offset_ms);
        let (origin, hop) = (session()?, session()?);
        let payload = relayed_payload(&origin, &hop, 0)?;
        let mut sender = DelegationLessee::new();
        let mut receiver = DelegationLessor::new();
        let granted = arrive(
            &mut receiver,
            sent(&mut sender, &payload, real_ms)?,
            receiver_clock(real_ms),
        )?;
        let receipt_ms = real_ms + LEASE_MARGIN_MS;
        for confirmation in granted {
            grant(&mut sender, confirmation, receipt_ms);
        }

        let last_ms = receipt_ms + LEASE_MS - LEASE_MARGIN_MS - 1;
        let last = sent(&mut sender, &payload, last_ms)?;
        assert_eq!(
            last.delegation_refs().map(|slot| slot.encoding()),
            BOTH_REFERENCED
        );
        assert_eq!(
            sent_encoding(&mut sender, &payload, last_ms + 1)?.origin,
            SlotEncoding::Inline
        );

        // `d + T = Δ + G − 1`: fill the table with live leases at arrival, then press once more.
        let arrival_ms = last_ms + LEASE_GRACE_MS;
        let filler_hop = session()?;
        fill_origin_share(&mut receiver, &filler_hop, receiver_clock(arrival_ms))?;
        let pressed = press(&mut receiver, &filler_hop, receiver_clock(arrival_ms))?;
        assert_eq!(
            confirmed(&pressed),
            Digests::from([filler_hop.delegation().digest()?])
        );
        let resolved_frame = resolved(&receiver, last);
        assert_eq!(resolved_frame.payload, payload);

        let late = received(&payload, ORIGIN_REFERENCED)?;
        press(&mut receiver, &filler_hop, receiver_clock(arrival_ms + 2))?;
        assert_eq!(
            missed(&receiver, late),
            Digests::from([origin.delegation().digest()?])
        );
    }
    Ok(())
}

/// Law (admission, receiving end): resolving a frame teaches the table nothing; only
/// [`DelegationLessor::admit_verified`] leases, after verification.
#[test]
fn test_resolution_alone_teaches_nothing() -> Result<()> {
    let node = session()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let receiver = DelegationLessor::new();

    resolved(&receiver, received(&payload, BOTH_INLINE)?);
    assert_eq!(receiver.leased_len(), 0);
    assert_eq!(
        missed(&receiver, received(&payload, BOTH_REFERENCED)?),
        Digests::from([node.delegation().digest()?])
    );
    Ok(())
}

/// Law (lease, renewal): a verified frame whose reference finds less than half a lease left
/// renews it, and the sender learns the longer lease; one that finds more than half renews
/// nothing.
#[test]
fn test_a_verified_reference_renews_a_lease_with_less_than_half_left() -> Result<()> {
    let granted_ms = get_epoch_ms();
    let node = session()?;
    let payload = relayed_payload(&node, &node, 0)?;
    let mut sender = DelegationLessee::new();
    let mut receiver = DelegationLessor::new();
    for confirmation in arrive(
        &mut receiver,
        sent(&mut sender, &payload, granted_ms)?,
        granted_ms,
    )? {
        grant(&mut sender, confirmation, granted_ms);
    }

    let early_ms = granted_ms + LEASE_MS / 2 - 1;
    let early = sent(&mut sender, &payload, early_ms)?;
    assert_eq!(arrive(&mut receiver, early, early_ms)?, Vec::new());
    let late_ms = granted_ms + LEASE_MS / 2 + 1;
    let late = sent(&mut sender, &payload, late_ms)?;
    let renewed = arrive(&mut receiver, late, late_ms)?;
    assert_eq!(renewed, vec![lease_of(&node, LEASE_MS)?]);

    let old_end_ms = granted_ms + LEASE_MS - LEASE_MARGIN_MS;
    assert_eq!(
        sent_encoding(&mut sender, &payload, old_end_ms)?,
        BOTH_INLINE
    );
    for confirmation in renewed {
        grant(&mut sender, confirmation, late_ms);
    }
    assert_eq!(
        sent_encoding(&mut sender, &payload, old_end_ms)?,
        BOTH_REFERENCED
    );
    Ok(())
}

/// Law (residence): a lease far past its end and grace is not reclaimed while the table has
/// room, so a reference to it still resolves, however late it is judged.
#[test]
fn test_an_ended_lease_resolves_while_the_table_has_room() -> Result<()> {
    let granted_ms = get_epoch_ms();
    let (origin, hop) = (session()?, session()?);
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut receiver = DelegationLessor::new();
    arrive(&mut receiver, received(&payload, BOTH_INLINE)?, granted_ms)?;

    let much_later_ms = granted_ms + 10 * (LEASE_MS + LEASE_GRACE_MS);
    press(&mut receiver, &session()?, much_later_ms)?;
    let resolved_frame = resolved(&receiver, received(&payload, BOTH_REFERENCED)?);
    assert_eq!(resolved_frame.payload, payload);
    Ok(())
}

/// Law (residence, pressure): under a full table, a new inline delegation reclaims the leases
/// past their grace, and a later reference to one of them misses.
#[test]
fn test_table_pressure_reclaims_an_ended_lease_and_its_reference_misses() -> Result<()> {
    let granted_ms = get_epoch_ms();
    let (origin, hop) = (session()?, session()?);
    let payload = relayed_payload(&origin, &hop, 0)?;
    let mut receiver = DelegationLessor::new();
    arrive(&mut receiver, received(&payload, BOTH_INLINE)?, granted_ms)?;
    fill_origin_share(&mut receiver, &session()?, granted_ms)?;

    let reclaimable_ms = granted_ms + LEASE_MS + LEASE_GRACE_MS + 1;
    let pressing = session()?;
    let leases = press(&mut receiver, &pressing, reclaimable_ms)?;
    assert_eq!(leases.len(), 2);
    assert_eq!(receiver.leased_len(), 2);
    assert_eq!(
        missed(&receiver, received(&payload, BOTH_REFERENCED)?),
        Digests::from([origin.delegation().digest()?, hop.delegation().digest()?])
    );
    Ok(())
}

/// Law (bound, residence): a live lease is never reclaimed. A table full of leases at the last
/// instant of their grace refuses, and so does not confirm, a new origin, and every lease it
/// holds still resolves; the hop slot still finds the peer reserve.
#[test]
fn test_a_live_lease_is_never_reclaimed() -> Result<()> {
    let granted_ms = get_epoch_ms();
    let hop = session()?;
    let mut receiver = DelegationLessor::new();
    let first = relayed_payload(&session()?, &hop, 0)?;
    arrive(&mut receiver, received(&first, BOTH_INLINE)?, granted_ms)?;
    fill_origin_share(&mut receiver, &hop, granted_ms)?;

    let last_grace_ms = granted_ms + LEASE_MS + LEASE_GRACE_MS;
    let late_origin = session()?;
    let refused = relayed_payload(&late_origin, &hop, 0)?;
    let leases = arrive(
        &mut receiver,
        received(&refused, BOTH_INLINE)?,
        last_grace_ms,
    )?;
    assert!(!confirmed(&leases).contains(&late_origin.delegation().digest()?));
    assert_eq!(receiver.leased_len(), ORIGIN_ROOM);
    resolved(&receiver, received(&first, BOTH_REFERENCED)?);

    let own = session()?;
    let own_frame = relayed_payload(&own, &own, 0)?;
    let leases = arrive(
        &mut receiver,
        received(&own_frame, BOTH_INLINE)?,
        last_grace_ms,
    )?;
    assert_eq!(leases, vec![lease_of(&own, LEASE_MS)?]);
    Ok(())
}

/// One event of the seeded walk, at the real instant it is keyed by.
enum Event {
    /// The sender encodes a frame for the payload at this index.
    Send(usize),
    /// The receiver resolves the frame sent at `sent_ms`, after its transit and its dwell in
    /// the receiver's lane.
    Process {
        frame: Box<WirePayload<'static>>,
        sent_ms: u128,
    },
    /// A control frame reaches the sender.
    Control(LinkControl),
}

/// The seeded schedule of the walk: events keyed by real instant, ties in scheduling order.
struct Schedule {
    events: BTreeMap<(u128, u64), Event>,
    scheduled: u64,
    seed: u64,
}

impl Schedule {
    /// An empty schedule drawing from `seed`.
    const fn new(seed: u64) -> Self {
        Self {
            events: BTreeMap::new(),
            scheduled: 0,
            seed,
        }
    }

    /// A draw uniform enough over `0..bound`.
    fn below(&mut self, bound: u128) -> u128 {
        self.seed = splitmix64(self.seed);
        u128::from(self.seed) % bound.max(1)
    }

    /// A draw over the indices of a slice of `len` items.
    fn index(&mut self, len: usize) -> usize {
        let bound = u128::try_from(len).unwrap_or(u128::MAX);
        usize::try_from(self.below(bound)).unwrap_or(0)
    }

    /// Schedule `event` at real instant `at_ms`.
    fn at(&mut self, at_ms: u128, event: Event) {
        self.scheduled += 1;
        self.events.insert((at_ms, self.scheduled), event);
    }

    /// The earliest event and its instant.
    fn next(&mut self) -> Option<(u128, Event)> {
        self.events
            .pop_first()
            .map(|((at_ms, _), event)| (at_ms, event))
    }
}

/// The seeded walk behind the soundness test, with the receiver's clock `offset_ms` off the
/// sender's. Post: how many frames resolved although they took longer than `G` from send to
/// resolution.
fn walk(seed: u64, offset_ms: i128) -> Result<usize> {
    /// Sessions in play: more than both tables hold.
    const ORIGINS: usize = LESSOR_TABLE_CAPACITY + LESSEE_TABLE_CAPACITY;
    /// Frames the sender sends.
    const SENDS: usize = 600;
    /// The longest gap between two sends.
    const MAX_GAP_MS: u128 = 1_000;
    /// The longest transit of a payload frame.
    const MAX_TRANSIT_MS: u128 = LEASE_GRACE_MS;
    /// The longest transit of a control frame: it is dropped after one delivery bound.
    const MAX_CONTROL_TRANSIT_MS: u128 = LEASE_MARGIN_MS;

    let hop = session()?;
    let payloads = (0..ORIGINS)
        .map(|_| relayed_payload(&session()?, &hop, 0))
        .collect::<Result<Vec<_>>>()?;
    let mut sender = DelegationLessee::new();
    let mut receiver = DelegationLessor::new();
    let mut schedule = Schedule::new(seed);
    // The end of each lease the receiver confirmed, on its clock: what a miss must have passed.
    let mut granted: BTreeMap<_, u128> = BTreeMap::new();
    let mut sends = 0;
    let mut late_resolved = 0;
    let mut fullest = 0;
    schedule.at(get_epoch_ms(), Event::Send(0));

    while let Some((now_ms, event)) = schedule.next() {
        match event {
            Event::Send(index) => {
                sends += 1;
                if sends < SENDS {
                    // Mostly a working set, often a cold session, so both tables churn.
                    let next = if schedule.below(2) == 0 {
                        schedule.index(ORIGINS)
                    } else {
                        schedule.index(16)
                    };
                    let gap_ms = schedule.below(MAX_GAP_MS);
                    schedule.at(now_ms + gap_ms, Event::Send(next));
                }
                let Some(payload) = payloads.get(index) else {
                    continue;
                };
                let frame = sent(&mut sender, payload, now_ms)?;
                // Lost now and then; otherwise any transit, so frames reorder.
                if schedule.below(10) != 0 {
                    let transit_ms = schedule.below(MAX_TRANSIT_MS + 1);
                    // Now and then the frame dwells in the receiver's lane for minutes.
                    let dwell_ms = if schedule.below(32) == 0 {
                        schedule.below(2 * LEASE_MS)
                    } else {
                        schedule.below(LEASE_GRACE_MS)
                    };
                    schedule.at(now_ms + transit_ms + dwell_ms, Event::Process {
                        frame,
                        sent_ms: now_ms,
                    });
                }
            }
            Event::Process { frame, sent_ms } => {
                let receiver_ms = reading(now_ms, offset_ms);
                let within_bound = now_ms - sent_ms <= LEASE_GRACE_MS;
                let controls = match receiver.resolve(frame) {
                    Resolution::Resolved(resolved) => {
                        let referenced = resolved.encoding != BOTH_INLINE;
                        late_resolved += usize::from(referenced && !within_bound);
                        let leases = admit(&mut receiver, *resolved, receiver_ms)?;
                        for lease in &leases {
                            let end_ms = receiver_ms + lease.remaining_ms;
                            granted
                                .entry(lease.digest)
                                .and_modify(|held: &mut u128| *held = (*held).max(end_ms))
                                .or_insert(end_ms);
                        }
                        leases
                            .into_iter()
                            .map(LinkControl::from)
                            .collect::<Vec<_>>()
                    }
                    Resolution::Missed(missing) => {
                        assert!(!within_bound, "an honest reference within the bound missed");
                        for digest in &missing {
                            let end_ms = granted.get(digest).copied();
                            assert!(
                                end_ms.is_some_and(|end_ms| past_grace(end_ms, receiver_ms)),
                                "a reference missed a lease that was not reclaimable"
                            );
                        }
                        missing.into_iter().map(LinkControl::Unknown).collect()
                    }
                };
                fullest = fullest.max(receiver.leased_len());
                // A control frame may be lost, duplicated, or delayed past later ones.
                for control in controls {
                    for _ in 0..schedule.below(3) {
                        let transit_ms = schedule.below(MAX_CONTROL_TRANSIT_MS + 1);
                        schedule.at(now_ms + transit_ms, Event::Control(control.clone()));
                    }
                }
            }
            Event::Control(control) => sender.observe(GENERATION, &control, now_ms),
        }
    }
    assert!(fullest <= LESSOR_TABLE_CAPACITY);
    assert_eq!(
        fullest, ORIGIN_ROOM,
        "the walk never put the table under pressure"
    );
    Ok(late_resolved)
}

/// Law (lease and residence, soundness) as seeded walks: a sender and a receiver whose clocks
/// are off by up to ten minutes either way exchange frames and control frames over a link that
/// loses, duplicates and reorders both, under more sessions than either table holds, with
/// every control frame crossing within `Δ` and frames dwelling in the receiver's lane for
/// arbitrary times. A reference that reaches resolution within `G` of its send always
/// resolves; one that misses missed a lease past its grace, reclaimed under pressure; and
/// frames that waited far longer still resolve while their lease stayed resident.
#[test]
fn test_an_honest_reference_never_misses_under_loss_reordering_and_pressure() -> Result<()> {
    let mut seed = 0x0908_u64;
    let span = CLOCK_OFFSET_MS.unsigned_abs() * 2 + 1;
    let drawn = i128::try_from(u128::from(splitmix64(seed)) % span).unwrap_or(0);
    for offset_ms in [-CLOCK_OFFSET_MS, CLOCK_OFFSET_MS, drawn - CLOCK_OFFSET_MS] {
        seed = splitmix64(seed);
        assert!(walk(seed, offset_ms)? > 0, "no late frame resolved");
    }
    Ok(())
}

/// Law (horizon, #1063): `now ≥ signing_horizon_ms(e)` exactly when the sender's margin refuses
/// a frame carrying a delegation that expires at `e`, for expiries below, at and past the margin
/// and every instant around each horizon.
#[test]
fn test_signing_horizon_is_where_the_margin_refuses() {
    let margin = LEASE_MARGIN_MS;
    for expires_at_ms in [0, 1, margin - 1, margin, margin + 1, 3 * margin, u128::MAX] {
        let horizon = signing_horizon_ms(expires_at_ms);
        for now_ms in [
            0,
            horizon.saturating_sub(1),
            horizon,
            horizon.saturating_add(1),
        ] {
            assert_eq!(
                now_ms >= horizon,
                within_margin(expires_at_ms, now_ms),
                "e = {expires_at_ms}, now = {now_ms}"
            );
        }
    }
}
