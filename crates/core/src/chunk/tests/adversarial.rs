use super::*;

#[test]
fn test_expired_partial_messages_are_evicted() {
    let mut r = MessageReassembler::new();
    let now = get_epoch_ms();
    let ttl_ms = 100;
    // Buffer one live partial message through the clock-injected production path.
    r.handle_at(
        Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"x"),
            meta: ChunkMeta {
                id: Uuid::new_v4(),
                ts_ms: now,
                ttl_ms,
            },
        },
        now,
    );
    assert_eq!(r.pending_count(), 1);
    assert_eq!(
        r.remove_expired_at(now.saturating_add(ttl_ms as u128)),
        1,
        "one expired logical message must be reported"
    );
    // A fresh partial can reuse the released capacity.
    r.handle_at(
        Chunk {
            chunk: [0, 2],
            data: Bytes::from_static(b"y"),
            meta: ChunkMeta {
                id: Uuid::new_v4(),
                ts_ms: now.saturating_add(ttl_ms as u128),
                ttl_ms: DEFAULT_TTL_MS,
            },
        },
        now.saturating_add(ttl_ms as u128),
    );
    assert_eq!(r.pending_count(), 1, "only the fresh partial remains");
}

/// A close whose timer cannot run drops every open message with its charge.
#[test]
fn test_close_timer_failure_drops_every_open_message_with_its_charge() {
    let charge = Arc::new(());
    let mut reassembler = MessageReassembler::<Arc<()>>::with_limits(small_limits());
    let now = get_epoch_ms();
    let first = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"retained"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: now,
            ttl_ms: 100,
        },
    };
    let (outcome, _) =
        reassembler.handle_charged_at(first, Arc::clone(&charge), (now, |_| 0), true, None);
    assert!(matches!(outcome, ReassemblyOutcome::Incomplete));
    assert_eq!(
        Arc::strong_count(&charge),
        2,
        "the open message holds its charge"
    );
    assert!(reassembler.prepare_for_close());

    reassembler.discard_after_close_timer_failure();

    assert_eq!(reassembler.pending_count(), 0);
    assert_eq!(Arc::strong_count(&charge), 1, "the charge is released");
}

#[test]
fn test_expired_partial_failure_is_deduplicated_by_logical_message_identity() {
    let mut reassembler = MessageReassembler::new();
    let now = get_epoch_ms();
    let id = Uuid::new_v4();
    let ttl_ms = 100;
    let partial = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"partial"),
        meta: ChunkMeta {
            id,
            ts_ms: now,
            ttl_ms,
        },
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(partial.clone(), now),
        ReassemblyOutcome::Incomplete
    ));
    assert_eq!(
        reassembler.remove_expired_at(now.saturating_add(ttl_ms as u128)),
        1
    );
    assert!(matches!(
        reassembler.handle_retained_outcome_at(partial, now.saturating_add(ttl_ms as u128)),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));

    let fresh_transmission = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"fresh"),
        meta: ChunkMeta {
            id,
            ts_ms: now.saturating_add(1),
            ttl_ms,
        },
    };
    assert!(matches!(
        reassembler
            .handle_retained_outcome_at(fresh_transmission, now.saturating_add(ttl_ms as u128)),
        ReassemblyOutcome::Incomplete
    ));
}

