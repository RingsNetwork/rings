# Rings dependency patch

This directory mirrors `webrtc-sctp 0.17.1` from crates.io, the version `webrtc 0.17` resolves
to. The one code change is in `src/association/mod.rs`, `Association::write_loop`: each batch
of packets that `gather_outbound` returns is marshalled in one `spawn_blocking` task, and the
packets are sent in the order they were gathered, instead of one `spawn_blocking` per packet
(#1090). `src/association/association_test.rs` adds
`test_assoc_write_loop_sends_data_chunks_in_tsn_order`.

The per-packet blocking task cost about 38% of the busy CPU of the #843 onion loop on a
multi-thread runtime, spent in the blocking pool's lock, against 1.5% for the marshalling it
moves off the loop. The batch keeps upstream's reason for the blocking task (#363: the write
loop suspends into the pool so the read loop can take the association lock) and its ordering
(#513), and adds no yield before the lock, which upstream reverted for deadlocking (#679, #682).
The same change is proposed upstream as webrtc-rs/webrtc#941, against `v0.17.x`.

## Test fix (#1127), not in webrtc-rs/webrtc#941

Three upstream tests sleep 10 ms after writing and then drop or reorder the packets they expect
on the `Bridge`'s queue: `test_assoc_reliable_retransmission` (`drop_offset(0, 0, 1)`),
`test_assoc_reliable_ordered_reordered` (`reorder(0)`) and
`test_assoc_unreliable_rexmit_unordered_fragment` (`drop_offset(0, 0, 2)`). A packet reaches the
queue only after the write loop wakes, one blocking-pool round trip, and `Bridge::push`'s timer
sleep (at least one tick). Upstream and the batch patch share that path: each takes one round
trip before a gather's first send. So the 10 ms is a race in the upstream tests, not one the
batch patch introduced. Upstream already disables two of these tests on some platforms for
"flakiness with timing/IO interactions". On a loaded runner the queue can still be empty, and
`drop_offset` panics with "range end index 1 out of range for slice of length 0" (#1127).

The fix waits on the event each test needs. The client's `Conn` is wrapped in the
`RecordingConn` that also serves the TSN-order test; it counts sends that have returned, and a
packet is in the bridge's queue once its send returns. Each test waits until the packets it acts
on are queued, with a 5 s timeout as a failure bound only. The retransmission test also replaces
its fixed 200 ms of ticks with `flush_buffers`, which ticks until both messages are acknowledged.

Two timing assumptions of the same kind remain upstream as they are, and have not been seen
failing here. `test_assoc_congestion_control_fast_retransmission` ticks for a fixed 500 ms
before it reads, and `test_assoc_abort` sleeps 10 ms before it reads the closed state. This
fix can be offered upstream on its own; it is independent of webrtc-rs/webrtc#941.

## The 1000-line rule

As verbatim third-party code, `vendor/` is exempt from the 1000-line file rule, which the
owner approved on 2026-10-26.

## Removal

Remove this patch, with its `[patch.crates-io]` entry, at #1071, the move to webrtc-rs 0.21.
