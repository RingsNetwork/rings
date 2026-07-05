#!/usr/bin/env python3
"""Generate Chord-paper-aligned simulator artifacts as JSONL.

This script is intentionally independent from the real Rings DHT runtime. It
models the experiments reported by the original Chord paper so the benchmark
artifacts can be mapped one-to-one to the paper's tables and data figures.
"""

from __future__ import annotations

import argparse
import bisect
import heapq
import json
import math
import random
import statistics
import time
from dataclasses import dataclass
from typing import Iterable


RING_BITS = 64
RING_SIZE = 1 << RING_BITS
FINGER_TABLE_SIZE = RING_BITS
LOOKUPS_PER_TABLE = 10_000
FIG10_LOOKUPS_PER_NODE = 32


def splitmix64(value: int) -> int:
    value = (value + 0x9E3779B97F4A7C15) & 0xFFFFFFFFFFFFFFFF
    value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & 0xFFFFFFFFFFFFFFFF
    value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & 0xFFFFFFFFFFFFFFFF
    return (value ^ (value >> 31)) & 0xFFFFFFFFFFFFFFFF


def deterministic_ids(count: int, seed: int) -> list[int]:
    ids: set[int] = set()
    cursor = seed
    while len(ids) < count:
        cursor += 1
        value = splitmix64(cursor)
        if value != 0:
            ids.add(value)
    return sorted(ids)


