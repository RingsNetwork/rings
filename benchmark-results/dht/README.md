# DHT Benchmark Artifacts - 2026-07-05

Commit: base Docker artifacts were collected from `2efb302bf92a7b383e10c3c6d17b0a7764b92579`;
the dummy simulator JSONL was regenerated after applying the Chord wrapping
finger fix tracked by issue #658 and adding the `maintained_chord`
maintenance model in this PR branch.
Machine: Apple M1 Max, 68719476736 bytes RAM, macOS-15.6.1-arm64-arm-64bit-Mach-O
Tools: `rustc 1.94.1 (e408947bf 2026-03-25)`, `cargo 1.94.1 (29ea6fb6a 2026-03-24)`, `Docker version 28.3.2, build 578ccf607d`
Docker image: `rings-node-cluster:benchmark-artifacts-2efb302b` / `sha256:1c5d41481958e3fe0f403b53955831cbbd36d2041bf3954a34d71ad02b6a5b5a`

## Files

- `dummy-paper-scale-2026-07-05.jsonl`: Paper-scale Chord baseline from the dummy DHT simulator.
- `chord-paper-sim-2026-07-05.jsonl`: Paper-aligned simulator rows for Chord Table II/III/IV and Fig. 8/9/10.
- `docker-convergence-16node-2026-07-05.jsonl`: Fresh 16-node ring Docker/WebRTC cluster samples from cluster-ready for about 5 minutes.
- `docker-transport-16node-2026-07-05.jsonl`: Real WebRTC node-internal transport burst payload sweep on fixed node0 -> node1.
- `environment-2026-07-05.json`: Machine, tool, image, commit, and command metadata.

## Commands

- `docker_convergence`:

```sh
for port in $(seq 50000 50015); do docker exec rings-cluster-16-artifacts curl -fsS "http://127.0.0.1:${port}/status"; done
```

The JSONL artifact was built from these real-node status snapshots, sampled
every 10 seconds for 30 samples. This PR does not commit a reusable Docker
collector script; if that automation is reintroduced, it should be a Rust
tool/bench.

- `docker_ring_start`:

```sh
docker run -d --rm --name rings-cluster-16-artifacts -e RINGS_NODE_COUNT=16 -e RINGS_ALLOW_RANDOM_KEYS=true -e RINGS_CONNECT_TOPOLOGY=ring -e RINGS_STABILIZE_INTERVAL=1 -e RINGS_DHT_FINGER_TABLE_SIZE=16 -e RINGS_READY_RETRIES=180 -e RINGS_LOG_LEVEL=warn rings-node-cluster:benchmark-artifacts-2efb302b
```

- `docker_transport_sweep`:

```sh
docker exec -i rings-cluster-16-artifacts curl -fsS -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"transportBenchmark","params":{"destination_did":"0x1dd69acbed7f795a9ad105fa97cf60b72d531288","namespace":"bench","payload_bytes":65536,"messages":1024,"flush_timeout_ms":60000}}' http://127.0.0.1:50000/
```

The committed transport JSONL contains the repeated node0 -> node1 payload
sweep and the before/after `listPeerMeasurements` deltas used for the
throughput summary.

- `dummy_paper_scale`:

```sh
RINGS_DHT_BENCH_NODES=1600 RINGS_DHT_BENCH_LOOKUPS_PER_NODE=64 RINGS_DHT_BENCH_FINGER_TABLE_SIZES=160 cargo bench -p rings-core --bench dht_network_sim --no-default-features --features dummy
```

By default, the dummy bench emits `instant_stale_snapshot` and
`maintained_chord` rows for failure/churn scenarios. Use
`RINGS_DHT_BENCH_MAINTENANCE_MODELS=instant_stale_snapshot` or
`RINGS_DHT_BENCH_MAINTENANCE_MODELS=maintained_chord` to limit the emitted
maintenance model. Use `RINGS_DHT_BENCH_SUCCESSOR_CAPACITY=<n>` to override
the default successor-list capacity of 3; use `20` when mapping directly to the
original Chord paper's Table II/III successor-list setting.

- `chord_paper_sim`:

```sh
cargo bench -p rings-core --bench chord_paper_sim --no-default-features --features dummy -- --include all > benchmark-results/dht/chord-paper-sim-2026-07-05.jsonl
```

