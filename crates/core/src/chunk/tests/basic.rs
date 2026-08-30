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
    assert!(constrained.max_total_buffered_cost < production.max_total_buffered_cost);
    assert!(constrained.max_completed_ids < production.max_completed_ids);
    assert_eq!(
        constrained.max_chunk_data_len,
        production.max_chunk_data_len
    );
}

#[test]
fn test_data_chunks() {
    let data = "helloworld".repeat(2).into();
    let ret: Vec<Chunk> = ChunkList::split(&data, 32).into();
    assert_eq!(ret.len(), 1);
    assert_eq!(ret[ret.len() - 1].chunk, [0, 1]);

    let data = "helloworld".repeat(1024).into();
    let ret: Vec<Chunk> = ChunkList::split(&data, 32).into();
    assert_eq!(ret.len(), 10 * 1024 / 32);
    assert_eq!(ret[ret.len() - 1].chunk, [319, 320]);
}

#[test]
fn test_split_empty_yields_no_chunks() {
    assert!(ChunkList::split(&Bytes::new(), 32).to_vec().is_empty());
}

#[test]
fn test_split_exact_multiple_all_full() {
    let data: Bytes = vec![0u8; 64].into();
    let chunks = ChunkList::split(&data, 32).to_vec();
    assert_eq!(chunks.len(), 2);
    assert!(chunks.iter().all(|c| c.data.len() == 32));
    assert_eq!(chunks[0].chunk, [0, 2]);
    assert_eq!(chunks[1].chunk, [1, 2]);
}

#[test]
fn test_split_non_multiple_last_is_remainder() {
    let data: Bytes = vec![0u8; 70].into();
    let chunks = ChunkList::split(&data, 32).to_vec();
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].data.len(), 32);
    assert_eq!(chunks[1].data.len(), 32);
    assert_eq!(chunks[2].data.len(), 6);
}

#[test]
fn test_split_larger_than_data_is_single_chunk() {
    let data: Bytes = vec![0u8; 10].into();
    let chunks = ChunkList::split(&data, 1024).to_vec();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].chunk, [0, 1]);
}

#[test]
fn test_split_zero_size_is_clamped_to_one() {
    let data: Bytes = vec![0u8; 4].into();
    let chunks = ChunkList::split(&data, 0).to_vec();
    assert_eq!(chunks.len(), 4);
    assert!(chunks.iter().all(|c| c.data.len() == 1));
}

#[test]
fn test_split_chunks_share_one_message_id() {
    let data: Bytes = vec![0u8; 100].into();
    let chunks = ChunkList::split(&data, 32).to_vec();
    let id = chunks[0].meta.id;
    assert!(chunks.iter().all(|c| c.meta.id == id));
}

