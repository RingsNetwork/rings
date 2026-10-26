use rings_transport::core::credit::LANE_CREDIT_WINDOW;
use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

use crate::consts::MAX_TTL_MS;
use crate::consts::MIN_CHUNK_DATA;
use crate::consts::TRANSPORT_MAX_SIZE;

/// `ρ = max_message_bytes / TTL`: 60 MB over 100 min, 10 bytes per millisecond, the least rate
/// at which holding an open message's reservation is no cheaper than holding the bytes
/// themselves (load-bounds paper, Law 4, #932).
const RATE_FLOOR_BYTES_PER_MS: u64 = TRANSPORT_MAX_SIZE as u64 / MAX_TTL_MS;

const _: () = assert!(RATE_FLOOR_BYTES_PER_MS == 10);

/// The limits a [`super::MessageReassembler`] enforces on incoming chunks, as an explicit value
/// rather than module globals. This keeps the core admission rule independent of *where* the
/// numbers come from: the shell supplies them (see [`ReassemblyLimits::production`]), the
/// reassembler only enforces what it is given, and tests can use small limits instead of giant
/// synthetic payloads.
///
/// Every cap is a validity bound an honest sender never meets, not a capacity: the memory a
/// message takes is charged once, by its first chunk, on the receiver's mailbox ledger
/// (load-bounds paper, Law 4, #932). The rate floor bounds how long an open message holds that
/// charge: by what its sender streams while it holds credit.
#[derive(Debug, Clone, Copy)]
pub struct ReassemblyLimits {
    /// Max number of distinct in-flight message ids on one connection: an honest sender keeps
    /// a message's chunks contiguous on its class lane, so it has at most one open per lane.
    pub max_pending_messages: usize,
    /// Max `data` bytes a single chunk may carry.
    pub max_chunk_data_len: usize,
    /// Max bytes of one message: a first chunk `[0, n]` of `d₀` bytes opens a message only while
    /// `(n − 1)·d₀ < max_message_bytes`, so the `n·d₀` it is charged for passes the message's
    /// own length by less than one chunk.
    pub max_message_bytes: usize,
    /// Max number of slots (chunks) one in-flight message may have - i.e. the largest `total` a
    /// chunk may claim. Caps the slot/`BTreeMap` count of a single message so a hostile peer cannot
    /// use one id with a huge `total` and tiny chunks to allocate millions of slots while staying
    /// under [`max_message_bytes`](Self::max_message_bytes) (which only counts data bytes).
    pub max_chunks_per_message: usize,
    /// Max number of recently-completed message ids remembered as tombstones, to suppress a
    /// re-delivery if a message is fully retransmitted after it already completed (within its TTL
    /// window). The same number separately caps invalid-terminal ids. Invalid ids fail open
    /// without peer attribution when their set is full, with a bounded saturation horizon
    /// preserving that decision while tombstones drain. Thus terminal bookkeeping retains at most
    /// twice this many ids across the independent completed and invalid sets, plus one scalar
    /// saturation horizon. NOTE: past
    /// this many *concurrent* live completion
    /// tombstones the oldest is dropped even if its TTL has not elapsed, so the "no
    /// post-completion redelivery" guarantee holds only for the most recent `max_completed_ids`
    /// completions within a TTL window.
    pub max_completed_ids: usize,
    /// `ρ`, in bytes per millisecond: an open message expires once the bytes it received fall
    /// below `ρ` times the credit-time its sender held unused credit for, less the grace (Law 4).
    pub rate_floor: u64,
    /// `W`, the grace of the rate floor in chunks: the frames a receiver may hold unprocessed
    /// while their sender's credit is spent, one lane window.
    pub grace_chunks: u64,
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
            max_completed_ids: 1024,
            rate_floor: RATE_FLOOR_BYTES_PER_MS,
            grace_chunks: LANE_CREDIT_WINDOW,
        }
    }

    /// Lower-bookkeeping limits for constrained deployments.
    ///
    /// The per-message ceiling remains protocol-compatible with production; constrained nodes
    /// keep fewer open ids and completion tombstones. Their reassembly memory is bounded, as
    /// every node's, by the mailbox ledger its first chunks are charged on.
    pub fn constrained() -> Self {
        Self {
            max_pending_messages: 64,
            max_chunk_data_len: MAX_DATA_CHANNEL_MESSAGE_SIZE,
            max_message_bytes: TRANSPORT_MAX_SIZE,
            max_chunks_per_message: TRANSPORT_MAX_SIZE / MIN_CHUNK_DATA + 1,
            max_completed_ids: 256,
            rate_floor: RATE_FLOOR_BYTES_PER_MS,
            grace_chunks: LANE_CREDIT_WINDOW,
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
            max_completed_ids: self.max_completed_ids.max(1),
            rate_floor: self.rate_floor.max(1),
            grace_chunks: self.grace_chunks,
        }
    }
}

impl Default for ReassemblyLimits {
    fn default() -> Self {
        Self::production()
    }
}
