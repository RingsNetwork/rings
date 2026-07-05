use std::collections::BTreeSet;
use std::collections::BinaryHeap;
use std::env;
use std::time::Instant;

use serde_json::json;
use serde_json::Value;

#[path = "chord_paper_sim/support.rs"]
mod support;

use support::build_stable_state;
use support::deterministic_ids;
use support::expected_key_counts;
use support::histogram;
use support::lookup_latency;
use support::mean_usize;
use support::median_f64;
use support::network_latency;
use support::percentile_f64;
use support::percentile_usize;
use support::proximity_path;
use support::push_event;
use support::round3;
use support::route_lookup;
use support::selected_indices;
use support::splitmix64;
use support::successor_index;
use support::successor_index_from_set;
use support::summarize_lookup_results;
use support::BenchError;
use support::DeterministicRng;
use support::DynamicChord;
use support::EventKind;
use support::LatencyContext;
use support::LookupStyle;
use support::NetworkModel;
use support::RING_BITS;

const LOOKUPS_PER_TABLE: usize = 10_000;
const FIG10_LOOKUPS_PER_NODE: usize = 32;
const SIMULATOR_PATH: &str = "crates/core/benches/chord_paper_sim.rs";
type PaperGenerator = fn() -> Result<Vec<Value>, BenchError>;

fn main() -> Result<(), BenchError> {
    let include = include_items()?;
    let started = Instant::now();
    let generators: [(&str, PaperGenerator); 6] = [
        ("table_ii", paper_table_ii),
        ("table_iii", paper_table_iii),
        ("fig_8", paper_fig_8),
        ("fig_9", paper_fig_9),
        ("fig_10", paper_fig_10),
        ("table_iv", paper_table_iv),
    ];

    for (name, generator) in generators {
        if !include.contains(name) {
            continue;
        }
        for row in generator()? {
            emit(row, &started)?;
        }
    }

    Ok(())
}

fn include_items() -> Result<BTreeSet<String>, BenchError> {
    let mut raw = "all".to_string();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--include" {
            if let Some(value) = args.next() {
                raw = value;
            }
        } else if let Some(value) = arg.strip_prefix("--include=") {
            raw = value.to_string();
        }
    }

    let all = [
        "table_ii",
        "table_iii",
        "table_iv",
        "fig_8",
        "fig_9",
        "fig_10",
    ];
    let mut include = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    if include.contains("all") {
        include = all.iter().map(|item| (*item).to_string()).collect();
    }
    for item in &include {
        if !all.contains(&item.as_str()) {
            return Err(BenchError::UnsupportedInclude(item.clone()));
        }
    }
    Ok(include)
}

fn emit(mut row: Value, started: &Instant) -> Result<(), BenchError> {
    if let Value::Object(ref mut object) = row {
        object.insert("simulator".to_string(), json!(SIMULATOR_PATH));
        object.insert(
            "elapsed_since_start_ms".to_string(),
            json!(round3(started.elapsed().as_secs_f64() * 1000.0)),
        );
    }
    println!("{}", serde_json::to_string(&row)?);
    Ok(())
}

fn paper_table_ii() -> Result<Vec<Value>, BenchError> {
    let node_count = 1000usize;
    let successor_capacity = 20usize;
    let ids = deterministic_ids(node_count, 0x2000);
    let state = build_stable_state(&ids, successor_capacity);
    let mut rng = DeterministicRng::new(0x2200);
    let mut rows = Vec::new();

    for failed_pct in [0usize, 10, 20, 30, 40, 50] {
        let failed = selected_indices(node_count, failed_pct, 0x2300 + failed_pct as u64);
        let active = (0..node_count)
            .filter(|index| !failed.contains(index))
            .collect::<BTreeSet<_>>();
        let active_origins = active.iter().copied().collect::<Vec<_>>();
        let mut state_snapshot = state.clone();
        let mut results = Vec::with_capacity(LOOKUPS_PER_TABLE);
        for lookup in 0..LOOKUPS_PER_TABLE {
            let origin = active_origins
                .get(lookup % active_origins.len())
                .copied()
                .ok_or(BenchError::EmptyActiveSet)?;
            let target = rng.next_u64();
            let expected = successor_index_from_set(&ids, &active, target)?;
            results.push(route_lookup(
                &mut state_snapshot,
                &active,
                origin,
                target,
                expected,
                false,
            ));
        }
        rows.push(json!({
            "report": "chord_paper_sim",
            "paper_item": "table_ii",
            "scenario": "simultaneous_node_failures",
            "node_count": node_count,
            "ring_identifier_bits": RING_BITS,
            "successor_list_size": successor_capacity,
            "failed_node_pct": failed_pct,
            "failed_nodes": failed.len(),
            "metrics": summarize_lookup_results(&results),
            "paper_reference": {
                "network_nodes": 1000,
                "successor_list_size": "20 = 2 log2 N",
                "lookups": 10_000,
            },
        }));
    }

    Ok(rows)
}

