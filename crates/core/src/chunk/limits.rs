use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

use crate::consts::MIN_CHUNK_DATA;
use crate::consts::TRANSPORT_MAX_SIZE;

/// The limits a [`super::MessageReassembler`] enforces on incoming chunks, as an explicit value
/// rather than module globals. This keeps the core admission rule independent of *where* the
/// numbers come from: the shell supplies them (see [`ReassemblyLimits::production`]), the
/// reassembler only enforces what it is given, and tests can use small limits instead of giant
/// synthetic payloads.
#[derive(Debug, Clone, Copy)]
pub struct ReassemblyLimits {
    /// Max number of distinct in-flight message ids (a cheap first-line cap; the byte budgets are
    /// the real memory guard).
    pub max_pending_messages: usize,
    /// Max `data` bytes a single chunk may carry.
    pub max_chunk_data_len: usize,
    /// Max buffered data bytes for one in-flight message.
    pub max_message_bytes: usize,
    /// Max number of slots (chunks) one in-flight message may have - i.e. the largest `total` a
    /// chunk may claim. Caps the slot/`BTreeMap` count of a single message so a hostile peer cannot
    /// use one id with a huge `total` and tiny chunks to allocate millions of slots while staying
    /// under [`max_message_bytes`](Self::max_message_bytes) (which only counts data bytes).
    pub max_chunks_per_message: usize,
    /// Max buffered cost (data bytes + per-slot overhead) summed across all in-flight messages.
    pub max_total_buffered_cost: usize,
    /// Bookkeeping charge per slot - a *conservative estimate* (not an exact measurement) of the
    /// `BTreeMap` node plus `Bytes` header/refcount a slot costs, so a flood of *tiny* chunks is
    /// bounded by slot count, not only by summed data bytes. Real per-slot heap use may differ;
    /// this is deliberately generous so the budget over- rather than under-counts.
    pub slot_overhead: usize,
    /// Max number of recently-completed message ids remembered as tombstones, to suppress a
    /// re-delivery if a message is fully retransmitted after it already completed (within its TTL
    /// window). The same number separately caps invalid-terminal ids and ids rejected before local
    /// capacity could retain state. Invalid ids fail open without peer attribution when their set
    /// is full, with a bounded saturation horizon preserving that decision while tombstones drain;
    /// capacity-history saturation temporarily rejects all new ids for that peer. Thus
    /// terminal bookkeeping retains at most three times this many ids across the independent
    /// completed, invalid, and capacity sets, plus two scalar saturation horizons. NOTE: past
    /// this many *concurrent* live completion
    /// tombstones the oldest is dropped even if its TTL has not elapsed, so the "no
    /// post-completion redelivery" guarantee holds only for the most recent `max_completed_ids`
    /// completions within a TTL window.
    pub max_completed_ids: usize,
}

impl ReassemblyLimits {
    /// The limits used in production, derived from the transport / message ceilings. This is the one
    /// place that reaches for transport-specific constants; the reassembler itself does not.
    pub fn production() -> Self {
        Self {
            max_pending_messages: 512,
            // A chunk crosses the wire as one data-channel message, capped by SCTP.
            max_chunk_data_len: MAX_DATA_CHANNEL_MESSAGE_SIZE,
            // The sender refuses to send more than this, so a larger reassembled message is forged;
            // this is what stops the "one id, huge `total`, stream unique positions" attack.
            max_message_bytes: TRANSPORT_MAX_SIZE,
            // The sender never produces chunks smaller than `MIN_CHUNK_DATA`, so a legitimate
            // message needs at most this many; a larger `total` is forged.
            max_chunks_per_message: TRANSPORT_MAX_SIZE / MIN_CHUNK_DATA + 1,
            // Admits several concurrent maximum-size transfers while staying hard-bounded.
            max_total_buffered_cost: TRANSPORT_MAX_SIZE * 4,
            slot_overhead: 128,
            max_completed_ids: 1024,
        }
    }

    /// Lower-concurrency limits for constrained deployments.
    ///
    /// The per-message ceiling remains protocol-compatible with production.
    /// Constrained nodes instead admit fewer simultaneous messages and only one
    /// maximum-size reassembly, including its contiguous output copy.
    pub fn constrained() -> Self {
        const CONSTRAINED_MESSAGE_BYTES: usize = TRANSPORT_MAX_SIZE;
        const CONSTRAINED_MAX_CHUNKS: usize = CONSTRAINED_MESSAGE_BYTES / MIN_CHUNK_DATA + 1;
        const CONSTRAINED_SLOT_OVERHEAD: usize = 128;
        const CONSTRAINED_TOTAL_COST: usize =
            crate::fair_admission::retained_wire_bytes(CONSTRAINED_MESSAGE_BYTES)
                + CONSTRAINED_MAX_CHUNKS * CONSTRAINED_SLOT_OVERHEAD;

        Self {
            max_pending_messages: 64,
            max_chunk_data_len: MAX_DATA_CHANNEL_MESSAGE_SIZE,
            max_message_bytes: CONSTRAINED_MESSAGE_BYTES,
            max_chunks_per_message: CONSTRAINED_MAX_CHUNKS,
            max_total_buffered_cost: CONSTRAINED_TOTAL_COST,
            slot_overhead: CONSTRAINED_SLOT_OVERHEAD,
            max_completed_ids: 256,
        }
    }

    /// Clamp nonsensical values to safe minimums so a caller-supplied [`ReassemblyLimits`] cannot
    /// disable an invariant: every cap is forced to at least `1` (a `0` cap would, depending on the
    /// field, reject all traffic or - for `max_completed_ids` - silently void the tombstone
    /// guarantee the docs advertise). Applied by [`super::MessageReassembler::with_limits`].
    pub(super) fn normalized(self) -> Self {
        Self {
            max_pending_messages: self.max_pending_messages.max(1),
            max_chunk_data_len: self.max_chunk_data_len.max(1),
            max_message_bytes: self.max_message_bytes.max(1),
            max_chunks_per_message: self.max_chunks_per_message.max(1),
            max_total_buffered_cost: self.max_total_buffered_cost.max(1),
            slot_overhead: self.slot_overhead,
            max_completed_ids: self.max_completed_ids.max(1),
        }
    }

    /// Pending cost one peer may retain. One maximum-size legitimate message
    /// still fits, while the node-wide budget keeps capacity available to other
    /// peers instead of allowing a single incomplete-chunk flood to consume it.
    pub(super) fn max_peer_buffered_cost(self) -> usize {
        self.max_message_bytes
            .saturating_add(
                self.max_chunks_per_message
                    .saturating_mul(self.slot_overhead),
            )
            .min(self.max_total_buffered_cost)
    }
}

impl Default for ReassemblyLimits {
    fn default() -> Self {
        Self::production()
    }
}