def percentile(values: list[float] | list[int], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = math.ceil((pct / 100.0) * len(ordered)) - 1
    index = max(0, min(index, len(ordered) - 1))
    return float(ordered[index])


def ratio(numerator: int, denominator: int) -> float:
    if denominator == 0:
        return 0.0
    return numerator / denominator


def clockwise_distance(start: int, end: int) -> int:
    if end >= start:
        return end - start
    return RING_SIZE - (start - end)


def in_interval(start: int, value: int, end: int) -> bool:
    distance = clockwise_distance(start, value)
    return 0 < distance < clockwise_distance(start, end)


def successor_index(ids: list[int], target: int) -> int:
    return bisect.bisect_left(ids, target) % len(ids)


def successor_index_from_set(ids: list[int], active: set[int], target: int) -> int:
    position = successor_index(ids, target)
    for offset in range(len(ids)):
        candidate = (position + offset) % len(ids)
        if candidate in active:
            return candidate
    raise RuntimeError("active node set is empty")


def selected_indices(count: int, pct: int, seed: int) -> set[int]:
    total = min(count - 1, (count * pct + 50) // 100)
    rng = random.Random(seed)
    return set(rng.sample(range(count), total))


def stable_successors(count: int, index: int, capacity: int) -> list[int]:
    limit = min(count, capacity + 1)
    return [(index + offset) % count for offset in range(1, limit)]


def stable_fingers(ids: list[int], index: int) -> list[int]:
    origin = ids[index]
    return [
        successor_index(ids, (origin + (1 << bit)) % RING_SIZE)
        for bit in range(FINGER_TABLE_SIZE)
    ]


@dataclass
class RouteState:
    ids: list[int]
    successors: dict[int, list[int]]
    fingers: dict[int, list[int]]


@dataclass
class LookupResult:
    resolved: bool
    correct: bool
    hops: int
    timeouts: int

    @property
    def contacts(self) -> int:
        return self.hops + self.timeouts


def build_stable_state(ids: list[int], successor_capacity: int) -> RouteState:
    count = len(ids)
    return RouteState(
        ids=ids,
        successors={
            index: stable_successors(count, index, successor_capacity)
            for index in range(count)
        },
        fingers={index: stable_fingers(ids, index) for index in range(count)},
    )


def route_lookup(
    state: RouteState,
    active: set[int],
    origin: int,
    target: int,
    expected: int,
    update_dead_pointers: bool = False,
) -> LookupResult:
    current = origin
    visited: set[int] = set()
    timeouts = 0

    for hops in range(1, 128):
        if current not in active or current in visited:
            return LookupResult(False, False, hops, timeouts)
        visited.add(current)

        current_id = state.ids[current]
        target_distance = clockwise_distance(current_id, target)
        missed_successors: list[int] = []
        for successor in state.successors.get(current, []):
            successor_distance = clockwise_distance(current_id, state.ids[successor])
            if target_distance <= successor_distance:
                if successor in active:
                    return LookupResult(True, successor == expected, hops, timeouts)
                timeouts += 1
                missed_successors.append(successor)
        if update_dead_pointers and missed_successors:
            remove_dead_pointers(state, current, set(missed_successors))

        candidates: list[tuple[int, int]] = []
        for candidate in reversed(state.fingers.get(current, [])):
            distance = clockwise_distance(current_id, state.ids[candidate])
            if 0 < distance < target_distance:
                candidates.append((distance, candidate))
        for candidate in reversed(state.successors.get(current, [])):
            distance = clockwise_distance(current_id, state.ids[candidate])
            if 0 < distance < target_distance:
                candidates.append((distance, candidate))
        candidates.sort(key=lambda item: (-item[0], item[1]))

        forwarded = False
        dead_candidates: set[int] = set()
        source = current
        seen: set[int] = set()
        for _, candidate in candidates:
            if candidate in seen:
                continue
            seen.add(candidate)
            if candidate not in active:
                timeouts += 1
                dead_candidates.add(candidate)
                continue
            current = candidate
            forwarded = True
            break
        if update_dead_pointers and dead_candidates:
            remove_dead_pointers(state, source, dead_candidates)
        if not forwarded:
            return LookupResult(False, False, hops, timeouts)

    return LookupResult(False, False, 128, timeouts)


def remove_dead_pointers(state: RouteState, index: int, dead: set[int]) -> None:
    state.successors[index] = [
        successor for successor in state.successors.get(index, []) if successor not in dead
    ]
    state.fingers[index] = [
        finger if finger not in dead else index for finger in state.fingers.get(index, [])
    ]


def summarize_lookup_results(results: list[LookupResult]) -> dict[str, object]:
    contacts = [result.contacts for result in results]
    hops = [result.hops for result in results]
    timeouts = [result.timeouts for result in results]
    failed = sum(1 for result in results if not result.correct)
    return {
        "lookups": len(results),
        "resolved": sum(1 for result in results if result.resolved),
        "correct": sum(1 for result in results if result.correct),
        "failed": failed,
        "success_rate": ratio(sum(1 for result in results if result.resolved), len(results)),
        "correctness_rate": ratio(sum(1 for result in results if result.correct), len(results)),
        "avg_path_length": statistics.fmean(hops) if hops else 0.0,
        "path_length_p1": percentile(hops, 1),
        "path_length_p10": percentile(hops, 10),
        "path_length_p90": percentile(hops, 90),
        "path_length_p99": percentile(hops, 99),
        "avg_live_hops": statistics.fmean(hops) if hops else 0.0,
        "avg_contacts_including_timeouts": statistics.fmean(contacts) if contacts else 0.0,
        "contacts_including_timeouts_p1": percentile(contacts, 1),
        "contacts_including_timeouts_p10": percentile(contacts, 10),
        "contacts_including_timeouts_p90": percentile(contacts, 90),
        "contacts_including_timeouts_p99": percentile(contacts, 99),
        "avg_timeouts": statistics.fmean(timeouts) if timeouts else 0.0,
        "timeouts_p1": percentile(timeouts, 1),
        "timeouts_p10": percentile(timeouts, 10),
        "timeouts_p90": percentile(timeouts, 90),
        "timeouts_p99": percentile(timeouts, 99),
        "lookup_failures_per_10k": ratio(failed * 10_000, len(results)),
    }


def paper_table_ii() -> Iterable[dict[str, object]]:
    node_count = 1000
    successor_capacity = 20
    ids = deterministic_ids(node_count, 0x2000)
    state = build_stable_state(ids, successor_capacity)
    rng = random.Random(0x2200)

    for failed_pct in [0, 10, 20, 30, 40, 50]:
        failed = selected_indices(node_count, failed_pct, 0x2300 + failed_pct)
        active = set(range(node_count)) - failed
        active_origins = sorted(active)
        results: list[LookupResult] = []
        for lookup in range(LOOKUPS_PER_TABLE):
            origin = active_origins[lookup % len(active_origins)]
            target = rng.randrange(RING_SIZE)
            expected = successor_index_from_set(ids, active, target)
            results.append(route_lookup(state, active, origin, target, expected))
        yield {
            "report": "chord_paper_sim",
            "paper_item": "table_ii",
            "scenario": "simultaneous_node_failures",
            "node_count": node_count,
            "successor_list_size": successor_capacity,
            "failed_node_pct": failed_pct,
            "failed_nodes": len(failed),
            "metrics": summarize_lookup_results(results),
            "paper_reference": {
                "network_nodes": 1000,
                "successor_list_size": "20 = 2 log2 N",
                "lookups": 10_000,
            },
        }


class DynamicChord:
    def __init__(self, node_count: int, successor_capacity: int, seed: int) -> None:
        self.successor_capacity = successor_capacity
        self.initial_node_count = node_count
        self.capacity = node_count + 6000
        pairs = sorted((splitmix64(seed + ordinal + 1), ordinal) for ordinal in range(self.capacity))
        self.ids = [node_id for node_id, _ in pairs]
        self.index_by_ordinal = {ordinal: index for index, (_, ordinal) in enumerate(pairs)}
        self.next_ordinal = node_count
        self.active: set[int] = {
            self.index_by_ordinal[ordinal] for ordinal in range(node_count)
        }
        self.active_ring = sorted(self.active)
        self.state = RouteState(self.ids, {}, {})
        self.finger_cursor: dict[int, int] = {index: 0 for index in self.active}
        for index in self.active_ring:
            self.state.successors[index] = self.correct_successors(index)
            self.state.fingers[index] = [
                self.correct_finger(index, bit) for bit in range(FINGER_TABLE_SIZE)
            ]

    def active_sorted(self) -> list[int]:
        return self.active_ring

    def add_node(self) -> int:
        if self.next_ordinal >= self.capacity:
            raise RuntimeError("dynamic Chord simulator exhausted pre-generated node IDs")
        index = self.index_by_ordinal[self.next_ordinal]
        self.next_ordinal += 1
        self.active.add(index)
        bisect.insort(self.active_ring, index)
        self.state.successors[index] = self.correct_successors(index)
        self.state.fingers[index] = [
            self.correct_finger(index, bit) for bit in range(FINGER_TABLE_SIZE)
        ]
        self.finger_cursor[index] = 0
        return index

    def remove_node(self, rng: random.Random) -> int:
        departed = rng.choice(self.active_ring)
        predecessor = self.predecessor(departed)
        self.active.remove(departed)
        self.active_ring.pop(bisect.bisect_left(self.active_ring, departed))
        if predecessor is not None:
            self.state.successors[predecessor] = self.correct_successors(predecessor)
        return departed

    def stabilize(self, index: int) -> None:
        if index not in self.active:
            return
        self.state.successors[index] = self.correct_successors(index)
        bit = self.finger_cursor.get(index, 0) % FINGER_TABLE_SIZE
        fingers = self.state.fingers.setdefault(index, [index] * FINGER_TABLE_SIZE)
        fingers[bit] = self.correct_finger(index, bit)
        self.finger_cursor[index] = bit + 1

    def correct_successors(self, index: int) -> list[int]:
        position = bisect.bisect_left(self.active_ring, index)
        if position >= len(self.active_ring) or self.active_ring[position] != index:
            return []
        limit = min(len(self.active_ring), self.successor_capacity + 1)
        return [
            self.active_ring[(position + offset) % len(self.active_ring)]
            for offset in range(1, limit)
        ]

    def correct_finger(self, index: int, bit: int) -> int:
        target = (self.ids[index] + (1 << bit)) % RING_SIZE
        return self.active_successor_index(target)

    def active_successor_index(self, target: int) -> int:
        position = successor_index(self.ids, target)
        active_position = bisect.bisect_left(self.active_ring, position)
        if active_position < len(self.active_ring):
            return self.active_ring[active_position]
        return self.active_ring[0]

    def predecessor(self, index: int) -> int | None:
        if index not in self.active or len(self.active_ring) <= 1:
            return None
        position = bisect.bisect_left(self.active_ring, index)
        return self.active_ring[(position - 1) % len(self.active_ring)]

    def lookup(self, origin: int, target: int) -> LookupResult:
        expected = self.active_successor_index(target)
        return route_lookup(
            self.state,
            self.active,
            origin,
            target,
            expected,
            update_dead_pointers=True,
        )


def paper_table_iii() -> Iterable[dict[str, object]]:
    node_count = 1000
    successor_capacity = 20
    rates = [0.05, 0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40]
    for rate in rates:
        rng = random.Random(0x3000 + int(rate * 1000))
        chord = DynamicChord(node_count, successor_capacity, 0x3100 + int(rate * 1000))
        events: list[tuple[float, str, int | None]] = []
        now = 0.0
        heapq.heappush(events, (rng.expovariate(1.0), "lookup", None))
        heapq.heappush(events, (rng.expovariate(rate), "churn", None))
        for index in sorted(chord.active):
            heapq.heappush(events, (rng.uniform(15.0, 45.0), "stabilize", index))

        results: list[LookupResult] = []
        while len(results) < LOOKUPS_PER_TABLE:
            now, event, index = heapq.heappop(events)
            if event == "lookup":
                origin = rng.choice(sorted(chord.active))
                target = rng.randrange(RING_SIZE)
                results.append(chord.lookup(origin, target))
                heapq.heappush(events, (now + rng.expovariate(1.0), "lookup", None))
            elif event == "churn":
                if len(chord.active) > 1:
                    chord.remove_node(rng)
                joined = chord.add_node()
                heapq.heappush(events, (now + rng.uniform(15.0, 45.0), "stabilize", joined))
                heapq.heappush(events, (now + rng.expovariate(rate), "churn", None))
            elif event == "stabilize" and index is not None:
                chord.stabilize(index)
                if index in chord.active:
                    heapq.heappush(events, (now + rng.uniform(15.0, 45.0), "stabilize", index))

        yield {
            "report": "chord_paper_sim",
            "paper_item": "table_iii",
            "scenario": "lookups_during_stabilization",
            "node_count_initial": node_count,
            "node_count_final": len(chord.active),
            "successor_list_size": successor_capacity,
            "join_leave_rate_per_second": rate,
            "join_leave_rate_per_stabilization_period": rate * 30.0,
            "metrics": summarize_lookup_results(results),
            "paper_reference": {
                "network_nodes": "roughly 1000",
                "successor_list_size": "20 = 2 log2 N",
                "stabilize_interval_seconds": "[15, 45]",
                "lookup_rate_per_second": 1,
            },
        }


def paper_fig_8() -> Iterable[dict[str, object]]:
    node_count = 10_000
    key_counts = list(range(100_000, 1_000_001, 100_000))
    seeds = range(20)
    for key_count in key_counts:
        all_counts: list[int] = []
        for seed in seeds:
            ids = deterministic_ids(node_count, 0x4000 + seed)
            all_counts.extend(expected_key_counts(ids, key_count))
        row: dict[str, object] = {
            "report": "chord_paper_sim",
            "paper_item": "fig_8a",
            "scenario": "load_balance_consistent_hashing",
            "node_count": node_count,
            "key_count": key_count,
            "seeds": 20,
            "mean_keys_per_node": statistics.fmean(all_counts),
            "keys_per_node_p1": percentile(all_counts, 1),
            "keys_per_node_p99": percentile(all_counts, 99),
            "max_keys_per_node": max(all_counts),
            "paper_reference": {
                "node_count": "10^4",
                "key_count_range": "10^5..10^6",
                "seeds": 20,
            },
        }
        yield row
        if key_count == 500_000:
            pdf_row = dict(row)
            pdf_row["paper_item"] = "fig_8b"
            pdf_row["pdf"] = histogram(all_counts, 10)
            yield pdf_row


def paper_fig_9() -> Iterable[dict[str, object]]:
    real_nodes = 10_000
    key_count = 1_000_000
    for virtual_nodes_per_real in [1, 2, 5, 10, 20]:
        ids_by_real: list[tuple[int, int]] = []
        for real in range(real_nodes):
            for vnode in range(virtual_nodes_per_real):
                ids_by_real.append((splitmix64(0x5000 + real * 1009 + vnode), real))
        ids_by_real.sort()
        virtual_ids = [node_id for node_id, _ in ids_by_real]
        virtual_counts = expected_key_counts(virtual_ids, key_count)
        real_counts = [0 for _ in range(real_nodes)]
        for (_, real), count in zip(ids_by_real, virtual_counts):
            real_counts[real] += count
        yield {
            "report": "chord_paper_sim",
            "paper_item": "fig_9",
            "scenario": "load_balance_virtual_nodes",
            "runtime_support": "simulator_only",
            "real_node_count": real_nodes,
            "virtual_nodes_per_real": virtual_nodes_per_real,
            "virtual_node_count": len(virtual_ids),
            "key_count": key_count,
            "mean_keys_per_real_node": statistics.fmean(real_counts),
            "keys_per_real_node_p1": percentile(real_counts, 1),
            "keys_per_real_node_p99": percentile(real_counts, 99),
            "max_keys_per_real_node": max(real_counts),
            "paper_reference": {
                "real_node_count": "10^4",
                "key_count": "10^6",
                "virtual_nodes_per_real": [1, 2, 5, 10, 20],
            },
        }


def expected_key_counts(ids: list[int], key_count: int) -> list[int]:
    lengths = []
    previous = ids[-1]
    for node_id in ids:
        lengths.append(clockwise_distance(previous, node_id))
        previous = node_id
    raw = [(key_count * length) / RING_SIZE for length in lengths]
    counts = [math.floor(value) for value in raw]
    remainder = key_count - sum(counts)
    if remainder > 0:
        fractions = sorted(
            ((value - math.floor(value), index) for index, value in enumerate(raw)),
            reverse=True,
        )
        for _, index in fractions[:remainder]:
            counts[index] += 1
    return counts


def histogram(values: list[int], width: int) -> list[dict[str, int]]:
    buckets: dict[int, int] = {}
    for value in values:
        bucket = (value // width) * width
        buckets[bucket] = buckets.get(bucket, 0) + 1
    return [
        {"start": start, "end": start + width - 1, "count": count}
        for start, count in sorted(buckets.items())
    ]


def paper_fig_10() -> Iterable[dict[str, object]]:
    for k in range(3, 15):
        node_count = 1 << k
        ids = deterministic_ids(node_count, 0x6000 + k)
        state = build_stable_state(ids, successor_capacity=1)
        active = set(range(node_count))
        rng = random.Random(0x6100 + k)
        lookups = min(node_count * FIG10_LOOKUPS_PER_NODE, 20_000)
        results: list[LookupResult] = []
        for lookup in range(lookups):
            origin = lookup % node_count
            target = rng.randrange(RING_SIZE)
            expected = successor_index(ids, target)
            results.append(route_lookup(state, active, origin, target, expected))
        metrics = summarize_lookup_results(results)
        row: dict[str, object] = {
            "report": "chord_paper_sim",
            "paper_item": "fig_10a",
            "scenario": "path_length_scaling",
            "k": k,
            "node_count": node_count,
            "stored_keys": 100 * node_count,
            "lookups": lookups,
            "metrics": metrics,
            "paper_reference": {
                "node_count": "2^k, k=3..14",
                "stored_keys": "100 * 2^k",
            },
        }
        yield row
        if k == 12:
            pdf_row = dict(row)
            pdf_row["paper_item"] = "fig_10b"
            pdf_row["path_length_pdf"] = histogram(
                [result.contacts for result in results],
                1,
            )
            yield pdf_row


def paper_table_iv() -> Iterable[dict[str, object]]:
    node_count = 1 << 16
    ids = deterministic_ids(node_count, 0x7000)
    rng = random.Random(0x7100)
    coordinate_cache = {
        index: (
            rng.random(),
            rng.random(),
            rng.random(),
        )
        for index in range(node_count)
    }
    transit_cache = {
        index: (rng.randrange(20), rng.randrange(250))
        for index in range(node_count)
    }
    lookup_pairs = [
        (rng.randrange(node_count), rng.randrange(RING_SIZE))
        for _ in range(LOOKUPS_PER_TABLE)
    ]

    for successors_per_finger in [1, 2, 4, 8, 16]:
        for topology in ["3d_space", "transit_stub"]:
            for style in ["iterative", "recursive"]:
                stretches = []
                for origin, target in lookup_pairs:
                    responsible = successor_index(ids, target)
                    path = proximity_path(
                        ids,
                        origin,
                        target,
                        successors_per_finger,
                        topology,
                        style,
                        coordinate_cache,
                        transit_cache,
                    )
                    actual = lookup_latency(
                        path,
                        origin,
                        responsible,
                        topology,
                        style,
                        coordinate_cache,
                        transit_cache,
                    )
                    optimal = max(
                        0.000001,
                        2.0
                        * network_latency(
                            origin,
                            responsible,
                            topology,
                            coordinate_cache,
                            transit_cache,
                        ),
                    )
                    stretches.append(actual / optimal)
                yield {
                    "report": "chord_paper_sim",
                    "paper_item": "table_iv",
                    "scenario": "lookup_latency_stretch",
                    "node_count": node_count,
                    "fingers_successors": successors_per_finger,
                    "lookup_style": style,
                    "network_model": topology,
                    "lookups": LOOKUPS_PER_TABLE,
                    "stretch_median": statistics.median(stretches),
                    "stretch_p10": percentile(stretches, 10),
                    "stretch_p90": percentile(stretches, 90),
                    "paper_reference": {
                        "node_count": "2^16",
                        "fingers_successors": [1, 2, 4, 8, 16],
                        "network_models": ["3-d Euclidean space", "transit-stub"],
                    },
                }


def proximity_path(
    ids: list[int],
    origin: int,
    target: int,
    successors_per_finger: int,
    topology: str,
    style: str,
    coordinate_cache: dict[int, tuple[float, float, float]],
    transit_cache: dict[int, tuple[int, int]],
) -> list[int]:
    current = origin
    path = [origin]
    visited = {origin}
    for _ in range(128):
        current_id = ids[current]
        target_distance = clockwise_distance(current_id, target)
        immediate_successor = (current + 1) % len(ids)
        if target_distance <= clockwise_distance(current_id, ids[immediate_successor]):
            path.append(immediate_successor)
            return path
        finger = closest_preceding_finger(ids, current, target)
        candidates = []
        for offset in range(successors_per_finger + 1):
            candidate = (finger + offset) % len(ids)
            if in_interval(current_id, ids[candidate], target):
                candidates.append(candidate)
        if not candidates:
            candidates.append(finger)
        anchor = origin if style == "iterative" else current
        next_hop = min(
            candidates,
            key=lambda candidate: network_latency(
                anchor,
                candidate,
                topology,
                coordinate_cache,
                transit_cache,
            ),
        )
        if next_hop in visited:
            return path
        path.append(next_hop)
        visited.add(next_hop)
        current = next_hop
    return path


def closest_preceding_finger(ids: list[int], current: int, target: int) -> int:
    current_id = ids[current]
    target_distance = clockwise_distance(current_id, target)
    best = current
    best_distance = 0
    for bit in reversed(range(FINGER_TABLE_SIZE)):
        candidate = successor_index(ids, (current_id + (1 << bit)) % RING_SIZE)
        distance = clockwise_distance(current_id, ids[candidate])
        if 0 < distance < target_distance and distance > best_distance:
            best = candidate
            best_distance = distance
    return best


def lookup_latency(
    path: list[int],
    origin: int,
    responsible: int,
    topology: str,
    style: str,
    coordinate_cache: dict[int, tuple[float, float, float]],
    transit_cache: dict[int, tuple[int, int]],
) -> float:
    if len(path) <= 1:
        return 0.0
    if style == "iterative":
        return sum(
            2.0
            * network_latency(origin, hop, topology, coordinate_cache, transit_cache)
            for hop in path[1:]
        )
    total = 0.0
    for left, right in zip(path, path[1:]):
        total += network_latency(left, right, topology, coordinate_cache, transit_cache)
    total += network_latency(responsible, origin, topology, coordinate_cache, transit_cache)
    return total


def network_latency(
    left: int,
    right: int,
    topology: str,
    coordinate_cache: dict[int, tuple[float, float, float]],
    transit_cache: dict[int, tuple[int, int]],
) -> float:
    if left == right:
        return 0.000001
    if topology == "3d_space":
        lx, ly, lz = coordinate_cache[left]
        rx, ry, rz = coordinate_cache[right]
        return math.sqrt((lx - rx) ** 2 + (ly - ry) ** 2 + (lz - rz) ** 2)
    left_transit, left_stub = transit_cache[left]
    right_transit, right_stub = transit_cache[right]
    if left_stub == right_stub:
        return 1.0
    if left_transit == right_transit:
        return 42.0
    return 92.0


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--include",
        default="all",
        help="comma-separated paper items: table_ii,table_iii,table_iv,fig_8,fig_9,fig_10,all",
    )
    args = parser.parse_args()
    include = {item.strip() for item in args.include.split(",") if item.strip()}
    if "all" in include:
        include = {"table_ii", "table_iii", "table_iv", "fig_8", "fig_9", "fig_10"}

    started = time.perf_counter()
    generators = [
        ("table_ii", paper_table_ii),
        ("table_iii", paper_table_iii),
        ("fig_8", paper_fig_8),
        ("fig_9", paper_fig_9),
        ("fig_10", paper_fig_10),
        ("table_iv", paper_table_iv),
    ]
    for name, generator in generators:
        if name not in include:
            continue
        for row in generator():
            row["simulator"] = "scripts/chord_paper_sim.py"
            row["elapsed_since_start_ms"] = round((time.perf_counter() - started) * 1000.0, 3)
            print(json.dumps(row, sort_keys=True))


if __name__ == "__main__":
    main()
