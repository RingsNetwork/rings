use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_CHUNK_ENVELOPE_OVERHEAD;
use crate::consts::MIN_CHUNK_DATA;
use crate::consts::TRANSPORT_CUSTOM_OVERHEAD;
use crate::error::Error;
use crate::error::Result;
use crate::utils::get_epoch_ms;

/// One chunk of a chunked message, as it travels on the wire.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chunk {
    /// `[position, total]` - this chunk's index and the number of chunks in the message.
    pub chunk: [usize; 2],
    /// chunk payload bytes
    pub data: Bytes,
    /// meta data of chunk
    pub meta: ChunkMeta,
}

impl Chunk {
    /// Serialize chunk to the Rings wire encoding.
    pub fn to_wire(&self) -> Result<Bytes> {
        rings_codec::serialize(self)
            .map(Bytes::from)
            .map_err(Error::CodecSerialize)
    }

    /// Deserialize chunk from the Rings wire encoding.
    pub fn from_wire(data: &[u8]) -> Result<Self> {
        rings_codec::deserialize(data).map_err(Error::CodecDeserialize)
    }
}

/// Meta data of a chunk
#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
pub struct ChunkMeta {
    /// uuid of msg
    pub id: uuid::Uuid,
    /// Created time
    pub ts_ms: u128,
    /// Time to live
    pub ttl_ms: u64,
}

impl Default for ChunkMeta {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            ts_ms: get_epoch_ms(),
            ttl_ms: DEFAULT_TTL_MS,
        }
    }
}

/// Sender side: an ordered list of [`Chunk`]s for one message. Build it from the payload with
/// [`ChunkList::split`], passing the per-message data size to cut at (the connection's negotiated
/// `max_message_size` minus the envelope reserve), then iterate (or convert to `Vec<Chunk>`) to put
/// each chunk on the wire. The cut size is a runtime argument rather than a type parameter because
/// it is decided per connection from the negotiated limit. Reassembly is the receiver's job - see
/// [`super::MessageReassembler`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChunkList(Vec<Chunk>);

impl ChunkList {
    /// Eagerly split `bytes` into chunks of at most `chunk_size` data bytes each, tagged
    /// `[i, total]`. A **test/helper** constructor (the production send path uses
    /// [`stream`](Self::stream), and [`WireReserves::plan`] never yields an unusable `chunk_size` -
    /// it returns `None` instead). `chunk_size` is clamped to at least 1 only as a defensive guard
    /// against a caller passing `0`; it is not a sanctioned way to produce 1-byte chunks on the
    /// wire.
    pub fn split(bytes: &Bytes, chunk_size: usize) -> Self {
        let chunk_size = chunk_size.max(1);
        let chunks: Vec<Bytes> = bytes
            .chunks(chunk_size)
            .map(|c| c.to_vec().into())
            .collect();
        let chunks_len: usize = chunks.len();
        let meta = ChunkMeta::default();
        Self(
            chunks
                .into_iter()
                .enumerate()
                .map(|(i, data)| Chunk {
                    meta,
                    chunk: [i, chunks_len],
                    data,
                })
                .collect::<Vec<Chunk>>(),
        )
    }

    /// Stream `bytes` into chunks of at most `chunk_size` data bytes each **without materializing
    /// the whole list**: each chunk's `data` is a zero-copy [`Bytes::slice`] of the input, and the
    /// chunks are yielded lazily, so a sender can frame and flush one chunk at a time with bounded
    /// memory (rather than allocating every chunk up front). All chunks share one `[i, total]`
    /// numbering and one [`ChunkMeta`]. `chunk_size` is clamped to at least 1 so a degenerate value
    /// still terminates; empty input yields **no** chunks, agreeing with [`split`](Self::split).
    pub fn stream(bytes: Bytes, chunk_size: usize) -> impl Iterator<Item = Chunk> {
        let chunk_size = chunk_size.max(1);
        let total = bytes.len().div_ceil(chunk_size);
        let meta = ChunkMeta::default();
        (0..total).map(move |i| {
            let start = i * chunk_size;
            let end = start.saturating_add(chunk_size).min(bytes.len());
            Chunk {
                meta,
                chunk: [i, total],
                data: bytes.slice(start..end),
            }
        })
    }

