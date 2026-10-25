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

As verbatim third-party code, `vendor/` is exempt from the 1000-line file rule, which the
owner approved on 2026-10-26.

Remove this patch, with its `[patch.crates-io]` entry, at #1071, the move to webrtc-rs 0.21.