## Chord Paper Alignment

`chord-paper-sim-2026-07-05.jsonl` is a simulator-only artifact for the
original Chord paper's numbered tables and data figures. It exists so paper
claims can be mapped one-to-one to a local artifact. It is not evidence that the
Rings runtime implements every modeled feature.

The paper-aligned simulator is implemented as a Rust cargo bench and emits
`ring_identifier_bits=64`. Use `dummy-paper-scale-2026-07-05.jsonl` for the
160-bit Rings DID baseline.

| paper item | original metric | artifact coverage |
| --- | --- | --- |
| Table I | Chord variable definitions | no benchmark row required; implementation semantics only |
| Table II | simultaneous failures, `N=1000`, successor list `r=20`, 10,000 lookups | `paper_item=table_ii`, failed nodes 0%-50% |
| Table III | lookups during stabilization, paired join/leave rates 0.05-0.40/s, `N~=1000`, `r=20` | `paper_item=table_iii`, event-driven dummy churn |
| Table IV | lookup latency stretch, `N=2^16`, `s=1,2,4,8,16`, iterative/recursive, 3D/transit-stub | `paper_item=table_iv` |
| Fig. 8 | consistent-hashing load balance, `10^4` nodes, `10^5..10^6` keys, 20 seeds | `paper_item=fig_8a` and `fig_8b` |
| Fig. 9 | virtual nodes load balance, `10^4` real nodes, `10^6` keys, `r=1,2,5,10,20` virtual nodes | `paper_item=fig_9`, `runtime_support=simulator_only` |
| Fig. 10 | path length scaling, `N=2^k`, `k=3..14`, plus PDF at `k=12` | `paper_item=fig_10a` and `fig_10b` |

Rings does not currently have Chord-style virtual nodes, meaning one physical
node advertising multiple unrelated ring positions for ownership/routing. The
old `VNode` terminology is storage-entry terminology, not this Chord feature.
Issue #659 tracks the runtime design decision for real virtual-node support.

### Paper Simulator Highlights

Table II, simultaneous node failures:

| failed nodes | avg path length | path p1/p99 | avg timeouts | timeout p1/p99 | failures / 10k |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 0% | 3.841 | (1, 6) | 0.000 | (0, 0) | 0.0 |
| 10% | 3.973 | (1, 7) | 0.419 | (0, 3) | 0.0 |
| 20% | 4.114 | (1, 7) | 0.972 | (0, 5) | 0.0 |
| 30% | 4.418 | (1, 8) | 1.980 | (0, 8) | 0.0 |
| 40% | 4.691 | (1, 9) | 3.291 | (0, 13) | 0.0 |
| 50% | 5.502 | (1, 12) | 6.938 | (0, 27) | 0.0 |

Table III, lookups during stabilization:

| join/leave rate | avg path length | path p1/p99 | avg timeouts | timeout p1/p99 | failures / 10k |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 0.05 / 1.5 | 3.904 | (1, 7) | 0.081 | (0, 2) | 6.0 |
| 0.10 / 3.0 | 3.932 | (1, 7) | 0.160 | (0, 2) | 17.0 |
| 0.15 / 4.5 | 4.032 | (1, 7) | 0.257 | (0, 3) | 21.0 |
| 0.20 / 6.0 | 4.118 | (1, 7) | 0.359 | (0, 3) | 36.0 |
| 0.25 / 7.5 | 4.169 | (1, 7) | 0.419 | (0, 4) | 43.0 |
| 0.30 / 9.0 | 4.233 | (1, 8) | 0.510 | (0, 4) | 41.0 |
| 0.35 / 10.5 | 4.275 | (1, 8) | 0.571 | (0, 4) | 53.0 |
| 0.40 / 12.0 | 4.385 | (1, 8) | 0.672 | (0, 5) | 59.0 |

The Table III simulator keeps the node count stable with paired join/leave
events and models periodic successor-list plus one-finger stabilization. Its
path length and timeout trends are paper-aligned, but its lookup failure counts
are more pessimistic than the original Chord table because it does not model
every departure optimization and application-level retry behavior from the
paper.

Table IV, lookup stretch medians with p10/p90 in parentheses:

