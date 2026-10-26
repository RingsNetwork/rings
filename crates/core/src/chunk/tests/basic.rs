use super::*;

#[test]
fn test_constrained_reassembly_limits_are_smaller_than_production() {
    let production = ReassemblyLimits::production();
    let constrained = ReassemblyLimits::constrained();

    assert!(constrained.max_pending_messages < production.max_pending_messages);
    assert_eq!(constrained.max_message_bytes, production.max_message_bytes);
    assert_eq!(
        constrained.max_chunks_per_message,
        production.max_chunks_per_message
    );
    assert!(constrained.max_completed_ids < production.max_completed_ids);
    assert_eq!(
        constrained.max_chunk_data_len,
        production.max_chunk_data_len
    );
}

#[test]
fn test_data_chunks() {
    let data = "helloworld".repeat(2).into();
    let ret: Vec<Chunk> = chunks_of(&data, 32);
    assert_eq!(ret.len(), 1);
    assert_eq!(ret[ret.len() - 1].chunk, [0, 1]);

    let data = "helloworld".repeat(1024).into();
    let ret: Vec<Chunk> = chunks_of(&data, 32);
    assert_eq!(ret.len(), 10 * 1024 / 32);
    assert_eq!(ret[ret.len() - 1].chunk, [319, 320]);
}

#[test]
fn test_stream_empty_yields_no_chunks() {
    assert!(chunks_of(&Bytes::new(), 32).is_empty());
}

#[test]
fn test_stream_exact_multiple_all_full() {
    let data: Bytes = vec![0u8; 64].into();
    let chunks = chunks_of(&data, 32);
    assert_eq!(chunks.len(), 2);
    assert!(chunks.iter().all(|c| c.data.len() == 32));
    assert_eq!(chunks[0].chunk, [0, 2]);
    assert_eq!(chunks[1].chunk, [1, 2]);
}

#[test]
fn test_stream_non_multiple_last_is_remainder() {
    let data: Bytes = vec![0u8; 70].into();
    let chunks = chunks_of(&data, 32);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].data.len(), 32);
    assert_eq!(chunks[1].data.len(), 32);
    assert_eq!(chunks[2].data.len(), 6);
}

#[test]
fn test_stream_larger_than_data_is_single_chunk() {
    let data: Bytes = vec![0u8; 10].into();
    let chunks = chunks_of(&data, 1024);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].chunk, [0, 1]);
}

#[test]
fn test_stream_zero_size_is_clamped_to_one() {
    let data: Bytes = vec![0u8; 4].into();
    let chunks = chunks_of(&data, 0);
    assert_eq!(chunks.len(), 4);
    assert!(chunks.iter().all(|c| c.data.len() == 1));
}

#[test]
fn test_stream_chunks_share_one_message_id() {
    let data: Bytes = vec![0u8; 100].into();
    let chunks = chunks_of(&data, 32);
    let id = chunks[0].meta.id;
    assert!(chunks.iter().all(|c| c.meta.id == id));
}

/// Cutting at any size and feeding the pieces back through the reassembler (in order) yields the
/// original bytes — across exact multiples, remainders, single-chunk, and one-byte cuts.
#[test]
fn test_stream_then_reassemble_round_trips() {
    for (len, size) in [
        (1usize, 7usize),
        (7, 7),
        (8, 7),
        (100, 7),
        (1000, 64),
        (5, 1),
    ] {
        let data: Bytes = (0..len).map(|i| i as u8).collect::<Vec<u8>>().into();
        let mut r = MessageReassembler::new();
        let mut out = None;
        for c in Chunk::stream(data.clone(), size) {
            out = r.handle(c).or(out);
        }
        assert_eq!(out.unwrap(), data, "len={len} size={size}");
    }
}

/// Test reserves with readable, distinct values (`whole < chunk`) so the two paths are easy to
/// tell apart in the assertions below.
fn reserves(whole: usize, chunk: usize, min_chunk_data: usize) -> WireReserves {
    WireReserves {
        whole,
        chunk,
        min_chunk_data,
    }
}