    /// Clone out the chunks.
    pub fn to_vec(&self) -> Vec<Chunk> {
        self.0.clone()
    }

    /// Borrow the chunks.
    pub fn as_vec(&self) -> &Vec<Chunk> {
        &self.0
    }
}

impl IntoIterator for &ChunkList {
    type Item = Chunk;
    type IntoIter = std::vec::IntoIter<Chunk>;

    fn into_iter(self) -> Self::IntoIter {
        self.to_vec().into_iter()
    }
}

impl IntoIterator for ChunkList {
    type Item = Chunk;
    type IntoIter = std::vec::IntoIter<Chunk>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl From<ChunkList> for Vec<Chunk> {
    fn from(l: ChunkList) -> Self {
        l.0
    }
}

impl From<Vec<Chunk>> for ChunkList {
    fn from(data: Vec<Chunk>) -> Self {
        Self(data)
    }
}

/// How one payload should be framed for a size-limited connection: sent whole, or split.
///
/// This is the *decision* only - a value, with no I/O - so the sender's effectful path
/// (`do_send_payload`) is a thin shell that matches on it. Separating the rule from the act keeps
/// the rule exhaustively testable in isolation (functional core / imperative shell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// The payload is within the connection's limit; send it as a single message, unchanged.
    Whole,
    /// The payload exceeds the limit; split it into [`Chunk`]s of at most `chunk_size` data bytes
    /// each (via [`ChunkList::split`]), each then re-wrapped in its own envelope.
    Chunked {
        /// Maximum data bytes per chunk.
        chunk_size: usize,
    },
}

/// The bytes the transport adds around a payload on the wire, per framing path. Bundled as a named
/// value so the framing rule reads `reserves.plan(len, limit)` instead of a row of positional
/// `usize`s, and so the production reserves live in exactly one place ([`WireReserves::PRODUCTION`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireReserves {
    /// Bytes added around a *whole* payload - the outer `TransportMessage::Custom` frame.
    pub whole: usize,
    /// Bytes added around *each chunk's* data - its `MessagePayload` envelope **and** the outer
    /// `TransportMessage::Custom` frame.
    pub chunk: usize,
    /// Smallest per-chunk data payload worth producing; a limit that cannot fit `chunk +
    /// min_chunk_data` is rejected rather than fragmented into near-empty chunks.
    pub min_chunk_data: usize,
}

impl WireReserves {
    /// The reserves used in production, derived from the transport/message ceilings.
    pub const PRODUCTION: Self = Self {
        whole: TRANSPORT_CUSTOM_OVERHEAD,
        chunk: MAX_CHUNK_ENVELOPE_OVERHEAD + TRANSPORT_CUSTOM_OVERHEAD,
        min_chunk_data: MIN_CHUNK_DATA,
    };

    /// Frame a `payload_len`-byte payload for a connection whose negotiated per-message limit is
    /// `max_message_size`. The decision is taken against the *wire* bytes (payload + reserves), not
    /// the bare payload, and is a pure total function:
    ///
    /// ```text
    ///   plan : (len, limit) -> Whole                   if len + whole <= limit
    ///                       -> Chunked(limit - chunk)  if limit >= chunk + min_chunk_data
    ///                       -> None                    otherwise
    /// ```
    ///
    /// `None` means the peer's limit is too small for even one useful chunk - a failure the caller
    /// surfaces, never a flood of 1-byte chunks. When `Chunked { chunk_size }` is returned,
    /// `min_chunk_data <= chunk_size` and `chunk_size + chunk <= limit`, so every wrapped chunk fits
    /// and a payload yields at most `ceil(len / min_chunk_data)` chunks. Every sum is `checked`, so
    /// the function is total over all `usize` inputs (no overflow/underflow).
    pub fn plan(&self, payload_len: usize, max_message_size: usize) -> Option<Framing> {
        let whole_fits = payload_len
            .checked_add(self.whole)
            .is_some_and(|wire| wire <= max_message_size);
        if whole_fits {
            return Some(Framing::Whole);
        }
        let min_viable = self.chunk.checked_add(self.min_chunk_data)?;
        (max_message_size >= min_viable).then(|| Framing::Chunked {
            chunk_size: max_message_size - self.chunk,
        })
    }
}
