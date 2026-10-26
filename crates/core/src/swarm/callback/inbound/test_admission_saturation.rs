//! The inbound queue under saturation (load-bounds paper, Obligation "Admission complexity",
//! #938): an event visits `O((g + r + 1) log n)` waiters, `g` the waiters it grants, `r` the
//! resources it names and `n` the waiters queued, never the whole queue.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;

use futures::FutureExt;
use rings_transport::core::pool::ChannelLane;
use rings_transport::hot_path;
use rings_transport::hot_path::HotPath;

use super::*;

/// The peers arriving.
const PEERS: u32 = 64;
/// The arrivals of each peer: the mailbox admits about four of each, the rest wait.
const ARRIVALS_PER_PEER: usize = 20;
/// The resources a frame's release names: its peer, its lane and the shared budget.
const NAMED: u64 = 3;
/// The constant `c` of the bound `c · (g + r + 1) · log₂ n`.
const BOUND_FACTOR: u64 = 2;

/// An acquisition, admitted or waiting.
type Acquisition<'a> = Pin<Box<dyn Future<Output = Result<InboundCapacityPermit>> + 'a>>;

/// The `index`-th arrival of `peer`: lanes, carriers and sizes spread over the peers, so the
/// queue holds every kind of waiter (blocked by a stream, by a peer, by a refusal, holding a
/// reservation) at once.
fn arrive(capacity: &Arc<InboundCapacity>, peer: u32, index: usize) -> Acquisition<'_> {
    let spread = usize::try_from(peer).expect("test peer fits") + index;
    let lane = InboundLane::ALL[spread % INBOUND_LANE_COUNT];
    let carrier = ChannelLane::new(u8::try_from(spread % 4).expect("carrier fits"));
    let bytes = 1 + (spread * 7_919) % 65_536;
    Box::pin(capacity.acquire(
        (Some(Did::from(peer + 1)), lane, Some(carrier)),
        InboundCharge::Bytes(memory_reservation(bytes)),
    ))
}

/// `⌈log₂ n⌉`, at least one.
fn log2(n: usize) -> u64 {
    u64::from(n.max(2).next_power_of_two().trailing_zeros())
}

/// About a thousand waiters of 64 peers drain one release at a time; each release visits at
/// most `c · (g + r + 1) · log₂ n` waiters, and every waiter is eventually admitted. The
/// reference pass, which judged the whole queue on every event, fails the bound by two orders.
#[test]
fn test_saturated_queue_visits_only_what_an_event_may_unblock() {
    let capacity = Arc::new(InboundCapacity::new());
    let mut held = VecDeque::new();
    let mut waiting = Vec::new();
    for index in 0..ARRIVALS_PER_PEER {
        for peer in 0..PEERS {
            let mut acquisition = arrive(&capacity, peer, index);
            match acquisition.as_mut().now_or_never() {
                Some(permit) => held.push_back(permit.expect("within the request bounds")),
                None => waiting.push(acquisition),
            }
        }
    }
    assert!(waiting.len() >= 1_000, "{} waiters", waiting.len());

    let mut worst = 0_u64;
    while let Some(permit) = held.pop_front() {
        let queued = waiting.len();
        let before = hot_path::count(HotPath::QueueService);
        drop(permit);
        let after = hot_path::count(HotPath::QueueService);
        let mut granted = 0_u64;
        waiting.retain_mut(|acquisition| match acquisition.as_mut().now_or_never() {
            Some(permit) => {
                held.push_back(permit.expect("within the request bounds"));
                granted += 1;
                false
            }
            None => true,
        });
        assert_eq!(after.events - before.events, 1, "one release is one event");
        let visits = after.operations - before.operations;
        let bound = BOUND_FACTOR * (granted + NAMED + 1) * log2(queued);
        assert!(
            visits <= bound,
            "{visits} visits granting {granted} of {queued} exceed the bound {bound}"
        );
        worst = worst.max(visits * 1_000 / bound);
    }
    assert!(
        waiting.is_empty(),
        "{} waiters were never admitted",
        waiting.len()
    );
    println!("worst visits per event: {worst}‰ of the bound");
}