#[test]
fn test_plan_whole_includes_whole_overhead() {
    let r = reserves(10, 20, 1);
    // Whole fits while payload + whole ≤ limit, up to and including the boundary.
    assert_eq!(r.plan(0, 100), Some(Framing::Whole));
    assert_eq!(r.plan(90, 100), Some(Framing::Whole));
    // One past the boundary must chunk.
    assert_eq!(r.plan(91, 100), Some(Framing::Chunked { chunk_size: 80 }));
}

/// The chunk size reserves the chunk overhead, so `chunk_size + chunk ≤ limit`: a wrapped chunk
/// can never exceed the negotiated limit.
#[test]
fn test_plan_chunk_size_reserves_overhead() {
    let (limit, chunk_overhead) = (65536usize, 4096usize);
    let Some(Framing::Chunked { chunk_size }) =
        reserves(16, chunk_overhead, 16).plan(limit * 2, limit)
    else {
        panic!("expected chunked");
    };
    assert_eq!(chunk_size, limit - chunk_overhead);
    assert!(chunk_size + chunk_overhead <= limit);
}

#[test]
fn test_plan_none_when_chunk_too_small() {
    // A limit that cannot fit `chunk + min_chunk_data` is rejected outright, not split tiny.
    assert_eq!(reserves(4, 10, 1).plan(100, 5), None); // below the overhead
    assert_eq!(reserves(4, 10, 1).plan(100, 10), None); // == overhead, 0 data bytes
                                                        // limit just clears chunk + min: the smallest *allowed* cut.
    assert_eq!(
        reserves(4, 10, 1).plan(100, 11),
        Some(Framing::Chunked { chunk_size: 1 })
    );
    // a realistic floor: min_chunk_data = 8 needs limit ≥ chunk + 8.
    assert_eq!(reserves(4, 10, 8).plan(100, 17), None); // 17 < 10 + 8
    assert_eq!(
        reserves(4, 10, 8).plan(100, 18),
        Some(Framing::Chunked { chunk_size: 8 })
    );
}

#[test]
fn test_plan_is_total_on_overflow() {
    // `payload_len + whole` overflows usize; must not panic, and (not a whole fit) falls through
    // to the chunked decision rather than wrapping around.
    assert_eq!(
        reserves(10, 20, 1).plan(usize::MAX, 100),
        Some(Framing::Chunked { chunk_size: 80 })
    );
    // overflow with a too-small limit still yields None, not a panic.
    assert_eq!(reserves(10, 20, 1).plan(usize::MAX, 10), None);
}

#[test]
fn test_reassembles_in_order() {
    let data: Bytes = "helloworld".repeat(1024).into();
    let mut r = MessageReassembler::new();
    let chunks = chunks_of(&data, 32);
    let mut out = None;
    for c in chunks {
        out = r.handle(c).or(out);
    }
    assert_eq!(out.unwrap(), data);
    assert_eq!(r.pending_count(), 0, "completed message is forgotten");
}

/// Once its first chunk opened a message, the later chunks may arrive in any order.
#[test]
fn test_reassembles_out_of_order_after_the_first_chunk() {
    let data: Bytes = "helloworld".repeat(64).into();
    let mut chunks = chunks_of(&data, 32);
    chunks[1..].reverse();
    let mut r = MessageReassembler::new();
    let mut out = None;
    for c in chunks {
        out = r.handle(c).or(out);
    }
    assert_eq!(out.unwrap(), data);
}