/// Cutting at any size and feeding the pieces back through the reassembler (in order) yields the
/// original bytes — across exact multiples, remainders, single-chunk, and one-byte cuts.
#[test]
fn test_split_then_reassemble_round_trips() {
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
        for c in ChunkList::split(&data, size) {
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

#[test]
fn test_reassembles_out_of_order() {
    let data: Bytes = "helloworld".repeat(64).into();
    let mut chunks = chunks_of(&data, 32);
    chunks.reverse();
    let mut r = MessageReassembler::new();
    let mut out = None;
    for c in chunks {
        out = r.handle(c).or(out);
    }
    assert_eq!(out.unwrap(), data);
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
    let invalid = reassembler.handle_retained_outcome(Chunk {
        chunk: [0, 0],
        data: Bytes::from_static(b"invalid"),
        meta: ChunkMeta::default(),
    });
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
        reassembler.handle_retained_outcome(first.clone()),
        ReassemblyOutcome::Incomplete
    ));
    assert!(matches!(
        reassembler.handle_retained_outcome(first.clone()),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));

    let conflicting = Chunk {
        data: Bytes::from_static(b"conflict"),
        ..first
    };
    assert!(matches!(
        reassembler.handle_retained_outcome(conflicting),
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
    assert_eq!(r.buffered_cost, 0);
}

#[test]
fn test_buffered_cost_returns_to_zero_after_completion() {
    let data: Bytes = "helloworld".repeat(100).into();
    let mut r = MessageReassembler::new();
    for c in ChunkList::split(&data, 32) {
        r.handle(c);
    }
    assert_eq!(r.pending_count(), 0);
    assert_eq!(r.buffered_cost, 0, "completing a message frees its budget");
}

/// A single id advertising a huge `total` and streaming distinct positions cannot grow without
/// bound: the per-message byte cap stops it.
#[test]
fn test_per_message_byte_cap_bounds_one_id() {
    let limits = small_limits();
    let mut r = MessageReassembler::with_limits(limits);
    let meta = ChunkMeta::default();
    let data: Bytes = vec![0u8; limits.max_chunk_data_len].into();
    // Within the slot cap, but its data far exceeds the per-message byte cap so it never fills.
    let total = limits.max_chunks_per_message;

    let mut accepted = 0usize;
    for position in 0..50 {
        let before = r.pending.get(&meta.id).map(|p| p.slots.len()).unwrap_or(0);
        r.handle(Chunk {
            meta,
            chunk: [position, total],
            data: data.clone(),
        });
        let after = r.pending.get(&meta.id).map(|p| p.slots.len()).unwrap_or(0);
        if after > before {
            accepted += 1;
        }
    }

    let pending = r.pending.get(&meta.id).expect("still pending");
    assert!(
        pending.data_bytes <= limits.max_message_bytes,
        "per-message buffered data must stay within the cap"
    );
    assert!(
        accepted < 50,
        "the cap must reject some chunks, got {accepted}"
    );
    assert_eq!(
        r.buffered_cost,
        pending.cost(limits.slot_overhead),
        "accounting stays exact"
    );
}

/// Spreading the flood across many ids is bounded too: the global buffered-cost ceiling caps
/// total memory regardless of how many ids are used.
#[test]
fn test_global_cost_cap_bounds_total() {
    let limits = small_limits();
    let mut r = MessageReassembler::with_limits(limits);
    // Each id contributes one slot of `max_chunk_data_len` data; keep them all pending.
    for _ in 0..(limits.max_pending_messages * 4) {
        r.handle(Chunk {
            chunk: [0, 2],
            data: vec![0u8; limits.max_chunk_data_len].into(),
            meta: ChunkMeta::default(),
        });
    }
    assert!(
        r.buffered_cost <= limits.max_total_buffered_cost,
        "global buffered cost {} exceeded cap {}",
        r.buffered_cost,
        limits.max_total_buffered_cost
    );
}

#[test]
fn test_shared_budget_bounds_multiple_peer_reassemblers() {
    let mut limits = small_limits();
    limits.max_total_buffered_cost = 96;
    let budget = Arc::new(ReassemblyBudget::new(limits));
    let mut first = MessageReassembler::with_limits_and_budget(limits, budget.clone());
    let mut second = MessageReassembler::with_limits_and_budget(limits, budget);

    for _ in 0..4 {
        first.handle(Chunk {
            chunk: [0, 2],
            data: vec![0_u8; limits.max_chunk_data_len].into(),
            meta: ChunkMeta::default(),
        });
    }
    second.handle(Chunk {
        chunk: [0, 2],
        data: vec![0_u8; limits.max_chunk_data_len].into(),
        meta: ChunkMeta::default(),
    });
    assert_eq!(first.pending_count(), 4);
    assert_eq!(second.pending_count(), 0);

    drop(first);
    second.handle(Chunk {
        chunk: [0, 2],
        data: vec![0_u8; limits.max_chunk_data_len].into(),
        meta: ChunkMeta::default(),
    });
    assert_eq!(second.pending_count(), 1);
}

#[test]
fn test_peer_budget_preserves_shared_capacity_for_another_peer() {
    let mut limits = small_limits();
    limits.max_message_bytes = 32;
    limits.max_chunks_per_message = 2;
    limits.max_total_buffered_cost = 96;
    let budget = Arc::new(ReassemblyBudget::new(limits));
    let mut first = MessageReassembler::with_limits_and_budget(limits, budget.clone());
    let mut second = MessageReassembler::with_limits_and_budget(limits, budget);

    for _ in 0..4 {
        first.handle(Chunk {
            chunk: [0, 2],
            data: vec![0_u8; limits.max_chunk_data_len].into(),
            meta: ChunkMeta::default(),
        });
    }
    assert_eq!(first.pending_count(), 2);
    assert_eq!(
        first.buffered_cost,
        limits.max_peer_buffered_cost(),
        "one peer must stop at its pending-cost allowance"
    );

    assert_eq!(
        second
            .handle(Chunk {
                chunk: [0, 1],
                data: Bytes::from_static(b"ok"),
                meta: ChunkMeta::default(),
            })
            .as_deref(),
        Some(b"ok".as_slice()),
        "another peer must still be able to complete a small reassembly"
    );
}

#[test]
fn test_completed_output_keeps_shared_budget_until_released() {
    let mut limits = small_limits();
    limits.slot_overhead = 0;
    limits.max_total_buffered_cost = 6;
    let budget = Arc::new(ReassemblyBudget::new(limits));
    let mut first = MessageReassembler::with_limits_and_budget(limits, budget.clone());
    let mut second = MessageReassembler::with_limits_and_budget(limits, budget.clone());

    let retained = first
        .handle_retained(Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(b"one"),
            meta: ChunkMeta::default(),
        })
        .expect("first output must fit its buffered and contiguous copies");
    assert_eq!(budget.buffered_cost.load(Ordering::Acquire), 3);
    assert!(
        second
            .handle_retained(Chunk {
                chunk: [0, 1],
                data: Bytes::from_static(b"two"),
                meta: ChunkMeta::default(),
            })
            .is_none(),
        "second output copy must not reuse retained output capacity"
    );

    drop(retained);
    assert_eq!(budget.buffered_cost.load(Ordering::Acquire), 0);
    assert!(second
        .handle_retained(Chunk {
            chunk: [0, 1],
            data: Bytes::from_static(b"two"),
            meta: ChunkMeta::default(),
        })
        .is_some());
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
