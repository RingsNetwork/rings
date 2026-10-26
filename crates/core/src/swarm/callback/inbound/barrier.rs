//! The reassembly barrier, per message category (load-bounds paper, Def. "Per-category
//! barrier", #927).
//!
//! A reassembled message keeps its final chunk's sequence and enters the lane of its category,
//! which the lane-mismatch refusal holds to the category of the carrier its chunks took. So a
//! frame of a category waits only for the unfinished reassembly of that category:
//!
//! ```text
//!   dispatch(lane, f) ⟺ ¬∃ u ∈ unfinished: seq(u) < seq(f) ∧ category(u) ∈ { lane, ⊤ }
//! ```
//!
//! where `⊤` is a chunk on no carrier, which only a test hands in and which orders every lane.
//! Law (order per category): the messages of one category are processed in sequence order,
//! whole or chunked; no order is kept across categories, and no protocol relies on one.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use super::InboundLane;

/// The reassembly events ticketed and not yet completed, by category.
#[derive(Default)]
pub(super) struct UnfinishedReassembly {
    /// Each unfinished event's category, by sequence.
    category: BTreeMap<u64, InboundLane>,
    /// The unfinished events of each category, in sequence order; the reassembly lane's are
    /// those of no carrier (`⊤`).
    by_category: BTreeMap<InboundLane, BTreeSet<u64>>,
}

impl UnfinishedReassembly {
    /// The reassembly event `sequence` of `category` was ticketed.
    pub(super) fn begin(&mut self, sequence: u64, category: InboundLane) {
        self.category.insert(sequence, category);
        self.by_category
            .entry(category)
            .or_default()
            .insert(sequence);
    }

    /// The reassembly event `sequence` completed, handing its message, if any, to its lane, or
    /// was cancelled.
    pub(super) fn end(&mut self, sequence: u64) {
        let Some(category) = self.category.remove(&sequence) else {
            return;
        };
        if let Some(sequences) = self.by_category.get_mut(&category) {
            sequences.remove(&sequence);
            if sequences.is_empty() {
                self.by_category.remove(&category);
            }
        }
    }

    /// Whether the front `sequence` of `lane` waits for an earlier unfinished reassembly of its
    /// category, or of no carrier (pure).
    pub(super) fn blocks(&self, lane: InboundLane, sequence: u64) -> bool {
        [lane, InboundLane::Reassembly].iter().any(|category| {
            self.by_category
                .get(category)
                .and_then(BTreeSet::first)
                .is_some_and(|earliest| *earliest < sequence)
        })
    }

    /// Forget every unfinished event: the actor's queues are discarded at close.
    pub(super) fn clear(&mut self) {
        self.category.clear();
        self.by_category.clear();
    }
}