fn paper_table_iii() -> Result<Vec<Value>, BenchError> {
    let node_count = 1000usize;
    let successor_capacity = 20usize;
    let rates = [0.05f64, 0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40];
    let mut rows = Vec::new();

    for rate in rates {
        let scaled_rate = (rate * 1000.0) as u64;
        let mut rng = DeterministicRng::new(0x3000 + scaled_rate);
        let mut chord = DynamicChord::new(node_count, successor_capacity, 0x3100 + scaled_rate);
        let mut events = BinaryHeap::new();
        let mut sequence = 0usize;
        push_event(
            &mut events,
            &mut sequence,
            rng.expovariate(1.0),
            EventKind::Lookup,
        );
        push_event(
            &mut events,
            &mut sequence,
            rng.expovariate(rate),
            EventKind::Churn,
        );
        for index in chord.active.iter().copied() {
            push_event(
                &mut events,
                &mut sequence,
                rng.uniform(15.0, 45.0),
                EventKind::Stabilize { index },
            );
        }

        let mut results = Vec::with_capacity(LOOKUPS_PER_TABLE);
        while results.len() < LOOKUPS_PER_TABLE {
            let event = events.pop().ok_or(BenchError::EmptyEventQueue)?;
            match event.kind {
                EventKind::Lookup => {
                    let origin = chord.random_active(&mut rng)?;
                    let target = rng.next_u64();
                    results.push(chord.lookup(origin, target));
                    push_event(
                        &mut events,
                        &mut sequence,
                        event.time + rng.expovariate(1.0),
                        EventKind::Lookup,
                    );
                }
                EventKind::Churn => {
                    if chord.active.len() > 1 {
                        chord.remove_node(&mut rng);
                    }
                    let joined = chord.add_node();
                    push_event(
                        &mut events,
                        &mut sequence,
                        event.time + rng.uniform(15.0, 45.0),
                        EventKind::Stabilize { index: joined },
                    );
                    push_event(
                        &mut events,
                        &mut sequence,
                        event.time + rng.expovariate(rate),
                        EventKind::Churn,
                    );
                }
                EventKind::Stabilize { index } => {
                    chord.stabilize(index);
                    if chord.active.contains(&index) {
                        push_event(
                            &mut events,
                            &mut sequence,
                            event.time + rng.uniform(15.0, 45.0),
                            EventKind::Stabilize { index },
                        );
                    }
                }
            }
        }

        rows.push(json!({
            "report": "chord_paper_sim",
            "paper_item": "table_iii",
            "scenario": "lookups_during_stabilization",
            "node_count_initial": node_count,
            "node_count_final": chord.active.len(),
            "ring_identifier_bits": RING_BITS,
            "successor_list_size": successor_capacity,
            "join_leave_rate_per_second": rate,
            "join_leave_rate_per_stabilization_period": rate * 30.0,
            "metrics": summarize_lookup_results(&results),
            "paper_reference": {
                "network_nodes": "roughly 1000",
                "successor_list_size": "20 = 2 log2 N",
                "stabilize_interval_seconds": "[15, 45]",
                "lookup_rate_per_second": 1,
            },
        }));
    }

    Ok(rows)
}

