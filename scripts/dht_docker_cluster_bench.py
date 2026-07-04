#!/usr/bin/env python3
"""Collect DHT benchmark metrics from a running docker/cluster Rings container.

The collector uses the real native node daemons inside the Docker cluster. DHT
lookup metrics are computed by replaying the successor/finger state exposed by
each node's /status endpoint, so the report explicitly labels them as a status
snapshot routing model. Optional transport sampling starts a node-internal
transport burst with one RPC call, so the measured loop runs inside the source
node process instead of paying one docker/curl/JSON-RPC round trip per message.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from collections import Counter
from typing import Any

RING_BITS = 160
RING_SIZE = 1 << RING_BITS


def env_int(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    return int(raw)


def docker_exec(
    container: str,
    command: list[str],
    *,
    stdin: bytes | None = None,
    timeout: int = 30,
) -> bytes:
    proc = subprocess.run(
        ["docker", "exec", "-i", container, *command],
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
    )
    if proc.returncode != 0:
        stderr = proc.stderr.decode("utf-8", "replace").strip()
        raise RuntimeError(
            f"docker exec {container} {' '.join(command)!r} failed: {stderr}"
        )
    return proc.stdout


def status(container: str, port: int) -> dict[str, Any]:
    raw = docker_exec(
        container,
        ["curl", "-fsS", f"http://127.0.0.1:{port}/status"],
    )
    return json.loads(raw)


def rpc(
    container: str,
    port: int,
    method: str,
    params: dict[str, Any],
    *,
    timeout: int = 60,
) -> dict[str, Any]:
    body = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    ).encode()
    raw = docker_exec(
        container,
        [
            "curl",
            "-fsS",
            "-H",
            "content-type: application/json",
            "-d",
            "@-",
            f"http://127.0.0.1:{port}/",
        ],
        stdin=body,
        timeout=timeout,
    )
    response = json.loads(raw)
    if "error" in response:
        raise RuntimeError(f"rpc {method} on {port} failed: {response['error']}")
    return response["result"]


def did_int(did: str) -> int:
    return int(did, 16)


def clockwise_distance(start: str, end: str) -> int:
    return (did_int(end) - did_int(start)) % RING_SIZE


def expand_fingers(dht: dict[str, Any], inferred_size: int) -> list[str | None]:
    size = inferred_size
    for entry in dht.get("finger_table_ranges", []):
        end = int(entry["end"])
        size = max(size, end + 1)

    fingers: list[str | None] = [None] * size
    for entry in dht.get("finger_table_ranges", []):
        did = entry.get("did")
        if did is None:
            continue
        start = int(entry["start"])
        end = int(entry["end"])
        for index in range(start, min(end + 1, len(fingers))):
            fingers[index] = did
    return fingers


def collect_nodes(
    container: str,
    node_count: int,
    base_internal_port: int,
    configured_finger_table_size: int | None,
) -> list[dict[str, Any]]:
    raw_statuses = [status(container, base_internal_port + i) for i in range(node_count)]
    inferred_size = configured_finger_table_size or 0
    if inferred_size == 0:
        for raw in raw_statuses:
            dht = raw["swarm"]["dht"]
            for entry in dht.get("finger_table_ranges", []):
                inferred_size = max(inferred_size, int(entry["end"]) + 1)

    nodes = []
    for index, raw in enumerate(raw_statuses):
        dht = raw["swarm"]["dht"]
        nodes.append(
            {
                "index": index,
                "port": base_internal_port + index,
                "did": dht["did"],
                "successors": dht.get("successors", []),
                "predecessor": dht.get("predecessor"),
                "fingers": expand_fingers(dht, inferred_size),
                "peers": raw["swarm"].get("peers", []),
                "version": raw.get("version"),
            }
        )
    return nodes


def expected_successors(local: str, all_dids: list[str], capacity: int) -> list[str]:
    return sorted(
        [did for did in all_dids if did != local],
        key=lambda did: clockwise_distance(local, did),
    )[:capacity]


def expected_predecessor(local: str, all_dids: list[str]) -> str | None:
    peers = [did for did in all_dids if did != local]
    if not peers:
        return None
    return max(peers, key=lambda did: clockwise_distance(local, did))


def expected_finger(local: str, all_dids: list[str], bit: int) -> str | None:
    threshold = 1 << bit
    candidates = [
        did
        for did in all_dids
        if did != local and clockwise_distance(local, did) >= threshold
    ]
    if not candidates:
        return None
    return min(candidates, key=lambda did: clockwise_distance(local, did))


def expected_owner(target: str, all_dids: list[str]) -> str:
    return min(all_dids, key=lambda did: clockwise_distance(target, did))


def modeled_lookup(
    origin: str,
    target: str,
    nodes_by_did: dict[str, dict[str, Any]],
    max_hops: int,
) -> tuple[str | None, int, int]:
    current = origin
    visited: set[str] = set()

    for hops in range(1, max_hops + 1):
        if current in visited:
            return None, hops, 1
        visited.add(current)

        node = nodes_by_did[current]
        successors = [did for did in node["successors"] if did in nodes_by_did]
        if not successors:
            return None, hops, 1

        head = successors[0]
        if clockwise_distance(current, target) <= clockwise_distance(current, head):
            return head, hops, 0

        target_distance = clockwise_distance(current, target)
        next_hop = None
        for candidate in reversed(node["fingers"]):
            if (
                candidate in nodes_by_did
                and clockwise_distance(current, candidate) < target_distance
            ):
                next_hop = candidate
                break

        if next_hop is None or next_hop == current:
            return None, hops, 1
        current = next_hop

    return None, max_hops, 1


def summarize_dht(
    nodes: list[dict[str, Any]],
    lookup_target_bits: int,
    max_lookup_hops: int,
    successor_capacity: int,
) -> dict[str, Any]:
    all_dids = [node["did"] for node in nodes]
    all_did_set = set(all_dids)
    nodes_by_did = {node["did"]: node for node in nodes}

    successor_matches = 0
    successor_first_matches = 0
    predecessor_matches = 0
    topology_matches = 0
    full_matches = 0
    finger_slot_matches: list[int] = []
    known_finger_slots: list[int] = []
    connected_peers: list[int] = []
    new_peers: list[int] = []
    peer_state_counts: Counter[str] = Counter()
    unknown_peer_dids: set[str] = set()

    finger_table_size = max((len(node["fingers"]) for node in nodes), default=0)
    compared_finger_slots = min(finger_table_size, RING_BITS)

    for node in nodes:
        expected_succ = expected_successors(
            node["did"], all_dids, min(successor_capacity, max(len(all_dids) - 1, 0))
        )
        expected_pred = expected_predecessor(node["did"], all_dids)
        actual_succ = node["successors"][: len(expected_succ)]
        succ_ok = actual_succ == expected_succ
        first_succ_ok = (
            bool(expected_succ)
            and bool(actual_succ)
            and actual_succ[0] == expected_succ[0]
        )
        pred_ok = node["predecessor"] == expected_pred

        successor_matches += int(succ_ok)
        successor_first_matches += int(first_succ_ok)
        predecessor_matches += int(pred_ok)
        topology_matches += int(succ_ok and pred_ok)

        matches = 0
        known = 0
        for bit in range(compared_finger_slots):
            actual = node["fingers"][bit] if bit < len(node["fingers"]) else None
            expected = expected_finger(node["did"], all_dids, bit)
            known += int(actual is not None)
            matches += int(actual == expected)
        finger_slot_matches.append(matches)
        known_finger_slots.append(known)
        full_matches += int(succ_ok and pred_ok and matches == compared_finger_slots)

        connected = 0
        new = 0
        for peer in node["peers"]:
            state = peer.get("state", "Unknown")
            peer_state_counts[state] += 1
            connected += int(state == "Connected")
            new += int(state == "New")
            if peer.get("did") not in all_did_set:
                unknown_peer_dids.add(peer.get("did", ""))
        connected_peers.append(connected)
        new_peers.append(new)

    lookups = 0
    resolved = 0
    correct = 0
    failed = 0
    timeouts = 0
    hop_sum = 0
    max_hops = 0
    hop_buckets: Counter[int] = Counter()
    target_bits = min(lookup_target_bits, RING_BITS)

    for origin in all_dids:
        for bit in range(target_bits):
            target_int = (did_int(origin) + (1 << bit)) % RING_SIZE
            target = hex(target_int)
            expected = expected_owner(target, all_dids)
            actual, hops, timeout_count = modeled_lookup(
                origin, target, nodes_by_did, max_lookup_hops
            )
            lookups += 1
            timeouts += timeout_count
            if actual is None:
                failed += 1
                continue
            resolved += 1
            hop_sum += hops
            max_hops = max(max_hops, hops)
            hop_buckets[hops] += 1
            if actual == expected:
                correct += 1
            else:
                failed += 1

    def avg(values: list[int]) -> float:
        return float(statistics.mean(values)) if values else 0.0

    return {
        "report": "docker_cluster_dht",
        "lookup_model": "status_snapshot_route",
        "node_count": len(nodes),
        "finger_table_size": finger_table_size,
        "lookup_target_bits": target_bits,
        "successor_capacity": successor_capacity,
        "topology_matches": topology_matches,
        "successor_matches": successor_matches,
        "successor_first_matches": successor_first_matches,
        "predecessor_matches": predecessor_matches,
        "full_matches": full_matches,
        "finger_slots_match_min": min(finger_slot_matches, default=0),
        "finger_slots_match_avg": avg(finger_slot_matches),
        "finger_slots_match_max": max(finger_slot_matches, default=0),
        "known_finger_slots_min": min(known_finger_slots, default=0),
        "known_finger_slots_avg": avg(known_finger_slots),
        "known_finger_slots_max": max(known_finger_slots, default=0),
        "connected_directed_edges": sum(connected_peers),
        "connected_peers_min": min(connected_peers, default=0),
        "connected_peers_avg": avg(connected_peers),
        "connected_peers_max": max(connected_peers, default=0),
        "new_peer_entries_total": sum(new_peers),
        "peer_state_counts": dict(sorted(peer_state_counts.items())),
        "unknown_peer_dids_count": len(unknown_peer_dids),
        "lookups": {
            "total": lookups,
            "resolved": resolved,
            "correct": correct,
            "failed": failed,
            "success_rate": resolved / lookups if lookups else 0.0,
            "correctness_rate": correct / lookups if lookups else 0.0,
            "avg_hops": hop_sum / resolved if resolved else 0.0,
            "max_hops": max_hops,
            "timeouts": timeouts,
            "mean_lookup_timeouts": timeouts / lookups if lookups else 0.0,
            "lookup_failures_per_10k": failed * 10_000 / lookups if lookups else 0.0,
            "hop_buckets": dict(sorted(hop_buckets.items())),
        },
    }


def measurements_by_did(measurements: dict[str, Any]) -> dict[str, dict[str, int]]:
    return {
        item["did"]: item.get("counters", {})
        for item in measurements.get("measurements", [])
    }


def counter_delta(
    before: dict[str, dict[str, int]],
    after: dict[str, dict[str, int]],
    did: str,
    counter: str,
) -> int | None:
    if did not in before and did not in after:
        return None
    return int(after.get(did, {}).get(counter, 0)) - int(
        before.get(did, {}).get(counter, 0)
    )


def choose_connected_pair(
    nodes: list[dict[str, Any]],
) -> tuple[dict[str, Any], dict[str, Any]] | None:
    nodes_by_did = {node["did"]: node for node in nodes}
    for source in nodes:
        for peer in source["peers"]:
            if peer.get("state") == "Connected" and peer.get("did") in nodes_by_did:
                return source, nodes_by_did[peer["did"]]
    return None


def transport_report(
    container: str,
    nodes: list[dict[str, Any]],
    payload_bytes: int,
    messages: int,
    settle_seconds: float,
    flush_timeout_ms: int,
    source_index: int | None,
    destination_index: int | None,
) -> dict[str, Any]:
    pair_selection = "auto_connected_pair"
    if source_index is None and destination_index is None:
        pair = choose_connected_pair(nodes)
    elif source_index is None or destination_index is None:
        return {
            "report": "docker_cluster_transport",
            "enabled": True,
            "error": "throughput source and destination indexes must be specified together",
        }
    elif source_index < 0 or source_index >= len(nodes):
        return {
            "report": "docker_cluster_transport",
            "enabled": True,
            "error": f"throughput source index {source_index} is out of range",
        }
    elif destination_index < 0 or destination_index >= len(nodes):
        return {
            "report": "docker_cluster_transport",
            "enabled": True,
            "error": f"throughput destination index {destination_index} is out of range",
        }
    elif source_index == destination_index:
        return {
            "report": "docker_cluster_transport",
            "enabled": True,
            "error": "throughput source and destination indexes must differ",
        }
    else:
        pair = (nodes[source_index], nodes[destination_index])
        pair_selection = "configured_index_pair"

    if pair is None:
        return {
            "report": "docker_cluster_transport",
            "enabled": True,
            "error": "no connected in-cluster peer pair found",
        }

    source, destination = pair
    errors: list[str] = []

    try:
        source_before = measurements_by_did(
            rpc(container, source["port"], "listPeerMeasurements", {})
        )
        dest_before = measurements_by_did(
            rpc(container, destination["port"], "listPeerMeasurements", {})
        )
    except Exception as exc:  # noqa: BLE001
        source_before = {}
        dest_before = {}
        errors.append(str(exc))

    started = time.monotonic()
    benchmark: dict[str, Any] = {}
    benchmark_timeout = max(60, int(flush_timeout_ms / 1000) + 30)
    try:
        benchmark = rpc(
            container,
            source["port"],
            "transportBenchmark",
            {
                "destination_did": destination["did"],
                "namespace": "bench",
                "payload_bytes": payload_bytes,
                "messages": messages,
                "flush_timeout_ms": flush_timeout_ms,
            },
            timeout=benchmark_timeout,
        )
    except Exception as exc:  # noqa: BLE001
        errors.append(str(exc))
    control_elapsed = time.monotonic() - started

    if settle_seconds > 0:
        time.sleep(settle_seconds)

    try:
        source_after = measurements_by_did(
            rpc(container, source["port"], "listPeerMeasurements", {})
        )
        dest_after = measurements_by_did(
            rpc(container, destination["port"], "listPeerMeasurements", {})
        )
    except Exception as exc:  # noqa: BLE001
        source_after = {}
        dest_after = {}
        errors.append(str(exc))

    admitted = int(benchmark.get("admitted_messages", 0))
    flushed = int(benchmark.get("flushed_messages", 0))
    total_payload_bytes = int(
        benchmark.get("total_payload_bytes", payload_bytes * admitted)
    )
    admission_elapsed_ms = float(benchmark.get("admission_elapsed_ms", 0.0))
    flush_elapsed_ms = float(benchmark.get("flush_elapsed_ms", 0.0))
    admission_mbps = float(benchmark.get("admission_mbps", 0.0))
    flush_mbps = float(benchmark.get("flush_mbps", 0.0))
    return {
        "report": "docker_cluster_transport",
        "enabled": True,
        "mode": "node_internal_burst",
        "pair_selection": pair_selection,
        "source_index": source["index"],
        "source_did": source["did"],
        "destination_index": destination["index"],
        "destination_did": destination["did"],
        "payload_bytes": payload_bytes,
        "messages": messages,
        "admitted_messages": admitted,
        "flushed_messages": flushed,
        "sent_messages": flushed,
        "total_payload_bytes": total_payload_bytes,
        "control_elapsed_ms": control_elapsed * 1000.0,
        "admission_elapsed_ms": admission_elapsed_ms,
        "flush_elapsed_ms": flush_elapsed_ms,
        "admission_mbps": admission_mbps,
        "flush_mbps": flush_mbps,
        "send_elapsed_ms": flush_elapsed_ms,
        "send_mbps": flush_mbps,
        "flush_timed_out": bool(benchmark.get("flush_timed_out", False)),
        "source_sent_delta": counter_delta(
            source_before, source_after, destination["did"], "sent"
        ),
        "source_failed_to_send_delta": counter_delta(
            source_before, source_after, destination["did"], "failed_to_send"
        ),
        "destination_received_delta": counter_delta(
            dest_before, dest_after, source["did"], "received"
        ),
        "destination_failed_to_receive_delta": counter_delta(
            dest_before, dest_after, source["did"], "failed_to_receive"
        ),
        "errors": errors,
    }


def sample(args: argparse.Namespace, sample_index: int) -> dict[str, Any]:
    nodes = collect_nodes(
        args.container,
        args.nodes,
        args.base_internal_port,
        args.finger_table_size,
    )
    dht = summarize_dht(
        nodes,
        args.lookup_target_bits,
        args.max_lookup_hops,
        args.successor_capacity,
    )
    dht.update(
        {
            "container": args.container,
            "sample_index": sample_index,
            "sample_time_ms": int(time.time() * 1000),
            "base_internal_port": args.base_internal_port,
            "configured_finger_table_size": args.finger_table_size,
            "cluster_topology": args.cluster_topology,
            "stabilize_interval_seconds": args.stabilize_interval_seconds,
            "docker_image": args.docker_image,
        }
    )
    if args.throughput_messages > 0:
        dht["transport"] = transport_report(
            args.container,
            nodes,
            args.throughput_payload_bytes,
            args.throughput_messages,
            args.throughput_settle_seconds,
            args.throughput_flush_timeout_ms,
            args.throughput_source_index,
            args.throughput_destination_index,
        )
    return dht


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--container",
        default=os.environ.get("RINGS_DHT_BENCH_CONTAINER", "rings-cluster-16-bench"),
    )
    parser.add_argument("--nodes", type=int, default=env_int("RINGS_NODE_COUNT", 16))
    parser.add_argument(
        "--base-internal-port",
        type=int,
        default=env_int("RINGS_BASE_INTERNAL_PORT", 50000),
    )
    parser.add_argument(
        "--cluster-topology",
        default=os.environ.get("RINGS_CONNECT_TOPOLOGY", "unknown"),
        help="Topology mode used when the Docker cluster was started.",
    )
    parser.add_argument(
        "--stabilize-interval-seconds",
        type=float,
        default=float(os.environ.get("RINGS_STABILIZE_INTERVAL", "0")),
        help="Stabilization interval used when the Docker cluster was started.",
    )
    parser.add_argument(
        "--docker-image",
        default=os.environ.get("RINGS_DHT_BENCH_DOCKER_IMAGE", "unknown"),
        help="Docker image tag or ID used for the cluster.",
    )
    parser.add_argument(
        "--finger-table-size",
        type=int,
        default=env_int("RINGS_DHT_FINGER_TABLE_SIZE", 0),
        help="Configured finger slots. 0 infers from /status.",
    )
    parser.add_argument(
        "--lookup-target-bits",
        type=int,
        default=env_int("RINGS_DHT_BENCH_LOOKUP_TARGET_BITS", RING_BITS),
    )
    parser.add_argument(
        "--max-lookup-hops",
        type=int,
        default=env_int("RINGS_DHT_BENCH_MAX_LOOKUP_HOPS", 64),
    )
    parser.add_argument(
        "--successor-capacity",
        type=int,
        default=env_int("RINGS_DHT_BENCH_SUCCESSOR_CAPACITY", 3),
    )
    parser.add_argument(
        "--samples",
        type=int,
        default=env_int("RINGS_DHT_BENCH_SAMPLES", 1),
    )
    parser.add_argument(
        "--sample-interval-seconds",
        type=float,
        default=float(os.environ.get("RINGS_DHT_BENCH_SAMPLE_INTERVAL_SECONDS", "30")),
    )
    parser.add_argument(
        "--throughput-messages",
        type=int,
        default=env_int("RINGS_DHT_BENCH_THROUGHPUT_MESSAGES", 0),
    )
    parser.add_argument(
        "--throughput-payload-bytes",
        type=int,
        default=env_int("RINGS_DHT_BENCH_THROUGHPUT_PAYLOAD_BYTES", 16 * 1024),
    )
    parser.add_argument(
        "--throughput-settle-seconds",
        type=float,
        default=float(os.environ.get("RINGS_DHT_BENCH_THROUGHPUT_SETTLE_SECONDS", "2")),
    )
    parser.add_argument(
        "--throughput-flush-timeout-ms",
        type=int,
        default=env_int("RINGS_DHT_BENCH_THROUGHPUT_FLUSH_TIMEOUT_MS", 30_000),
    )
    parser.add_argument(
        "--throughput-source-index",
        type=int,
        default=env_int("RINGS_DHT_BENCH_THROUGHPUT_SOURCE_INDEX", -1),
        help="Source node index for throughput sampling. -1 chooses an in-cluster connected pair.",
    )
    parser.add_argument(
        "--throughput-destination-index",
        type=int,
        default=env_int("RINGS_DHT_BENCH_THROUGHPUT_DESTINATION_INDEX", -1),
        help="Destination node index for throughput sampling. -1 chooses an in-cluster connected pair.",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.nodes <= 1:
        raise SystemExit("--nodes must be greater than 1")
    if args.samples <= 0:
        raise SystemExit("--samples must be greater than 0")
    if args.finger_table_size == 0:
        args.finger_table_size = None
    if args.throughput_source_index < 0:
        args.throughput_source_index = None
    if args.throughput_destination_index < 0:
        args.throughput_destination_index = None

    for index in range(args.samples):
        if index > 0:
            time.sleep(args.sample_interval_seconds)
        print(json.dumps(sample(args, index), sort_keys=True), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