#[test]
fn test_expired_partial_replay_is_independent_of_tombstone_capacity() {
    let mut limits = small_limits();
    limits.max_completed_ids = 1;
    let mut reassembler = MessageReassembler::with_limits(limits);
    let now = get_epoch_ms();
    let ttl_ms = 100;
    let partial = |id| Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"partial"),
        meta: ChunkMeta {
            id,
            ts_ms: now,
            ttl_ms,
        },
    };
    let first = partial(Uuid::new_v4());
    let second = partial(Uuid::new_v4());
    assert!(matches!(
        reassembler.handle_retained_outcome_at(first.clone(), now),
        ReassemblyOutcome::Incomplete
    ));
    assert!(matches!(
        reassembler.handle_retained_outcome_at(second, now),
        ReassemblyOutcome::Incomplete
    ));
    let expiry = now.saturating_add(ttl_ms as u128);
    assert_eq!(reassembler.remove_expired_at(expiry), 2);
    assert!(matches!(
        reassembler.handle_retained_outcome_at(first, expiry),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
}

#[test]
fn test_pending_invalid_failure_is_charged_once_and_not_again_at_expiry() {
    let mut reassembler = MessageReassembler::new();
    let now = get_epoch_ms();
    let ttl_ms = 100;
    let first = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"first"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: now,
            ttl_ms,
        },
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(first.clone(), now),
        ReassemblyOutcome::Incomplete
    ));
    let conflict = Chunk {
        data: Bytes::from_static(b"conflict"),
        ..first
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(conflict.clone(), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
    assert!(matches!(
        reassembler.handle_retained_outcome_at(conflict, now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
    assert_eq!(
        reassembler.remove_expired_at(now.saturating_add(ttl_ms as u128)),
        0,
        "expiry must not charge a logical failure already reported as invalid"
    );
}

#[test]
fn test_invalid_new_uuid_has_one_bounded_terminal_outcome() {
    let mut reassembler = MessageReassembler::new();
    let now = get_epoch_ms();
    let meta = ChunkMeta {
        id: Uuid::new_v4(),
        ts_ms: now,
        ttl_ms: DEFAULT_TTL_MS,
    };
    let invalid = Chunk {
        chunk: [0, 0],
        data: Bytes::from_static(b"invalid"),
        meta,
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(invalid.clone(), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
    assert!(matches!(
        reassembler.handle_retained_outcome_at(invalid, now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
    let valid = Chunk {
        chunk: [0, 1],
        data: Bytes::from_static(b"valid"),
        meta,
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(valid, now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
}

#[test]
fn test_invalid_terminal_tracking_fails_open_at_its_memory_bound() {
    let mut limits = small_limits();
    limits.max_completed_ids = 1;
    let mut reassembler = MessageReassembler::with_limits(limits);
    let now = get_epoch_ms();
    let invalid = |id| Chunk {
        chunk: [0, 0],
        data: Bytes::from_static(b"invalid"),
        meta: ChunkMeta {
            id,
            ts_ms: now,
            ttl_ms: DEFAULT_TTL_MS,
        },
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(invalid(Uuid::new_v4()), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
    assert!(matches!(
        reassembler.handle_retained_outcome_at(invalid(Uuid::new_v4()), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
}

#[test]
fn test_invalid_terminal_saturation_preserves_replay_after_older_witness_drains() {
    let mut limits = small_limits();
    limits.max_completed_ids = 1;
    let mut reassembler = MessageReassembler::with_limits(limits);
    let now = get_epoch_ms();
    let occupying_failure = Chunk {
        chunk: [0, 0],
        data: Bytes::from_static(b"invalid"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: now,
            ttl_ms: DEFAULT_TTL_MS,
        },
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(occupying_failure, now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));

    let pending_at = now.saturating_add(100);
    let ttl_ms = 100;
    let untracked_expiry = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"partial"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: pending_at,
            ttl_ms,
        },
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(untracked_expiry.clone(), pending_at),
        ReassemblyOutcome::Incomplete
    ));
    let expiry = pending_at.saturating_add(ttl_ms as u128);
    assert_eq!(reassembler.remove_expired_at(expiry), 1);

    // The older occupying witness has drained, but the later untracked failure's bounded
    // saturation horizon still prevents the same logical transmission from being charged twice.
    let after_older_witness = now.saturating_add(MAX_TTL_MS as u128).saturating_add(1);
    assert!(after_older_witness < expiry.saturating_add(MAX_TTL_MS as u128));
    assert!(matches!(
        reassembler.handle_retained_outcome_at(untracked_expiry, after_older_witness),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));

    // A second scored expiry near the first saturation deadline must extend the horizon. Merely
    // observing that a prior saturation interval is active is insufficient: this pending message
    // has just contributed a real failure and therefore needs its own bounded replay protection.
    let first_saturation_end = expiry.saturating_add(MAX_TTL_MS as u128);
    let second_pending_at = first_saturation_end.saturating_sub(50);
    let second_untracked_expiry = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"second-partial"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: second_pending_at,
            ttl_ms: 10,
        },
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(second_untracked_expiry.clone(), second_pending_at),
        ReassemblyOutcome::Incomplete
    ));
    let second_expiry = second_pending_at.saturating_add(10);
    assert_eq!(reassembler.remove_expired_at(second_expiry), 1);
    assert!(matches!(
        reassembler.handle_retained_outcome_at(
            second_untracked_expiry,
            first_saturation_end.saturating_add(1),
        ),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
}

#[test]
fn test_invalid_oversize_ttl_cannot_immediately_expire_its_terminal_id() {
    let mut reassembler = MessageReassembler::new();
    let now = get_epoch_ms();
    let invalid = Chunk {
        chunk: [0, 1],
        data: Bytes::from_static(b"invalid"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: now.saturating_sub(MAX_TTL_MS as u128 + 10),
            ttl_ms: MAX_TTL_MS.saturating_add(1),
        },
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(invalid.clone(), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
    assert!(matches!(
        reassembler.handle_retained_outcome_at(invalid, now.saturating_add(1)),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
}

#[test]
fn test_metadata_mismatch_terminates_one_in_flight_uuid_once() {
    let mut reassembler = MessageReassembler::new();
    let now = get_epoch_ms();
    let ttl_ms = 100;
    let first = Chunk {
        chunk: [0, 2],
        data: Bytes::from_static(b"first"),
        meta: ChunkMeta {
            id: Uuid::new_v4(),
            ts_ms: now,
            ttl_ms,
        },
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(first.clone(), now),
        ReassemblyOutcome::Incomplete
    ));
    let mismatched = Chunk {
        chunk: [1, 2],
        data: Bytes::from_static(b"mismatch"),
        meta: ChunkMeta {
            ts_ms: now.saturating_add(1),
            ..first.meta
        },
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(mismatched.clone(), now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Invalid)
    ));
    assert!(matches!(
        reassembler.handle_retained_outcome_at(mismatched, now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
    let original_tail = Chunk {
        chunk: [1, 2],
        data: Bytes::from_static(b"tail"),
        ..first
    };
    assert!(matches!(
        reassembler.handle_retained_outcome_at(original_tail, now),
        ReassemblyOutcome::Rejected(ReassemblyRejection::Replay)
    ));
    assert_eq!(
        reassembler.remove_expired_at(now.saturating_add(ttl_ms as u128)),
        0
    );
}

#[test]
fn test_pending_messages_are_capped() {
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
fn test_round_trip_reordered_with_duplicates() {
    let data: Bytes = "abcdefghij".repeat(500).into();
    let mut chunks = chunks_of(&data, 64);
    // Reorder after the first chunk, which opens the message, and inject duplicates mid-stream
    // (not after the final chunk, which a tombstone would drop as a late retransmit).
    chunks[1..].reverse();
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
fn test_total_over_slot_cap_is_rejected() {
    let limits = small_limits();
    let mut r = MessageReassembler::with_limits(limits);
    let out = r.handle(Chunk {
        chunk: [0, limits.max_chunks_per_message + 1],
        data: Bytes::from_static(b"x"),
        meta: ChunkMeta::default(),
    });
    assert!(out.is_none());
    assert_eq!(r.pending_count(), 0);
}

/// Two chunks sharing an id/total but from different transmissions (different `ts_ms`/`ttl_ms`)
/// must not be merged into one pending entry.
#[test]
fn test_mismatched_ts_or_ttl_for_same_id_is_rejected() {
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
fn test_tombstone_expires_then_id_is_reusable() {
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
