use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::BinaryHeap;

use serde_json::json;
use serde_json::Map;
use serde_json::Value;

pub const RING_BITS: usize = 64;
const FINGER_TABLE_SIZE: usize = RING_BITS;

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("unsupported paper simulator include item {0:?}")]
    UnsupportedInclude(String),
    #[error("paper simulator event queue unexpectedly became empty")]
    EmptyEventQueue,
    #[error("active node set is empty")]
    EmptyActiveSet,
    #[error("failed to encode paper simulator row: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        splitmix64(self.state)
    }

    pub fn usize_below(&mut self, upper: usize) -> usize {
        if upper <= 1 {
            return 0;
        }
        (self.next_u64() as usize) % upper
    }

    pub fn f64_unit(&mut self) -> f64 {
        let value = self.next_u64() >> 11;
        value as f64 / ((1u64 << 53) as f64)
    }

    pub fn expovariate(&mut self, lambda: f64) -> f64 {
        let unit = self.f64_unit().clamp(f64::MIN_POSITIVE, 1.0 - f64::EPSILON);
        -unit.ln() / lambda
    }

    pub fn uniform(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.f64_unit()
    }
}

#[derive(Clone)]
pub struct RouteState {
    ids: Vec<u64>,
    successors: Vec<Vec<usize>>,
    fingers: Vec<Vec<usize>>,
}

#[derive(Clone, Copy)]
pub struct LookupResult {
    resolved: bool,
    correct: bool,
    hops: usize,
    timeouts: usize,
}

impl LookupResult {
    pub fn contacts(self) -> usize {
        self.hops.saturating_add(self.timeouts)
    }
}

#[derive(Clone)]
pub struct DynamicChord {
    successor_capacity: usize,
    capacity: usize,
    ids: Vec<u64>,
    index_by_ordinal: Vec<usize>,
    next_ordinal: usize,
    pub active: BTreeSet<usize>,
    active_ring: Vec<usize>,
    state: RouteState,
    finger_cursor: Vec<usize>,
}

#[derive(Clone, Copy)]
pub enum EventKind {
    Lookup,
    Churn,
    Stabilize { index: usize },
}

#[derive(Clone)]
pub struct ScheduledEvent {
    pub time: f64,
    pub sequence: usize,
    pub kind: EventKind,
}

impl Eq for ScheduledEvent {}

impl PartialEq for ScheduledEvent {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.sequence == other.sequence
    }
}

impl Ord for ScheduledEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        match other.time.partial_cmp(&self.time) {
            Some(ordering) => ordering.then_with(|| other.sequence.cmp(&self.sequence)),
            None => Ordering::Equal,
        }
    }
}

impl PartialOrd for ScheduledEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy)]
pub enum NetworkModel {
    Space3d,
    TransitStub,
}

impl NetworkModel {
    pub fn name(self) -> &'static str {
        match self {
            Self::Space3d => "3d_space",
            Self::TransitStub => "transit_stub",
        }
    }
}

#[derive(Clone, Copy)]
pub enum LookupStyle {
    Iterative,
    Recursive,
}

impl LookupStyle {
    pub fn name(self) -> &'static str {
        match self {
            Self::Iterative => "iterative",
            Self::Recursive => "recursive",
        }
    }
}

#[derive(Clone, Copy)]
pub struct LatencyContext<'a> {
    pub topology: NetworkModel,
    pub coordinates: &'a [(f64, f64, f64)],
    pub transit: &'a [(usize, usize)],
}

pub fn push_event(
    events: &mut BinaryHeap<ScheduledEvent>,
    sequence: &mut usize,
    time: f64,
    kind: EventKind,
) {
    events.push(ScheduledEvent {
        time,
        sequence: *sequence,
        kind,
    });
    *sequence = sequence.saturating_add(1);
}

pub fn splitmix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

pub fn deterministic_ids(count: usize, seed: u64) -> Vec<u64> {
    let mut ids = BTreeSet::new();
    let mut cursor = seed;
    while ids.len() < count {
        cursor = cursor.wrapping_add(1);
        let value = splitmix64(cursor);
        if value != 0 {
            ids.insert(value);
        }
    }
    ids.into_iter().collect()
}