| s | iterative 3D | recursive 3D | iterative transit-stub | recursive transit-stub |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 7.17 (4.47, 13.63) | 3.98 (2.53, 7.73) | 8.46 (6.00, 11.46) | 4.73 (3.50, 6.23) |
| 2 | 6.42 (4.04, 12.18) | 3.57 (2.29, 6.98) | 8.46 (5.91, 11.00) | 4.68 (3.46, 6.00) |
| 4 | 5.61 (3.57, 10.57) | 3.18 (2.08, 6.07) | 7.91 (5.46, 10.91) | 4.46 (3.23, 5.96) |
| 8 | 4.86 (3.15, 9.08) | 2.82 (1.86, 5.36) | 7.37 (5.00, 10.00) | 4.18 (2.96, 5.46) |
| 16 | 4.31 (2.80, 7.88) | 2.52 (1.71, 4.83) | 6.47 (4.46, 9.19) | 3.68 (2.68, 4.96) |

Fig. 9, simulator-only virtual-node load balance:

| virtual nodes / real node | mean keys | p1 | p99 | max |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 100.0 | 1 | 478 | 866 |
| 2 | 100.0 | 7 | 332 | 619 |
| 5 | 100.0 | 25 | 235 | 374 |
| 10 | 100.0 | 41 | 188 | 268 |
| 20 | 100.0 | 56 | 159 | 227 |

## Dummy DHT Simulator

This is the Chord-style baseline for paper object `B_C(eta)`. It does not use WebRTC; it uses deterministic Chord routing state with wrapping Chord finger semantics at `N=1600`, `finger_table_size=160`, and `lookups_per_node=64`. The `instant_stale_snapshot` rows keep stale failure/churn routing state without repair; the `maintained_chord` rows rebuild active nodes' successor/finger state over the current active ring, modelling post-maintenance steady-state Chord.

| scenario | model | active nodes | lookups | correctness | avg lookup rounds | avg forward hops | timeouts | failures / 10k | full matches |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| stable | maintained_chord | 1600 | 102400 | 100.00% | 5.469 | 4.469 | 0 | 0.000 | 1600 |
| failed_nodes_10pct | instant_stale_snapshot | 1440 | 92160 | 99.02% | 5.758 | 4.758 | 67623 | 98.199 | 398 |
| failed_nodes_10pct | maintained_chord | 1440 | 92160 | 100.00% | 5.394 | 4.394 | 0 | 0.000 | 1440 |
| failed_nodes_20pct | instant_stale_snapshot | 1280 | 81920 | 96.85% | 6.186 | 5.186 | 154801 | 314.697 | 97 |
| failed_nodes_20pct | maintained_chord | 1280 | 81920 | 100.00% | 5.316 | 4.316 | 0 | 0.000 | 1280 |
| churn_5pct | instant_stale_snapshot | 1600 | 102400 | 94.54% | 5.617 | 4.617 | 36979 | 546.191 | 456 |
| churn_5pct | maintained_chord | 1600 | 102400 | 100.00% | 5.465 | 4.465 | 0 | 0.000 | 1600 |
| churn_10pct | instant_stale_snapshot | 1600 | 102400 | 89.57% | 5.756 | 4.756 | 74269 | 1043.359 | 269 |
| churn_10pct | maintained_chord | 1600 | 102400 | 100.00% | 5.464 | 4.464 | 0 | 0.000 | 1600 |
| churn_15pct | instant_stale_snapshot | 1600 | 102400 | 83.95% | 5.923 | 4.923 | 123922 | 1605.273 | 277 |
| churn_15pct | maintained_chord | 1600 | 102400 | 100.00% | 5.452 | 4.452 | 0 | 0.000 | 1600 |
| churn_20pct | instant_stale_snapshot | 1600 | 102400 | 77.88% | 6.151 | 5.151 | 185826 | 2211.621 | 331 |
| churn_20pct | maintained_chord | 1600 | 102400 | 100.00% | 5.458 | 4.458 | 0 | 0.000 | 1600 |
| churn_25pct | instant_stale_snapshot | 1600 | 102400 | 69.80% | 6.270 | 5.270 | 241159 | 3019.922 | 406 |
| churn_25pct | maintained_chord | 1600 | 102400 | 100.00% | 5.456 | 4.456 | 0 | 0.000 | 1600 |
| churn_30pct | instant_stale_snapshot | 1600 | 102400 | 62.86% | 6.485 | 5.485 | 327091 | 3713.574 | 480 |
| churn_30pct | maintained_chord | 1600 | 102400 | 100.00% | 5.456 | 4.456 | 0 | 0.000 | 1600 |
| churn_35pct | instant_stale_snapshot | 1600 | 102400 | 55.63% | 6.659 | 5.659 | 409032 | 4436.914 | 560 |
| churn_35pct | maintained_chord | 1600 | 102400 | 100.00% | 5.465 | 4.465 | 0 | 0.000 | 1600 |
| churn_40pct | instant_stale_snapshot | 1600 | 102400 | 48.39% | 6.746 | 5.746 | 471442 | 5161.133 | 640 |
| churn_40pct | maintained_chord | 1600 | 102400 | 100.00% | 5.470 | 4.470 | 0 | 0.000 | 1600 |

