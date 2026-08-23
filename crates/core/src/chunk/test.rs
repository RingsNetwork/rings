use super::*;

fn chunks_of(data: &Bytes, mtu: usize) -> Vec<Chunk> {
    ChunkList::split(data, mtu).into()
}

/// Tiny limits so the admission rule can be exercised without giant synthetic payloads.
fn small_limits() -> ReassemblyLimits {
    ReassemblyLimits {
        max_pending_messages: 4,
        max_chunk_data_len: 16,
        max_message_bytes: 100,
        max_chunks_per_message: 64,
        max_total_buffered_cost: 256,
        slot_overhead: 8,
        max_completed_ids: 8,
    }
}

#[test]
fn constrained_reassembly_limits_are_smaller_than_production() {
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
fn split_empty_yields_no_chunks() {
    assert!(ChunkList::split(&Bytes::new(), 32).to_vec().is_empty());
}

#[test]
fn split_exact_multiple_all_full() {
    let data: Bytes = vec![0u8; 64].into();
    let chunks = ChunkList::split(&data, 32).to_vec();
    assert_eq!(chunks.len(), 2);
    assert!(chunks.iter().all(|c| c.data.len() == 32));
    assert_eq!(chunks[0].chunk, [0, 2]);
    assert_eq!(chunks[1].chunk, [1, 2]);
}

#[test]
fn split_non_multiple_last_is_remainder() {
    let data: Bytes = vec![0u8; 70].into();
    let chunks = ChunkList::split(&data, 32).to_vec();
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].data.len(), 32);
    assert_eq!(chunks[1].data.len(), 32);
    assert_eq!(chunks[2].data.len(), 6);
}

#[test]
fn split_larger_than_data_is_single_chunk() {
    let data: Bytes = vec![0u8; 10].into();
    let chunks = ChunkList::split(&data, 1024).to_vec();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].chunk, [0, 1]);
}

#[test]
fn split_zero_size_is_clamped_to_one() {
    let data: Bytes = vec![0u8; 4].into();
    let chunks = ChunkList::split(&data, 0).to_vec();
    assert_eq!(chunks.len(), 4);
    assert!(chunks.iter().all(|c| c.data.len() == 1));
}

#[test]
fn split_chunks_share_one_message_id() {
    let data: Bytes = vec![0u8; 100].into();
    let chunks = ChunkList::split(&data, 32).to_vec();
    let id = chunks[0].meta.id;
    assert!(chunks.iter().all(|c| c.meta.id == id));
}