pub fn selected_indices(count: usize, pct: usize, seed: u64) -> BTreeSet<usize> {
    let total = count
        .saturating_mul(pct)
        .saturating_add(50)
        .saturating_div(100)
        .min(count.saturating_sub(1));
    let mut scored = (0..count)
        .map(|index| (splitmix64(seed.wrapping_add(index as u64)), index))
        .collect::<Vec<_>>();
    scored.sort_by_key(|(score, index)| (*score, *index));
    scored
        .into_iter()
        .take(total)
        .map(|(_, index)| index)
        .collect()
}

pub fn build_stable_state(ids: &[u64], successor_capacity: usize) -> RouteState {
    let count = ids.len();
    RouteState {
        ids: ids.to_vec(),
        successors: (0..count)
            .map(|index| stable_successors(count, index, successor_capacity))
            .collect(),
        fingers: (0..count).map(|index| stable_fingers(ids, index)).collect(),
    }
}

fn stable_successors(count: usize, index: usize, capacity: usize) -> Vec<usize> {
    let limit = count.min(capacity.saturating_add(1));
    (1..limit).map(|offset| (index + offset) % count).collect()
}

fn stable_fingers(ids: &[u64], index: usize) -> Vec<usize> {
    let origin = ids.get(index).copied().unwrap_or(0);
    (0..FINGER_TABLE_SIZE)
        .map(|bit| successor_index(ids, origin.wrapping_add(1u64 << bit)))
        .collect()
}

pub fn successor_index(ids: &[u64], target: u64) -> usize {
    let position = ids.partition_point(|node_id| *node_id < target);
    if position < ids.len() {
        position
    } else {
        0
    }
}