The dummy JSONL includes per-scenario `build_elapsed_ms` and
`lookup_elapsed_ms`. Summed across the 21 emitted scenarios, build time was
1031.342 ms and lookup time was 126267.594 ms. `avg_hops` in the JSONL is
lookup-round count; `avg_forward_hops` subtracts the terminal resolution round
and is the metric to compare with the Chord paper's forwarding/path length.
After switching to wrapping finger semantics, the stable baseline remained
`avg_hops=5.469355` and `avg_forward_hops=4.469355`; the change aligns
runtime/spec finger semantics but does not by itself reduce this aggregate
stable hop count.

For paper Chord-baseline comparisons, use the `maintained_chord` rows. The
`instant_stale_snapshot` rows are retained as a harsher no-maintenance pressure
test that explains the large timeout/failure counts.

## Docker/WebRTC Convergence

The Docker/WebRTC cluster used real native node daemons, `RINGS_CONNECT_TOPOLOGY=ring`, `RINGS_STABILIZE_INTERVAL=1`, `RINGS_DHT_FINGER_TABLE_SIZE=16`, and `node_count=16`. Sampling began immediately after `cluster ready`.

| samples | window seconds | successor matches first/last | predecessor matches first/last | full matches first/last | connected edges first/last | correctness first/last | avg hops first/last | failures / 10k first/last |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 30 | 309.532 | 0/0 | 3/3 | 0/0 | 86/86 | 19.41%/19.41% | 1.026/1.026 | 8058.594/8058.594 |

This run did not reach the steady-state criterion used by the collector notes (`correctness >= 99%` with at least 15 first-successor and predecessor matches for three consecutive samples). Therefore this artifact is convergence evidence, not a valid steady-state Docker lookup window.

## Docker/WebRTC Transport

Transport samples use one internal `transportBenchmark` RPC from node0 to node1. Throughput is computed from application payload bytes and the source-side flushed-message measurement. Failure counters are populated from `listPeerMeasurements`.

| payload bytes | messages | repeats | total payload / run | flush Mbps min/mean/max | admission Mbps mean | source sent delta total | destination received delta total | send failures | receive failures | timeouts |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1024 | 4096 | 2 | 4194304 | 10.221/10.462/10.702 | 11.579 | 8192 | 8192 | 0 | 0 | 0 |
| 16384 | 1024 | 2 | 16777216 | 105.832/107.353/108.873 | 141.687 | 2048 | 2048 | 0 | 0 | 0 |
| 65536 | 1024 | 2 | 67108864 | 89.590/90.358/91.126 | 100.671 | 2048 | 6144 | 0 | 0 | 0 |

For 64 KiB payloads, `destination_received_delta` is larger than application message count because the peer measurement counts lower-level accepted transport chunks/messages. `source_sent_delta` tracks the application-level benchmark burst count in these runs.

## Paper Use

- Use `dummy-paper-scale-2026-07-05.jsonl` for the Chord baseline curves and tables.
- Prefer the `maintained_chord` rows for comparisons against the original Chord paper.
- Use `instant_stale_snapshot` rows only when discussing stale-state sensitivity after abrupt failure/churn without repair.
- Use Docker/WebRTC convergence and transport artifacts as Rings implementation evidence, not as the Chord theoretical baseline.
- Do not collapse the fresh-cluster Docker convergence result into a steady-state lookup number. This run did not converge within the sampled window.