/// Only the first chunk, charged for the whole message, opens it: a later chunk of a message no
/// first chunk opened is invalid, and so is a chunk longer than its message's first.
#[test]
fn test_a_message_opens_only_at_its_first_chunk_and_never_outgrows_it() {
    let data: Bytes = "helloworld".repeat(8).into();
    let chunks = chunks_of(&data, 32);
    let now = get_epoch_ms();
    let mut r = MessageReassembler::new();
    assert!(matches!(
        r.handle_retained_outcome_at(chunks[1].clone(), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
    assert_eq!(r.pending_count(), 0);

    let mut r = MessageReassembler::new();
    let meta = ChunkMeta::default();
    let chunk = |position, length| Chunk {
        chunk: [position, 3],
        data: vec![0u8; length].into(),
        meta,
    };
    assert!(matches!(
        r.handle_retained_outcome_at(chunk(0, 8), now),
        ReassemblyOutcome::Incomplete
    ));
    assert!(matches!(
        r.handle_retained_outcome_at(chunk(1, 8), now),
        ReassemblyOutcome::Incomplete
    ));
    assert!(matches!(
        r.handle_retained_outcome_at(chunk(2, 9), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
    // The invalid chunk failed its message: what follows of it is a replay.
    assert!(matches!(
        r.handle_retained_outcome_at(chunk(2, 8), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
}

#[test]
fn test_full_retransmit_after_completion_is_not_redelivered() {
    // A message that completes, then is *fully* retransmitted within its TTL window, must not be
    // delivered a second time — the completed id is tombstoned.
    let data: Bytes = "helloworld".repeat(64).into();
    let chunks = chunks_of(&data, 32);
    assert!(chunks.len() > 1, "need a multi-chunk message for this test");

    let mut r = MessageReassembler::new();
    let mut first = None;
    for c in chunks.clone() {
        first = r.handle(c).or(first);
    }
    assert_eq!(first.unwrap(), data, "first assembly delivers once");
    assert_eq!(r.pending_count(), 0);

    // Replay every chunk of the same message; none should re-open a pending entry or re-deliver.
    for c in chunks {
        assert!(
            r.handle(c).is_none(),
            "a retransmit of an already-completed message must be dropped"
        );
    }
    assert_eq!(
        r.pending_count(),
        0,
        "no pending re-opened by the retransmit"
    );
}

#[test]
fn test_duplicate_chunk_does_not_break_reassembly() {
    // Regression: arrival order [0, 1, 0] used to dedup-before-sort and never complete.
    let data: Bytes = "helloworld".repeat(8).into(); // > 32 bytes => 3 chunks
    let chunks = chunks_of(&data, 32);
    assert!(chunks.len() >= 2);
    let mut r = MessageReassembler::new();

    // Feed every chunk, re-feeding chunk 0 in the middle as a duplicate.
    assert!(r.handle(chunks[0].clone()).is_none());
    for c in &chunks[1..] {
        let _ = r.handle(chunks[0].clone()); // duplicate of position 0, repeatedly
        if let Some(out) = r.handle(c.clone()) {
            assert_eq!(out, data);
            assert_eq!(r.pending_count(), 0);
            return;
        }
    }
    panic!("message never completed despite all chunks arriving");
}

#[test]
fn test_interleaved_messages_are_isolated() {
    let d1: Bytes = "hello".repeat(64).into();
    let d2: Bytes = "world".repeat(64).into();
    let c1 = chunks_of(&d1, 32);
    let c2 = chunks_of(&d2, 32);
    let mut r = MessageReassembler::new();

    // interleave the two messages
    let (mut o1, mut o2) = (None, None);
    for pair in c1.iter().zip(c2.iter()) {
        o1 = r.handle(pair.0.clone()).or(o1);
        o2 = r.handle(pair.1.clone()).or(o2);
    }
    // drain any tail (lengths may differ)
    for c in c1.iter().chain(c2.iter()) {
        let out = r.handle(c.clone());
        o1 = out.clone().filter(|b| *b == d1).or(o1);
        o2 = out.filter(|b| *b == d2).or(o2);
    }
    assert_eq!(o1.unwrap(), d1);
    assert_eq!(o2.unwrap(), d2);
}

#[test]
fn test_incomplete_message_stays_pending() {
    let data: Bytes = "helloworld".repeat(64).into();
    let chunks = chunks_of(&data, 32);
    let mut r = MessageReassembler::new();
    for c in &chunks[..chunks.len() - 1] {
        assert!(r.handle(c.clone()).is_none());
    }
    assert_eq!(r.pending_count(), 1);
    let out = r.handle(chunks.last().unwrap().clone());
    assert_eq!(out.unwrap(), data);
}

#[test]
fn test_malformed_chunks_are_dropped() {
    let mut r = MessageReassembler::new();
    // total == 0
    assert!(r
        .handle(Chunk {
            chunk: [0, 0],
            data: Bytes::from_static(b"x"),
            meta: ChunkMeta::default(),
        })
        .is_none());
    // position >= total
    assert!(r
        .handle(Chunk {
            chunk: [5, 3],
            data: Bytes::from_static(b"x"),
            meta: ChunkMeta::default(),
        })
        .is_none());
    assert_eq!(r.pending_count(), 0);
}

#[test]
fn test_reassembly_outcome_distinguishes_invalid_and_replayed_chunks() {
    let mut reassembler = MessageReassembler::new();
    let now = get_epoch_ms();
    let invalid = reassembler.handle_retained_outcome_at(
        Chunk {
            chunk: [0, 0],
            data: Bytes::from_static(b"invalid"),
            meta: ChunkMeta::default(),
        },
        now,
    );
    assert!(matches!(
        invalid,
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));

    let first = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"first"),
        meta: ChunkMeta::default(),
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(first.clone(), now),
        ReassemblyOutcome::Incomplete
    ));
    assert!(matches!(
        reassembler.handle_retained_outcome_at(first.clone(), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));

    let conflicting = Chunk {
        data: Bytes::from_static(b"conflict"),
        ..first
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(conflicting, now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
}

#[test]
fn test_old_timestamp_is_dropped_without_panic() {
    // ts_ms < TS_OFFSET_TOLERANCE_MS would underflow a plain `u128` subtraction (no panic with
    // saturating arithmetic), and a chunk stamped at the epoch is already long expired — it must
    // be dropped, not delivered, even though it is a complete `total == 1` message.
    let mut r = MessageReassembler::new();
    let out = r.handle(Chunk {
        chunk: [0, 1],
        data: Bytes::from_static(b"ok"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: 0,
            ttl_ms: DEFAULT_TTL_MS,
        },
    });
    assert!(out.is_none());
    assert_eq!(r.pending_count(), 0);
}

#[test]
fn test_expired_single_chunk_is_not_delivered() {
    // Regression: sweeping *other* pending entries before insertion let an already-expired
    // `total == 1` chunk be delivered immediately. It must be rejected up front.
    let mut r = MessageReassembler::new();
    let now = get_epoch_ms();
    let expired = Chunk {
        chunk: [0, 1],
        data: Bytes::from_static(b"x"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: now.saturating_sub(1000),
            ttl_ms: 100, // expired 900ms ago
        },
    };
    assert!(matches!(
        r.handle_retained_outcome_at(expired.clone(), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
    assert!(matches!(
        r.handle_retained_outcome_at(expired, now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
    assert_eq!(r.pending_count(), 0);
}

#[test]
fn test_oversize_chunk_data_is_rejected() {
    let limits = small_limits();
    let mut r = MessageReassembler::with_limits(limits);
    let data: Bytes = vec![0u8; limits.max_chunk_data_len + 1].into();
    let out = r.handle(Chunk {
        chunk: [0, 1],
        data,
        meta: ChunkMeta::default(),
    });
    assert!(out.is_none());
    assert_eq!(r.pending_count(), 0);
}

/// A charge that counts its own drops, standing for the mailbox reservation a first chunk brings.
struct CountedCharge(Arc<AtomicUsize>);

impl Drop for CountedCharge {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

/// One charge per message (load-bounds paper, Law 4): the first chunk's charge is held while its
/// message is open and handed on with the completed message; a later chunk's charge is dropped at
/// once; an expired message drops its charge.
#[test]
fn test_the_first_chunk_charge_is_held_and_handed_on_with_its_message() {
    let now = get_epoch_ms();
    let drops = Arc::new(AtomicUsize::new(0));
    let charge = || CountedCharge(Arc::clone(&drops));
    let dropped = || drops.load(Ordering::Acquire);
    let mut r = MessageReassembler::<CountedCharge>::with_limits(ReassemblyLimits::production());
    let chunks = chunks_of(&"helloworld".repeat(8).into(), 32);
    assert_eq!(chunks.len(), 3);

    assert!(matches!(
        r.handle_charged_at(chunks[0].clone(), charge(), (now, |_| 0), true, None)
            .0,
        ReassemblyOutcome::Incomplete
    ));
    assert_eq!(dropped(), 0, "the first chunk's charge is held");
    assert!(matches!(
        r.handle_charged_at(chunks[1].clone(), charge(), (now, |_| 0), true, None)
            .0,
        ReassemblyOutcome::Incomplete
    ));
    assert_eq!(dropped(), 1, "a later chunk's charge is dropped at once");
    let ReassemblyOutcome::Complete(message) = r
        .handle_charged_at(chunks[2].clone(), charge(), (now, |_| 0), true, None)
        .0
    else {
        panic!("the third chunk completes the message");
    };
    assert_eq!(dropped(), 2, "the message's charge leaves with it");
    let (_, message_charge) = message.into_parts();
    drop(message_charge);
    assert_eq!(dropped(), 3);

    let ttl_ms = u128::from(chunks[0].meta.ttl_ms);
    let expiring = chunks_of(&"helloworld".repeat(8).into(), 32);
    r.handle_charged_at(expiring[0].clone(), charge(), (now, |_| 0), true, None);
    assert_eq!(dropped(), 3);
    assert_eq!(r.remove_expired_at(expiring[0].meta.ts_ms + ttl_ms), 1);
    assert_eq!(dropped(), 4, "an expired message drops its charge");
}

/// The per-message bound: a first chunk `[0, n]` of `d₀` bytes opens a message only while
/// `(n − 1)·d₀ < max_message_bytes`, so an honest message of at most that many bytes always
/// opens, while a forged `total` that would pass it is invalid.
#[test]
fn test_the_message_bound_is_judged_at_the_first_chunk() {
    let limits = small_limits();
    let now = get_epoch_ms();
    let first = |total| Chunk {
        chunk: [0, total],
        data: vec![0u8; limits.max_chunk_data_len].into(),
        meta: ChunkMeta::default(),
    };
    let d0 = limits.max_chunk_data_len;
    // The largest honest message, `max_message_bytes`, needs `⌈max / d₀⌉` chunks.
    let honest = limits.max_message_bytes.div_ceil(d0);
    assert!((honest - 1) * d0 < limits.max_message_bytes);
    let mut r = MessageReassembler::with_limits(limits);
    assert!(matches!(
        r.handle_retained_outcome_at(first(honest), now),
        ReassemblyOutcome::Incomplete
    ));
    assert!(matches!(
        r.handle_retained_outcome_at(first(honest + 1), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
    assert_eq!(r.pending_count(), 1);
}

#[test]
fn test_future_timestamp_is_dropped() {
    let mut r = MessageReassembler::new();
    let out = r.handle(Chunk {
        chunk: [0, 1],
        data: Bytes::from_static(b"x"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: get_epoch_ms() + 10 * TS_OFFSET_TOLERANCE_MS,
            ttl_ms: DEFAULT_TTL_MS,
        },
    });
    assert!(out.is_none());
}

/// A message's chunks travel one lane (one lane per class): a chunk that arrives on another lane than
/// the message's first chunk is refused as invalid, and the message completes on its own lane,
/// reporting it as its carrier.
#[test]
fn test_a_chunk_that_changes_lanes_is_refused_and_the_message_keeps_its_carrier() {
    use rings_transport::core::pool::ChannelLane;

    let mut reassembler = MessageReassembler::new();
    let now = get_epoch_ms();
    let (data, other) = (ChannelLane::new(3), ChannelLane::PRIORITY);
    let meta = ChunkMeta::default();
    let chunk = |position, bytes: &'static [u8]| Chunk {
        chunk: [position, 2],
        data: Bytes::from_static(bytes),
        meta,
    };
    let handle = |reassembler: &mut MessageReassembler, chunk, carrier| {
        reassembler
            .handle_charged_at(chunk, (), (now, |_| 0), true, Some(carrier))
            .0
    };
    assert!(matches!(
        handle(&mut reassembler, chunk(0, b"first"), data),
        ReassemblyOutcome::Incomplete
    ));
    assert!(matches!(
        handle(&mut reassembler, chunk(1, b"other"), other),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));

    let mut reassembler = MessageReassembler::new();
    assert!(matches!(
        handle(&mut reassembler, chunk(0, b"first"), data),
        ReassemblyOutcome::Incomplete
    ));
    match handle(&mut reassembler, chunk(1, b"last"), data) {
        ReassemblyOutcome::Complete(message) => assert_eq!(message.carrier(), Some(data)),
        _ => panic!("the message completes on its own lane"),
    }
}