pub fn successor_index_from_set(
    ids: &[u64],
    active: &BTreeSet<usize>,
    target: u64,
) -> Result<usize, BenchError> {
    if active.is_empty() {
        return Err(BenchError::EmptyActiveSet);
    }
    let position = successor_index(ids, target);
    for offset in 0..ids.len() {
        let candidate = (position + offset) % ids.len();
        if active.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(BenchError::EmptyActiveSet)
}

pub fn route_lookup(
    state: &mut RouteState,
    active: &BTreeSet<usize>,
    origin: usize,
    target: u64,
    expected: usize,
    update_dead_pointers: bool,
) -> LookupResult {
    let mut current = origin;
    let mut visited = BTreeSet::new();
    let mut timeouts = 0usize;

    for hops in 1..128 {
        if !active.contains(&current) || !visited.insert(current) {
            return LookupResult {
                resolved: false,
                correct: false,
                hops,
                timeouts,
            };
        }

        let current_id = state.ids.get(current).copied().unwrap_or(0);
        let target_distance = clockwise_distance(current_id, target);
        let successors = state.successors.get(current).cloned().unwrap_or_default();
        let mut missed_successors = BTreeSet::new();
        for successor in successors {
            let successor_id = state.ids.get(successor).copied().unwrap_or(0);
            let successor_distance = clockwise_distance(current_id, successor_id);
            if target_distance <= successor_distance {
                if active.contains(&successor) {
                    return LookupResult {
                        resolved: true,
                        correct: successor == expected,
                        hops,
                        timeouts,
                    };
                }
                timeouts = timeouts.saturating_add(1);
                missed_successors.insert(successor);
            }
        }
        if update_dead_pointers && !missed_successors.is_empty() {
            remove_dead_pointers(state, current, &missed_successors);
        }

        let mut candidates = Vec::new();
        for candidate in state
            .fingers
            .get(current)
            .into_iter()
            .flat_map(|fingers| fingers.iter().rev())
            .chain(
                state
                    .successors
                    .get(current)
                    .into_iter()
                    .flat_map(|successors| successors.iter().rev()),
            )
        {
            let candidate_id = state.ids.get(*candidate).copied().unwrap_or(0);
            let distance = clockwise_distance(current_id, candidate_id);
            if distance > 0 && distance < target_distance {
                candidates.push((distance, *candidate));
            }
        }
        candidates.sort_by(|(left_distance, left), (right_distance, right)| {
            right_distance
                .cmp(left_distance)
                .then_with(|| left.cmp(right))
        });

        let mut forwarded = false;
        let mut dead_candidates = BTreeSet::new();
        let source = current;
        let mut seen = BTreeSet::new();
        for (_, candidate) in candidates {
            if !seen.insert(candidate) {
                continue;
            }
            if !active.contains(&candidate) {
                timeouts = timeouts.saturating_add(1);
                dead_candidates.insert(candidate);
                continue;
            }
            current = candidate;
            forwarded = true;
            break;
        }
        if update_dead_pointers && !dead_candidates.is_empty() {
            remove_dead_pointers(state, source, &dead_candidates);
        }
        if !forwarded {
            return LookupResult {
                resolved: false,
                correct: false,
                hops,
                timeouts,
            };
        }
    }

    LookupResult {
        resolved: false,
        correct: false,
        hops: 128,
        timeouts,
    }
}

fn remove_dead_pointers(state: &mut RouteState, index: usize, dead: &BTreeSet<usize>) {
    if let Some(successors) = state.successors.get_mut(index) {
        successors.retain(|successor| !dead.contains(successor));
    }
    if let Some(fingers) = state.fingers.get_mut(index) {
        for finger in fingers {
            if dead.contains(finger) {
                *finger = index;
            }
        }
    }
}

impl DynamicChord {
    pub fn new(node_count: usize, successor_capacity: usize, seed: u64) -> Self {
        let capacity = node_count.saturating_add(6000);
        let mut pairs = (0..capacity)
            .map(|ordinal| {
                (
                    splitmix64(seed.wrapping_add(ordinal as u64).wrapping_add(1)),
                    ordinal,
                )
            })
            .collect::<Vec<_>>();
        pairs.sort_by_key(|(node_id, ordinal)| (*node_id, *ordinal));
        let mut ids = Vec::with_capacity(capacity);
        let mut index_by_ordinal = vec![0usize; capacity];
        for (index, (node_id, ordinal)) in pairs.into_iter().enumerate() {
            ids.push(node_id);
            if let Some(slot) = index_by_ordinal.get_mut(ordinal) {
                *slot = index;
            }
        }
        let active = (0..node_count)
            .filter_map(|ordinal| index_by_ordinal.get(ordinal).copied())
            .collect::<BTreeSet<_>>();
        let active_ring = active.iter().copied().collect::<Vec<_>>();
        let state = RouteState {
            ids,
            successors: vec![Vec::new(); capacity],
            fingers: vec![Vec::new(); capacity],
        };
        let mut chord = Self {
            successor_capacity,
            capacity,
            ids: state.ids.clone(),
            index_by_ordinal,
            next_ordinal: node_count,
            active,
            active_ring,
            state,
            finger_cursor: vec![0usize; capacity],
        };
        for index in chord.active_ring.clone() {
            chord.set_successors(index, chord.correct_successors(index));
            chord.set_fingers(
                index,
                (0..FINGER_TABLE_SIZE)
                    .map(|bit| chord.correct_finger(index, bit))
                    .collect(),
            );
        }
        chord
    }

    pub fn random_active(&self, rng: &mut DeterministicRng) -> Result<usize, BenchError> {
        self.active_ring
            .get(rng.usize_below(self.active_ring.len()))
            .copied()
            .ok_or(BenchError::EmptyActiveSet)
    }

    pub fn add_node(&mut self) -> usize {
        if self.next_ordinal >= self.capacity {
            self.next_ordinal = 0;
        }
        let index = self
            .index_by_ordinal
            .get(self.next_ordinal)
            .copied()
            .unwrap_or(0);
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        self.active.insert(index);
        insert_sorted(&mut self.active_ring, index);
        self.set_successors(index, self.correct_successors(index));
        self.set_fingers(
            index,
            (0..FINGER_TABLE_SIZE)
                .map(|bit| self.correct_finger(index, bit))
                .collect(),
        );
        self.set_finger_cursor(index, 0);
        index
    }

    pub fn remove_node(&mut self, rng: &mut DeterministicRng) -> Option<usize> {
        let position = rng.usize_below(self.active_ring.len());
        let departed = self.active_ring.get(position).copied()?;
        let predecessor = self.predecessor(departed);
        self.active.remove(&departed);
        self.active_ring.remove(position);
        if let Some(predecessor) = predecessor {
            self.set_successors(predecessor, self.correct_successors(predecessor));
        }
        Some(departed)
    }

    pub fn stabilize(&mut self, index: usize) {
        if !self.active.contains(&index) {
            return;
        }
        self.set_successors(index, self.correct_successors(index));
        let bit = self.finger_cursor.get(index).copied().unwrap_or(0) % FINGER_TABLE_SIZE;
        let next_finger = self.correct_finger(index, bit);
        if let Some(fingers) = self.state.fingers.get_mut(index) {
            if fingers.len() != FINGER_TABLE_SIZE {
                *fingers = vec![index; FINGER_TABLE_SIZE];
            }
            if let Some(slot) = fingers.get_mut(bit) {
                *slot = next_finger;
            }
        }
        self.set_finger_cursor(index, bit.saturating_add(1));
    }

    fn correct_successors(&self, index: usize) -> Vec<usize> {
        let Ok(position) = self.active_ring.binary_search(&index) else {
            return Vec::new();
        };
        let limit = self
            .active_ring
            .len()
            .min(self.successor_capacity.saturating_add(1));
        (1..limit)
            .filter_map(|offset| {
                self.active_ring
                    .get((position + offset) % self.active_ring.len())
                    .copied()
            })
            .collect()
    }

    fn correct_finger(&self, index: usize, bit: usize) -> usize {
        let target = self
            .ids
            .get(index)
            .copied()
            .unwrap_or(0)
            .wrapping_add(1u64 << bit);
        self.active_successor_index(target)
    }

    fn active_successor_index(&self, target: u64) -> usize {
        let position = successor_index(&self.ids, target);
        let active_position = self.active_ring.partition_point(|index| *index < position);
        if active_position < self.active_ring.len() {
            self.active_ring
                .get(active_position)
                .copied()
                .unwrap_or_default()
        } else {
            self.active_ring.first().copied().unwrap_or_default()
        }
    }

    fn predecessor(&self, index: usize) -> Option<usize> {
        if !self.active.contains(&index) || self.active_ring.len() <= 1 {
            return None;
        }
        let position = self.active_ring.binary_search(&index).ok()?;
        self.active_ring
            .get((position + self.active_ring.len() - 1) % self.active_ring.len())
            .copied()
    }

    pub fn lookup(&mut self, origin: usize, target: u64) -> LookupResult {
        let expected = self.active_successor_index(target);
        route_lookup(
            &mut self.state,
            &self.active,
            origin,
            target,
            expected,
            true,
        )
    }

    fn set_successors(&mut self, index: usize, successors: Vec<usize>) {
        if let Some(slot) = self.state.successors.get_mut(index) {
            *slot = successors;
        }
    }

    fn set_fingers(&mut self, index: usize, fingers: Vec<usize>) {
        if let Some(slot) = self.state.fingers.get_mut(index) {
            *slot = fingers;
        }
    }

    fn set_finger_cursor(&mut self, index: usize, cursor: usize) {
        if let Some(slot) = self.finger_cursor.get_mut(index) {
            *slot = cursor;
        }
    }
}

fn insert_sorted(values: &mut Vec<usize>, value: usize) {
    match values.binary_search(&value) {
        Ok(_) => {}
        Err(position) => values.insert(position, value),
    }
}

pub fn proximity_path(
    ids: &[u64],
    origin: usize,
    target: u64,
    successors_per_finger: usize,
    style: LookupStyle,
    context: LatencyContext<'_>,
) -> Vec<usize> {
    let mut current = origin;
    let mut path = vec![origin];
    let mut visited = BTreeSet::from([origin]);
    for _ in 0..128 {
        let current_id = ids.get(current).copied().unwrap_or(0);
        let target_distance = clockwise_distance(current_id, target);
        let immediate_successor = (current + 1) % ids.len();
        let successor_id = ids.get(immediate_successor).copied().unwrap_or(0);
        if target_distance <= clockwise_distance(current_id, successor_id) {
            path.push(immediate_successor);
            return path;
        }
        let finger = closest_preceding_finger(ids, current, target);
        let mut candidates = Vec::new();
        for offset in 0..=successors_per_finger {
            let candidate = (finger + offset) % ids.len();
            let candidate_id = ids.get(candidate).copied().unwrap_or(0);
            if in_interval(current_id, candidate_id, target) {
                candidates.push(candidate);
            }
        }
        if candidates.is_empty() {
            candidates.push(finger);
        }
        let anchor = match style {
            LookupStyle::Iterative => origin,
            LookupStyle::Recursive => current,
        };
        let next_hop = candidates
            .into_iter()
            .min_by(|left, right| {
                network_latency(anchor, *left, context)
                    .partial_cmp(&network_latency(anchor, *right, context))
                    .unwrap_or(Ordering::Equal)
            })
            .unwrap_or(finger);
        if !visited.insert(next_hop) {
            return path;
        }
        path.push(next_hop);
        current = next_hop;
    }
    path
}

fn closest_preceding_finger(ids: &[u64], current: usize, target: u64) -> usize {
    let current_id = ids.get(current).copied().unwrap_or(0);
    let target_distance = clockwise_distance(current_id, target);
    let mut best = current;
    let mut best_distance = 0u64;
    for bit in (0..FINGER_TABLE_SIZE).rev() {
        let candidate = successor_index(ids, current_id.wrapping_add(1u64 << bit));
        let candidate_id = ids.get(candidate).copied().unwrap_or(0);
        let distance = clockwise_distance(current_id, candidate_id);
        if distance > 0 && distance < target_distance && distance > best_distance {
            best = candidate;
            best_distance = distance;
        }
    }
    best
}

pub fn lookup_latency(
    path: &[usize],
    origin: usize,
    responsible: usize,
    style: LookupStyle,
    context: LatencyContext<'_>,
) -> f64 {
    if path.len() <= 1 {
        return 0.0;
    }
    match style {
        LookupStyle::Iterative => path
            .iter()
            .skip(1)
            .map(|hop| 2.0 * network_latency(origin, *hop, context))
            .sum(),
        LookupStyle::Recursive => {
            let mut total = 0.0;
            for pair in path.windows(2) {
                if let [left, right] = pair {
                    total += network_latency(*left, *right, context);
                }
            }
            total + network_latency(responsible, origin, context)
        }
    }
}

pub fn network_latency(left: usize, right: usize, context: LatencyContext<'_>) -> f64 {
    if left == right {
        return 0.000001;
    }
    match context.topology {
        NetworkModel::Space3d => {
            let (lx, ly, lz) = context
                .coordinates
                .get(left)
                .copied()
                .unwrap_or((0.0, 0.0, 0.0));
            let (rx, ry, rz) = context
                .coordinates
                .get(right)
                .copied()
                .unwrap_or((0.0, 0.0, 0.0));
            ((lx - rx).powi(2) + (ly - ry).powi(2) + (lz - rz).powi(2)).sqrt()
        }
        NetworkModel::TransitStub => {
            let (left_transit, left_stub) = context.transit.get(left).copied().unwrap_or((0, 0));
            let (right_transit, right_stub) = context.transit.get(right).copied().unwrap_or((0, 0));
            if left_stub == right_stub {
                1.0
            } else if left_transit == right_transit {
                42.0
            } else {
                92.0
            }
        }
    }
}

pub fn summarize_lookup_results(results: &[LookupResult]) -> Value {
    let contacts = results
        .iter()
        .map(|result| result.contacts())
        .collect::<Vec<_>>();
    let hops = results.iter().map(|result| result.hops).collect::<Vec<_>>();
    let timeouts = results
        .iter()
        .map(|result| result.timeouts)
        .collect::<Vec<_>>();
    let resolved = results.iter().filter(|result| result.resolved).count();
    let correct = results.iter().filter(|result| result.correct).count();
    let failed = results.len().saturating_sub(correct);
    json!({
        "lookups": results.len(),
        "resolved": resolved,
        "correct": correct,
        "failed": failed,
        "success_rate": ratio(resolved, results.len()),
        "correctness_rate": ratio(correct, results.len()),
        "avg_path_length": mean_usize(&hops),
        "path_length_p1": percentile_usize(&hops, 1.0),
        "path_length_p10": percentile_usize(&hops, 10.0),
        "path_length_p90": percentile_usize(&hops, 90.0),
        "path_length_p99": percentile_usize(&hops, 99.0),
        "avg_live_hops": mean_usize(&hops),
        "avg_contacts_including_timeouts": mean_usize(&contacts),
        "contacts_including_timeouts_p1": percentile_usize(&contacts, 1.0),
        "contacts_including_timeouts_p10": percentile_usize(&contacts, 10.0),
        "contacts_including_timeouts_p90": percentile_usize(&contacts, 90.0),
        "contacts_including_timeouts_p99": percentile_usize(&contacts, 99.0),
        "avg_timeouts": mean_usize(&timeouts),
        "timeouts_p1": percentile_usize(&timeouts, 1.0),
        "timeouts_p10": percentile_usize(&timeouts, 10.0),
        "timeouts_p90": percentile_usize(&timeouts, 90.0),
        "timeouts_p99": percentile_usize(&timeouts, 99.0),
        "lookup_failures_per_10k": ratio(failed.saturating_mul(10_000), results.len()),
    })
}

pub fn expected_key_counts(ids: &[u64], key_count: usize) -> Vec<usize> {
    if ids.is_empty() {
        return Vec::new();
    }
    let ring_size = 1u128 << RING_BITS;
    let mut lengths = Vec::with_capacity(ids.len());
    let mut previous = ids.last().copied().unwrap_or(0);
    for node_id in ids {
        lengths.push(clockwise_distance(previous, *node_id));
        previous = *node_id;
    }
    let mut counts = Vec::with_capacity(lengths.len());
    let mut fractions = Vec::with_capacity(lengths.len());
    for (index, length) in lengths.iter().copied().enumerate() {
        let scaled = (key_count as u128).saturating_mul(length as u128);
        counts.push((scaled / ring_size) as usize);
        fractions.push((scaled % ring_size, index));
    }
    let assigned = counts.iter().copied().sum::<usize>();
    let remainder = key_count.saturating_sub(assigned);
    fractions.sort_by(|(left_fraction, left), (right_fraction, right)| {
        right_fraction
            .cmp(left_fraction)
            .then_with(|| right.cmp(left))
    });
    for (_, index) in fractions.into_iter().take(remainder) {
        if let Some(slot) = counts.get_mut(index) {
            *slot = slot.saturating_add(1);
        }
    }
    counts
}

pub fn histogram(values: &[usize], width: usize) -> Value {
    let mut buckets = BTreeMap::new();
    for value in values {
        let bucket = value
            .saturating_div(width.max(1))
            .saturating_mul(width.max(1));
        let count = buckets.entry(bucket).or_insert(0usize);
        *count = count.saturating_add(1);
    }
    Value::Array(
        buckets
            .into_iter()
            .map(|(start, count)| {
                let mut object = Map::new();
                object.insert("start".to_string(), json!(start));
                object.insert(
                    "end".to_string(),
                    json!(start.saturating_add(width.max(1) - 1)),
                );
                object.insert("count".to_string(), json!(count));
                Value::Object(object)
            })
            .collect(),
    )
}

pub fn percentile_usize(values: &[usize], pct: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut ordered = values.to_vec();
    ordered.sort_unstable();
    let index = percentile_index(ordered.len(), pct);
    ordered.get(index).copied().unwrap_or(0) as f64
}

pub fn percentile_f64(values: &[f64], pct: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut ordered = values.to_vec();
    ordered.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    let index = percentile_index(ordered.len(), pct);
    ordered.get(index).copied().unwrap_or(0.0)
}

fn percentile_index(len: usize, pct: f64) -> usize {
    let raw = ((pct / 100.0) * len as f64).ceil() as usize;
    raw.saturating_sub(1).min(len.saturating_sub(1))
}

pub fn median_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut ordered = values.to_vec();
    ordered.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    let mid = ordered.len() / 2;
    if ordered.len().is_multiple_of(2) {
        let left = ordered.get(mid.saturating_sub(1)).copied().unwrap_or(0.0);
        let right = ordered.get(mid).copied().unwrap_or(0.0);
        (left + right) / 2.0
    } else {
        ordered.get(mid).copied().unwrap_or(0.0)
    }
}

pub fn mean_usize(values: &[usize]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().copied().sum::<usize>() as f64 / values.len() as f64
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

fn clockwise_distance(start: u64, end: u64) -> u64 {
    end.wrapping_sub(start)
}

fn in_interval(start: u64, value: u64, end: u64) -> bool {
    let distance = clockwise_distance(start, value);
    distance > 0 && distance < clockwise_distance(start, end)
}

pub fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}
