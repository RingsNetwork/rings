use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use super::*;
use crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

#[test]
fn test_raw_frame_capacity_releases_count_and_bytes_with_permit() {
    let capacity = Arc::new(InboundFrameCapacity::new());
    let permit = capacity
        .try_acquire("peer-a", INBOUND_PEER_BYTE_CAPACITY)
        .expect("one peer's byte allowance must fit");
    assert!(capacity.try_acquire("peer-a", 1).is_none());
    drop(permit);
    assert!(capacity.try_acquire("peer-a", 1).is_some());
}

#[test]
fn test_one_peer_cannot_exhaust_another_peers_allowance() {
    let capacity = Arc::new(InboundFrameCapacity::new());
    let permits = (0..INBOUND_PEER_FRAME_CAPACITY)
        .map(|_| capacity.try_acquire("noisy", 1))
        .collect::<Option<Vec<_>>>()
        .expect("one peer may use its complete frame allowance");

    assert!(capacity.try_acquire("noisy", 1).is_none());
    assert!(capacity.try_acquire("other", 1).is_some());
    drop(permits);
}

#[test]
fn test_raw_frame_capacity_is_shared_node_wide() {
    let capacity = Arc::new(InboundFrameCapacity::new());
    let permits = (0..INBOUND_FRAME_CAPACITY)
        .map(|index| {
            capacity.try_acquire(
                &format!("peer-{}", index / INBOUND_PEER_FRAME_CAPACITY),
                MAX_DATA_CHANNEL_MESSAGE_SIZE,
            )
        })
        .collect::<Option<Vec<_>>>()
        .expect("all node-wide frame slots must be available");

    assert!(capacity
        .try_acquire("extra", MAX_DATA_CHANNEL_MESSAGE_SIZE)
        .is_none());
    drop(permits);
}

#[test]
fn test_inbound_data_channel_count_is_monotonic_and_bounded() {
    let admitted = AtomicUsize::new(0);
    for _ in 0..INBOUND_DATA_CHANNEL_CAPACITY {
        assert!(admit_inbound_data_channel(&admitted));
    }
    assert!(!admit_inbound_data_channel(&admitted));
}
