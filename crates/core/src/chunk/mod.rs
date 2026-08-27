#![deny(missing_docs)]
//! Message framing / chunking. A message larger than the connection's negotiated
//! `max_message_size` is split into MTU-sized [`Chunk`]s on the sender and reassembled on the
//! receiver.
//!
//! NOTE: this is **whole-message** buffering, not MSRP-style (RFC 4975) streaming. There is no
//! incremental delivery: the receiver yields a payload only once *every* chunk has arrived (or
//! drops it on TTL). The outbound scheduler keeps each message class FIFO, so two chunked messages
//! in the same class do not interleave; a higher-priority class may run between chunks while a
//! delivery is pending. The "split into ordered, id-tagged pieces and reassemble" idea is borrowed
//! from MSRP chunking; MSRP interruption semantics are not implemented.
//!
//! Two halves, deliberately separated:
//!
//! - **Send** - [`ChunkList`] turns a [`bytes::Bytes`] into ordered [`Chunk`]s, where `chunk_size`
//!   comes from the connection's negotiated `max_message_size`. The sender uses
//!   [`ChunkList::stream`], which yields chunks lazily as zero-copy slices so one chunk is held in
//!   flight at a time; [`ChunkList::split`] (eager `Vec`) remains for tests.
//! - **Receive** - [`MessageReassembler`] collects incoming [`Chunk`]s keyed by message id and
//!   yields the original payload once every position has arrived.
//!
//! The receiver is robust to the realities of a multi-hop / DHT overlay: out-of-order arrival,
//! **duplicates / retransmits** (first write per position wins), and partial messages (evicted
//! by TTL). It is also bounded against a hostile peer: per-chunk and per-message byte caps, a
//! global buffered-cost ceiling (charging a per-slot overhead so tiny-chunk floods are bounded by
//! count too), an id-count cap, and up-front rejection of already-expired chunks. No single id and
//! no peer-supplied `total` can drive memory without limit. See [`MessageReassembler`].
//!
//! ```text
//!   send    : Bytes -> [Chunk{ chunk=[i, n], data=data_i, meta } | i in 0..n]
//!   receive : a message id is complete iff received positions = 0..total (all n of them);
//!             then payload = concat(data_i for i in 0..total)
//! ```

mod framing;
mod limits;
mod reassembly;

pub use framing::Chunk;
pub use framing::ChunkList;
pub use framing::ChunkMeta;
pub use framing::Framing;
pub use framing::WireReserves;
pub use limits::ReassemblyLimits;
pub use reassembly::MessageReassembler;
pub(crate) use reassembly::ReassemblyBudget;
pub(crate) use reassembly::ReassemblyOutcome;
pub(crate) use reassembly::ReassemblyRejection;
pub(crate) use reassembly::RetainedReassembly;

#[cfg(test)]
mod tests;