/// Cutting at any size and feeding the pieces back through the reassembler (in order) yields the
/// original bytes — across exact multiples, remainders, single-chunk, and one-byte cuts.
#[test]
fn split_then_reassemble_round_trips() {
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
fn plan_whole_includes_whole_overhead() {
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
fn plan_chunk_size_reserves_overhead() {
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
fn plan_none_when_chunk_too_small() {
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
fn plan_is_total_on_overflow() {
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
fn reassembles_in_order() {
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
fn reassembles_out_of_order() {
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
fn full_retransmit_after_completion_is_not_redelivered() {
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
fn duplicate_chunk_does_not_break_reassembly() {
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
fn interleaved_messages_are_isolated() {
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
fn incomplete_message_stays_pending() {
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
fn malformed_chunks_are_dropped() {
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
fn old_timestamp_is_dropped_without_panic() {
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
fn expired_single_chunk_is_not_delivered() {
    // Regression: sweeping *other* pending entries before insertion let an already-expired
    // `total == 1` chunk be delivered immediately. It must be rejected up front.
    let mut r = MessageReassembler::new();
    let now = get_epoch_ms();
    let out = r.handle(Chunk {
        chunk: [0, 1],
        data: Bytes::from_static(b"x"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: now.saturating_sub(1000),
            ttl_ms: 100, // expired 900ms ago
        },
    });
    assert!(out.is_none());
    assert_eq!(r.pending_count(), 0);
}

#[test]
fn oversize_chunk_data_is_rejected() {
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
fn buffered_cost_returns_to_zero_after_completion() {
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
fn per_message_byte_cap_bounds_one_id() {
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
fn global_cost_cap_bounds_total() {
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
fn shared_budget_bounds_multiple_peer_reassemblers() {
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
fn peer_budget_preserves_shared_capacity_for_another_peer() {
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
fn completed_output_keeps_shared_budget_until_released() {
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
fn future_timestamp_is_dropped() {
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

#[test]
fn expired_partial_messages_are_evicted() {
    let mut r = MessageReassembler::new();
    let now = get_epoch_ms();
    // a partial (1 of 2) message that is already expired
    r.handle(Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"x"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: now.saturating_sub(1000),
            ttl_ms: 100,
        },
    });
    // a fresh partial message triggers remove_expired, dropping the stale one
    r.handle(Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"y"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: now,
            ttl_ms: DEFAULT_TTL_MS,
        },
    });
    assert_eq!(r.pending_count(), 1, "only the fresh partial remains");
}

#[test]
fn pending_messages_are_capped() {
    let limits = small_limits();
    let mut r = MessageReassembler::with_limits(limits);
    // each is the first of two chunks => stays pending
    for _ in 0..(limits.max_pending_messages + 10) {
        r.handle(Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"x"),
            meta: ChunkMeta::default(), // fresh id, fresh ts each time
        });
    }
    assert_eq!(r.pending_count(), limits.max_pending_messages);
}

#[test]
fn round_trip_reordered_with_duplicates() {
    let data: Bytes = "abcdefghij".repeat(500).into();
    let mut chunks = chunks_of(&data, 64);
    // reorder + inject duplicates mid-stream (not after the final chunk, which would just
    // start a fresh, TTL-evicted pending entry — a late retransmit, not a reassembly bug).
    chunks.reverse();
    let dup = chunks[chunks.len() / 2].clone();
    chunks.insert(1, dup.clone());
    chunks.insert(chunks.len() / 3, dup);

    let mut r = MessageReassembler::new();
    let mut out = None;
    for c in chunks {
        out = r.handle(c).or(out);
    }
    assert_eq!(out.unwrap(), data);
    assert_eq!(r.pending_count(), 0);
}

/// A forged `total` larger than the per-message slot cap is rejected before it can allocate a
/// huge slot map, even though each individual chunk's data is tiny.
#[test]
fn total_over_slot_cap_is_rejected() {
    let limits = small_limits();
    let mut r = MessageReassembler::with_limits(limits);
    let out = r.handle(Chunk {
        chunk: [0, limits.max_chunks_per_message + 1],
        data: Bytes::from_static(b"x"),
        meta: ChunkMeta::default(),
    });
    assert!(out.is_none());
    assert_eq!(r.pending_count(), 0);
    assert_eq!(r.buffered_cost, 0);
}

/// Two chunks sharing an id/total but from different transmissions (different `ts_ms`/`ttl_ms`)
/// must not be merged into one pending entry.
#[test]
fn mismatched_ts_or_ttl_for_same_id_is_rejected() {
    let mut r = MessageReassembler::new();
    let id = Uuid::new_v4();
    let now = get_epoch_ms();
    assert!(r
        .handle(Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"a"),
            meta: ChunkMeta {
                id,
                ts_ms: now,
                ttl_ms: DEFAULT_TTL_MS
            },
        })
        .is_none());
    // Same id/total, different ts_ms → rejected (a chunk from another transmission).
    let out = r.handle(Chunk {
        chunk: [1, 2],
        data: Bytes::from_static(b"b"),
        meta: ChunkMeta {
            id,
            ts_ms: now + 1,
            ttl_ms: DEFAULT_TTL_MS,
        },
    });
    assert!(out.is_none(), "must not complete by mixing transmissions");
    let p = r.pending.get(&id).expect("first chunk still pending");
    assert_eq!(p.slots.len(), 1, "the mismatched chunk left no trace");
}

/// Once a completed message's TTL elapses, its tombstone is evicted by the real
/// `remove_expired_at` path (driven here by an injected clock, not by poking internal state),
/// and a fresh message reusing the same id is then accepted rather than suppressed.
#[test]
fn tombstone_expires_then_id_is_reusable() {
    let mut r = MessageReassembler::new();
    let id = Uuid::new_v4();
    // A fixed base well above the future-skew tolerance, so timestamps are unambiguous.
    let base = 1_000_000u128;
    let ttl = 100u64;
    let one_chunk = |label: &'static [u8], ts_ms: u128, ttl_ms: u64| Chunk {
        chunk: [0, 1],
        data: Bytes::from_static(label),
        meta: ChunkMeta { id, ts_ms, ttl_ms },
    };

    // Complete a 1-chunk message at t = base; its tombstone expires at base + ttl.
    let first = r.handle_at(one_chunk(b"first", base, ttl), base);
    assert_eq!(first.as_deref(), Some(&b"first"[..]));
    assert!(r.completed_ids.contains(&id), "tombstoned after completion");

    // A full retransmit *within* the TTL window (t = base + ttl/2) is suppressed.
    let dup = r.handle_at(one_chunk(b"first", base, ttl), base + (ttl as u128) / 2);
    assert!(
        dup.is_none(),
        "post-completion retransmit suppressed within TTL"
    );
    assert!(
        r.completed_ids.contains(&id),
        "tombstone still live within TTL"
    );

    // Past the tombstone's expiry (t = base + ttl + 1), a brand-new message reusing the id is
    // delivered: `remove_expired_at` evicts the now-expired tombstone before classify runs.
    let later = base + ttl as u128 + 1;
    let reused = r.handle_at(one_chunk(b"second", later, ttl), later);
    assert_eq!(
        reused.as_deref(),
        Some(&b"second"[..]),
        "id reusable after its tombstone expired via remove_expired_at"
    );
}
