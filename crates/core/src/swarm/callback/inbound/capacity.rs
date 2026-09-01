//! Pure request-size validation for the bounded inbound mailbox.

use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

use super::InboundLane;
use super::INBOUND_LANE_COUNT;
use super::INBOUND_MAILBOX_BYTE_CAPACITY;
use super::INBOUND_MAILBOX_CAPACITY;
use super::INBOUND_PEER_BYTE_CAPACITY;
use super::INBOUND_PEER_CAPACITY;
use super::INBOUND_RESERVED_BYTES;
use super::INBOUND_RESERVED_BYTES_PER_LANE;
use super::INBOUND_RESERVED_TRANSFERS_PER_LANE;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::admissible_capacity;
use crate::fair_admission::retained_wire_bytes;

const _: () = {
    // One peer cannot consume the node budget, every lane retains a fixed
    // minimum, and one maximum legal frame always fits that minimum.
    assert!(INBOUND_PEER_CAPACITY < INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_PEER_BYTE_CAPACITY < INBOUND_MAILBOX_BYTE_CAPACITY);
    assert!(retained_wire_bytes(crate::consts::TRANSPORT_MAX_SIZE) <= INBOUND_PEER_BYTE_CAPACITY);
    assert!(INBOUND_RESERVED_TRANSFERS_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_CAPACITY);
    assert!(INBOUND_RESERVED_BYTES_PER_LANE * INBOUND_LANE_COUNT <= INBOUND_MAILBOX_BYTE_CAPACITY);
    assert!(memory_reservation(MAX_DATA_CHANNEL_MESSAGE_SIZE) <= INBOUND_RESERVED_BYTES_PER_LANE);
};

pub(super) const fn memory_reservation(bytes: usize) -> usize {
    retained_wire_bytes(bytes)
}

pub(super) fn memory_capacity_error(requested_bytes: usize) -> Error {
    Error::InboundMailboxMemoryCapacityExceeded {
        requested_bytes,
        capacity_bytes: INBOUND_MAILBOX_BYTE_CAPACITY,
    }
}

pub(super) fn peer_memory_capacity_error(peer: Option<Did>, requested_bytes: usize) -> Error {
    Error::InboundPeerMemoryCapacityExceeded {
        peer,
        requested_bytes,
        capacity_bytes: INBOUND_PEER_BYTE_CAPACITY,
    }
}

pub(super) fn validate_peer_memory_request(
    peer: Option<Did>,
    requested_bytes: usize,
) -> Result<()> {
    if requested_bytes > INBOUND_PEER_BYTE_CAPACITY {
        return Err(peer_memory_capacity_error(peer, requested_bytes));
    }
    Ok(())
}

pub(super) fn validate_memory_request(lane: InboundLane, requested_bytes: usize) -> Result<()> {
    let limit = admissible_capacity(
        INBOUND_MAILBOX_BYTE_CAPACITY,
        &INBOUND_RESERVED_BYTES,
        lane.index(),
    );
    if requested_bytes > limit {
        return Err(Error::InboundMailboxMemoryCapacityExceeded {
            requested_bytes,
            capacity_bytes: limit,
        });
    }
    Ok(())
}
