//! The rate floor of an open message (load-bounds paper, Def. 7, Law 4, Props. 4 and 5), driven
//! through the reassembler with an injected clock and an injected reading of the credit the
//! sender holds unused on its carrier.

use rings_transport::core::pool::ChannelLane;

use super::*;

/// `ρ` of these tests, in bytes per millisecond.
const RATE: u64 = 1;
/// `W`, the grace in chunks.
const GRACE: u64 = 2;
/// `d₀`, the chunk length.
const CHUNK: usize = 16;
/// `Δ`, the sweep period, in milliseconds: `ρ·Δ` stays below the credit a withholding sender
/// holds, `W·d₀`, so each sweep advances the clock by its full period.
const SWEEP_MS: u128 = 10;

/// Limits with the rate floor of these tests and room for one message of four chunks.
fn floor_limits() -> ReassemblyLimits {
    ReassemblyLimits {
        max_pending_messages: 4,
        max_chunk_data_len: CHUNK,
        max_message_bytes: 4 * CHUNK + 1,
        max_chunks_per_message: 8,
        max_completed_ids: 8,
        rate_floor: RATE,
        grace_chunks: GRACE,
    }
}

/// A message of four chunks of `d₀`, stamped `now`.
fn four_chunks(now: u128) -> Vec<Chunk> {
    let mut chunks = chunks_of(&Bytes::from(vec![7_u8; 4 * CHUNK]), CHUNK);
    for chunk in chunks.iter_mut() {
        chunk.meta.ts_ms = now;
    }
    assert_eq!(chunks.len(), 4);
    chunks
}

/// A credit reading in which the sender holds `credit` unused on every carrier.
fn holding(credit: u64) -> impl Fn(Option<ChannelLane>) -> u64 {
    move |_| credit
}

/// An honest sender the receiver throttles is never expired: while the receiver holds its
/// credit (the frames that took it, a release awaiting its batch, a deferred advertisement), the
/// sender holds none unused and the clock pauses, however long; once it holds credit, it sends
/// at once, and the clock's advance stays within what it sent (Prop. 4).
#[test]
fn test_an_honest_throttled_sender_is_never_expired() {
    let t0 = get_epoch_ms();
    let mut r = MessageReassembler::<()>::with_limits(floor_limits());
    let chunks = four_chunks(t0);
    let deliver = |r: &mut MessageReassembler, chunk: &Chunk, now: u128| {
        r.handle_charged_at(chunk.clone(), (), (now, holding(0)), true, None)
            .0
    };

    assert!(matches!(
        deliver(&mut r, &chunks[0], t0),
        ReassemblyOutcome::Incomplete
    ));
    // Throttled for a hundred grace windows: the receiver holds the sender's credit.
    let throttled = 100 * (GRACE + 1) as u128 * CHUNK as u128 / RATE as u128;
    for now in (t0..=t0 + throttled).step_by(SWEEP_MS as usize) {
        assert_eq!(r.sweep_at(now, holding(0)), 0);
    }
    assert_eq!(r.pending_count(), 1, "a throttled sender's clock pauses");

    // The receiver releases two credits; the sender spends them at once.
    let released = t0 + throttled + SWEEP_MS;
    assert_eq!(r.sweep_at(released, holding(2)), 0);
    for chunk in &chunks[1..3] {
        assert!(matches!(
            deliver(&mut r, chunk, released + 1),
            ReassemblyOutcome::Incomplete
        ));
    }
    // Throttled again, as long again.
    for now in (released + 1..=released + throttled).step_by(SWEEP_MS as usize) {
        assert_eq!(r.sweep_at(now, holding(0)), 0);
    }
    assert!(matches!(
        deliver(&mut r, &chunks[3], released + throttled + 1),
        ReassemblyOutcome::Complete(_)
    ));
}

/// A bare first chunk whose sender holds its credit and sends nothing more is expired within the
/// grace window `G = (W + 1)·d₀/ρ + Δ`, attributably, and not before `(W + 1)·d₀/ρ` (Prop. 5).
#[test]
fn test_a_bare_first_chunk_is_expired_within_the_grace_window() {
    let t0 = get_epoch_ms();
    let mut r = MessageReassembler::<()>::with_limits(floor_limits());
    let chunks = four_chunks(t0);
    let withholding = holding(GRACE);
    assert!(matches!(
        r.handle_charged_at(chunks[0].clone(), (), (t0, &withholding), true, None)
            .0,
        ReassemblyOutcome::Incomplete
    ));

    let floor_ms = u128::from((GRACE + 1) * CHUNK as u64 / RATE);
    let grace_ms = floor_ms + SWEEP_MS;
    let mut expired_at = None;
    for now in (t0..=t0 + 2 * grace_ms).step_by(SWEEP_MS as usize) {
        if r.sweep_at(now, &withholding) == 1 {
            expired_at = Some(now - t0);
            break;
        }
    }
    let expired_at = expired_at.expect("a withholding sender is expired");
    assert!(
        expired_at > floor_ms,
        "not before (W + 1)·d₀/ρ: {expired_at}"
    );
    assert!(expired_at <= grace_ms, "within G: {expired_at}");
    assert_eq!(r.pending_count(), 0);

    // The expired message is terminal: a late chunk is a replay, not a second failure.
    assert!(matches!(
        r.handle_charged_at(
            chunks[1].clone(),
            (),
            (t0 + grace_ms, holding(0)),
            true,
            None
        )
        .0,
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
}

/// A caller that reads no credit never advances the clock: the plain API keeps TTL expiry
/// alone.
#[test]
fn test_without_a_credit_reading_a_message_expires_at_its_ttl_alone() {
    let t0 = get_epoch_ms();
    let mut r = MessageReassembler::<()>::with_limits(floor_limits());
    let chunks = four_chunks(t0);
    assert!(r.handle_at(chunks[0].clone(), t0).is_none());
    let ttl = u128::from(chunks[0].meta.ttl_ms);
    assert_eq!(r.remove_expired_at(t0 + ttl - 1), 0);
    assert_eq!(r.remove_expired_at(t0 + ttl), 1);
}
