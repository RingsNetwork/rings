# Gateway benchmark record

Reference run: 2026-08-28, Apple M1 Max (10 cores, 64 GB), macOS 15.6.1, release profile.
The implementation worktree was based on `5ebf13489d6486dcebca2d9422f98279e27f5d01`;
the gateway changes were not yet committed when these measurements were taken.

The benchmark uses in-memory packet IO and an echo connector. It measures complete IPv4/TCP
packet reconstruction, stream bridging, payload verification, and simultaneous flow ownership. It
does not measure TUN system calls, WebRTC, Onion cryptography, or public-network throughput.

| Active flows | Bytes/flow | Runtime setup | First-flow setup | Transfer | Payload rate | Max RSS | User / system CPU |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 256 | 1,024 | 0.087 ms | 0.044 ms | 0.0160 s | 15.60 MiB/s | 44,056,576 B | 0.00 / 0.00 s |
| 1,024 | 1,024 | 0.077 ms | 0.044 ms | 0.1136 s | 8.81 MiB/s | 169,885,696 B | 0.08 / 0.02 s |

The 1,024-flow run holds every flow open, so its roughly 162 MiB RSS is a high-water measurement
for the configured 64 KiB application buffer plus the TCP and bridge allocations owned by each
flow. It is not a steady-state per-node minimum.

The benchmark initially exposed a packet-event path that reconciled the entire active-flow table.
At 1,024 flows that version took 4.6438 s at 0.215 MiB/s and consumed 4.51 s user CPU. Scoping
packet and bridge reconciliation to the affected flow reduced the same fixture to 0.1136 s at
8.81 MiB/s and 0.08 s user CPU. Timer ticks retain the linear full scan required for idle
timeouts, with constant-time socket access for every visited flow.

Reproduce the 1,024-flow reference run after warming the release binary:

```bash
cargo build --release -p rings-gateway --example gateway-bench
RINGS_GATEWAY_BENCH_FLOWS=1024 \
  /usr/bin/time -l target/aarch64-apple-darwin/release/examples/gateway-bench
```

Use `/usr/bin/time -v` on Linux. Always record the final commit and host alongside refreshed
numbers; do not compare this synthetic payload rate with a public Onion path as if they measured
the same boundary.
