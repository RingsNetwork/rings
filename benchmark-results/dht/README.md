# DHT Benchmark Artifacts - 2026-07-05

Commit: base Docker artifacts were collected from `2efb302bf92a7b383e10c3c6d17b0a7764b92579`;
the dummy simulator JSONL was regenerated after applying the Chord wrapping
finger fix tracked by issue #658 in this PR branch.
Machine: Apple M1 Max, 68719476736 bytes RAM, macOS-15.6.1-arm64-arm-64bit-Mach-O
Tools: `rustc 1.94.1 (e408947bf 2026-03-25)`, `cargo 1.94.1 (29ea6fb6a 2026-03-24)`, `Docker version 28.3.2, build 578ccf607d`
Docker image: `rings-node-cluster:benchmark-artifacts-2efb302b` / `sha256:1c5d41481958e3fe0f403b53955831cbbd36d2041bf3954a34d71ad02b6a5b5a`

## Files

- `dummy-paper-scale-2026-07-05.jsonl`: Paper-scale Chord baseline from the dummy DHT simulator.
- `docker-convergence-16node-2026-07-05.jsonl`: Fresh 16-node ring Docker/WebRTC cluster samples from cluster-ready for about 5 minutes.
- `docker-transport-16node-2026-07-05.jsonl`: Real WebRTC node-internal transport burst payload sweep on fixed node0 -> node1.
- `environment-2026-07-05.json`: Machine, tool, image, commit, and command metadata.

## Commands

- `docker_convergence`:

```sh
python3 scripts/dht_docker_cluster_bench.py --container rings-cluster-16-artifacts --nodes 16 --finger-table-size 16 --cluster-topology ring --stabilize-interval-seconds 1 --docker-image rings-node-cluster:benchmark-artifacts-2efb302b --samples 1 (looped every 10s for 30 samples)
```

- `docker_ring_start`:

```sh
docker run -d --rm --name rings-cluster-16-artifacts -e RINGS_NODE_COUNT=16 -e RINGS_ALLOW_RANDOM_KEYS=true -e RINGS_CONNECT_TOPOLOGY=ring -e RINGS_STABILIZE_INTERVAL=1 -e RINGS_DHT_FINGER_TABLE_SIZE=16 -e RINGS_READY_RETRIES=180 -e RINGS_LOG_LEVEL=warn rings-node-cluster:benchmark-artifacts-2efb302b
```

- `docker_transport_sweep`:

```sh
python3 scripts/dht_docker_cluster_bench.py --container rings-cluster-16-artifacts --nodes 16 --finger-table-size 16 --cluster-topology ring --stabilize-interval-seconds 1 --docker-image rings-node-cluster:benchmark-artifacts-2efb302b --throughput-source-index 0 --throughput-destination-index 1 --throughput-flush-timeout-ms 60000 (payload sweep)
```

- `dummy_paper_scale`:

```sh
RINGS_DHT_BENCH_NODES=1600 RINGS_DHT_BENCH_LOOKUPS_PER_NODE=64 RINGS_DHT_BENCH_FINGER_TABLE_SIZES=160 cargo bench -p rings-core --bench dht_network_sim --no-default-features --features dummy
```

## Dummy DHT Simulator

This is the Chord-style baseline for paper object `B_C(eta)`. It does not use WebRTC; it uses deterministic Chord routing state with wrapping Chord finger semantics at `N=1600`, `finger_table_size=160`, and `lookups_per_node=64`.

| scenario | active nodes | lookups | correctness | avg hops | timeouts | failures / 10k | full matches |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| stable | 1600 | 102400 | 100.00% | 5.469 | 0 | 0.000 | 1600 |
| failed_nodes_10pct | 1440 | 92160 | 99.02% | 5.758 | 67623 | 98.199 | 398 |
| failed_nodes_20pct | 1280 | 81920 | 96.85% | 6.186 | 154801 | 314.697 | 97 |
| churn_5pct | 1600 | 102400 | 94.54% | 5.617 | 36979 | 546.191 | 456 |
| churn_10pct | 1600 | 102400 | 89.57% | 5.756 | 74269 | 1043.359 | 269 |
| churn_15pct | 1600 | 102400 | 83.95% | 5.923 | 123922 | 1605.273 | 277 |
| churn_20pct | 1600 | 102400 | 77.88% | 6.151 | 185826 | 2211.621 | 331 |
| churn_25pct | 1600 | 102400 | 69.80% | 6.270 | 241159 | 3019.922 | 406 |
| churn_30pct | 1600 | 102400 | 62.86% | 6.485 | 327091 | 3713.574 | 480 |
| churn_35pct | 1600 | 102400 | 55.63% | 6.659 | 409032 | 4436.914 | 560 |
| churn_40pct | 1600 | 102400 | 48.39% | 6.746 | 471442 | 5161.133 | 640 |

The dummy JSONL includes per-scenario `build_elapsed_ms` and
`lookup_elapsed_ms`. Summed across the 11 emitted scenarios, build time was
519.763 ms and lookup time was 69914.777 ms. After switching to wrapping
finger semantics, the stable baseline remained `avg_hops=5.469355`; the change
aligns runtime/spec finger semantics but does not by itself reduce this
aggregate stable hop count.

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
- Use Docker/WebRTC convergence and transport artifacts as Rings implementation evidence, not as the Chord theoretical baseline.
- Do not collapse the fresh-cluster Docker convergence result into a steady-state lookup number. This run did not converge within the sampled window.