fn paper_fig_8() -> Result<Vec<Value>, BenchError> {
    let node_count = 10_000usize;
    let key_counts = (100_000usize..=1_000_000usize).step_by(100_000);
    let mut rows = Vec::new();

    for key_count in key_counts {
        let mut all_counts = Vec::new();
        for seed in 0..20u64 {
            let ids = deterministic_ids(node_count, 0x4000 + seed);
            all_counts.extend(expected_key_counts(&ids, key_count));
        }
        let row = json!({
            "report": "chord_paper_sim",
            "paper_item": "fig_8a",
            "scenario": "load_balance_consistent_hashing",
            "node_count": node_count,
            "ring_identifier_bits": RING_BITS,
            "key_count": key_count,
            "seeds": 20,
            "mean_keys_per_node": mean_usize(&all_counts),
            "keys_per_node_p1": percentile_usize(&all_counts, 1.0),
            "keys_per_node_p99": percentile_usize(&all_counts, 99.0),
            "max_keys_per_node": all_counts.iter().copied().max().unwrap_or(0),
            "paper_reference": {
                "node_count": "10^4",
                "key_count_range": "10^5..10^6",
                "seeds": 20,
            },
        });
        rows.push(row.clone());
        if key_count == 500_000 {
            let mut pdf_row = row;
            if let Value::Object(ref mut object) = pdf_row {
                object.insert("paper_item".to_string(), json!("fig_8b"));
                object.insert("pdf".to_string(), histogram(&all_counts, 10));
            }
            rows.push(pdf_row);
        }
    }

    Ok(rows)
}

fn paper_fig_9() -> Result<Vec<Value>, BenchError> {
    let real_nodes = 10_000usize;
    let key_count = 1_000_000usize;
    let mut rows = Vec::new();

    for virtual_nodes_per_real in [1usize, 2, 5, 10, 20] {
        let mut ids_by_real = Vec::with_capacity(real_nodes.saturating_mul(virtual_nodes_per_real));
        for real in 0..real_nodes {
            for vnode in 0..virtual_nodes_per_real {
                let seed = 0x5000u64
                    .wrapping_add((real as u64).wrapping_mul(1009))
                    .wrapping_add(vnode as u64);
                ids_by_real.push((splitmix64(seed), real));
            }
        }
        ids_by_real.sort_by_key(|(node_id, real)| (*node_id, *real));
        let virtual_ids = ids_by_real
            .iter()
            .map(|(node_id, _)| *node_id)
            .collect::<Vec<_>>();
        let virtual_counts = expected_key_counts(&virtual_ids, key_count);
        let mut real_counts = vec![0usize; real_nodes];
        for ((_, real), count) in ids_by_real.iter().zip(virtual_counts.iter().copied()) {
            if let Some(slot) = real_counts.get_mut(*real) {
                *slot = slot.saturating_add(count);
            }
        }
        rows.push(json!({
            "report": "chord_paper_sim",
            "paper_item": "fig_9",
            "scenario": "load_balance_virtual_nodes",
            "runtime_support": "simulator_only",
            "real_node_count": real_nodes,
            "ring_identifier_bits": RING_BITS,
            "virtual_nodes_per_real": virtual_nodes_per_real,
            "virtual_node_count": virtual_ids.len(),
            "key_count": key_count,
            "mean_keys_per_real_node": mean_usize(&real_counts),
            "keys_per_real_node_p1": percentile_usize(&real_counts, 1.0),
            "keys_per_real_node_p99": percentile_usize(&real_counts, 99.0),
            "max_keys_per_real_node": real_counts.iter().copied().max().unwrap_or(0),
            "paper_reference": {
                "real_node_count": "10^4",
                "key_count": "10^6",
                "virtual_nodes_per_real": [1, 2, 5, 10, 20],
            },
        }));
    }

    Ok(rows)
}

