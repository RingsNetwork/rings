use std::sync::atomic::Ordering;
use std::sync::Arc;

use bytes::Bytes;
use uuid::Uuid;

use super::*;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::utils::get_epoch_ms;

fn chunks_of(data: &Bytes, mtu: usize) -> Vec<Chunk> {
    ChunkList::split(data, mtu).into()
}

/// Tiny limits so the admission rule can be exercised without giant synthetic payloads.
fn small_limits() -> ReassemblyLimits {
    ReassemblyLimits {
        max_pending_messages: 4,
        max_chunk_data_len: 16,
        max_message_bytes: 100,
        max_chunks_per_message: 64,
        max_total_buffered_cost: 256,
        slot_overhead: 8,
        max_completed_ids: 8,
    }
}

mod adversarial;
mod basic;