fn paper_fig_10() -> Result<Vec<Value>, BenchError> {
    let mut rows = Vec::new();

    for k in 3usize..15 {
        let node_count = 1usize << k;
        let ids = deterministic_ids(node_count, 0x6000 + k as u64);
        let mut state = build_stable_state(&ids, 1);
        let active = (0..node_count).collect::<BTreeSet<_>>();
        let mut rng = DeterministicRng::new(0x6100 + k as u64);
        let lookups = node_count
            .saturating_mul(FIG10_LOOKUPS_PER_NODE)
            .min(20_000);
        let mut results = Vec::with_capacity(lookups);
        for lookup in 0..lookups {
            let origin = lookup % node_count;
            let target = rng.next_u64();
            let expected = successor_index(&ids, target);
            results.push(route_lookup(
                &mut state, &active, origin, target, expected, false,
            ));
        }
        let row = json!({
            "report": "chord_paper_sim",
            "paper_item": "fig_10a",
            "scenario": "path_length_scaling",
            "k": k,
            "node_count": node_count,
            "ring_identifier_bits": RING_BITS,
            "stored_keys": 100 * node_count,
            "lookups": lookups,
            "metrics": summarize_lookup_results(&results),
            "paper_reference": {
                "node_count": "2^k, k=3..14",
                "stored_keys": "100 * 2^k",
            },
        });
        rows.push(row.clone());
        if k == 12 {
            let contacts = results
                .iter()
                .map(|result| result.contacts())
                .collect::<Vec<_>>();
            let mut pdf_row = row;
            if let Value::Object(ref mut object) = pdf_row {
                object.insert("paper_item".to_string(), json!("fig_10b"));
                object.insert("path_length_pdf".to_string(), histogram(&contacts, 1));
            }
            rows.push(pdf_row);
        }
    }

    Ok(rows)
}

fn paper_table_iv() -> Result<Vec<Value>, BenchError> {
    let node_count = 1usize << 16;
    let ids = deterministic_ids(node_count, 0x7000);
    let mut rng = DeterministicRng::new(0x7100);
    let coordinates = (0..node_count)
        .map(|_| (rng.f64_unit(), rng.f64_unit(), rng.f64_unit()))
        .collect::<Vec<_>>();
    let transit = (0..node_count)
        .map(|_| (rng.usize_below(20), rng.usize_below(250)))
        .collect::<Vec<_>>();
    let lookup_pairs = (0..LOOKUPS_PER_TABLE)
        .map(|_| (rng.usize_below(node_count), rng.next_u64()))
        .collect::<Vec<_>>();
    let mut rows = Vec::new();

    for successors_per_finger in [1usize, 2, 4, 8, 16] {
        for topology in [NetworkModel::Space3d, NetworkModel::TransitStub] {
            for style in [LookupStyle::Iterative, LookupStyle::Recursive] {
                let context = LatencyContext {
                    topology,
                    coordinates: &coordinates,
                    transit: &transit,
                };
                let mut stretches = Vec::with_capacity(LOOKUPS_PER_TABLE);
                for (origin, target) in &lookup_pairs {
                    let responsible = successor_index(&ids, *target);
                    let path = proximity_path(
                        &ids,
                        *origin,
                        *target,
                        successors_per_finger,
                        style,
                        context,
                    );
                    let actual = lookup_latency(&path, *origin, responsible, style, context);
                    let optimal =
                        (2.0 * network_latency(*origin, responsible, context)).max(0.000001);
                    stretches.push(actual / optimal);
                }
                rows.push(json!({
                    "report": "chord_paper_sim",
                    "paper_item": "table_iv",
                    "scenario": "lookup_latency_stretch",
                    "node_count": node_count,
                    "ring_identifier_bits": RING_BITS,
                    "fingers_successors": successors_per_finger,
                    "lookup_style": style.name(),
                    "network_model": topology.name(),
                    "lookups": LOOKUPS_PER_TABLE,
                    "stretch_median": median_f64(&stretches),
                    "stretch_p10": percentile_f64(&stretches, 10.0),
                    "stretch_p90": percentile_f64(&stretches, 90.0),
                    "paper_reference": {
                        "node_count": "2^16",
                        "fingers_successors": [1, 2, 4, 8, 16],
                        "network_models": ["3-d Euclidean space", "transit-stub"],
                    },
                }));
            }
        }
    }

    Ok(rows)
}
