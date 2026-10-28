use futures::FutureExt;

use super::ApplicationOutlet;
use super::Delivered;
use super::APPLICATION_INBOX_WEIGHT;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::message::CustomMessage;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationDelivery;
use crate::swarm::application::ApplicationMessage;
use crate::swarm::application::SwarmEvent;

/// A fresh peer.
fn peer() -> Did {
    SecretKey::random().address().into()
}

/// A custom message of `len` bytes.
fn message(len: usize) -> ApplicationMessage {
    ApplicationMessage {
        tx_id: uuid::Uuid::new_v4(),
        from: peer(),
        data: ApplicationData::Custom(CustomMessage(vec![0; len])),
    }
}

#[test]
fn test_the_inbox_is_claimed_once() {
    let outlet = ApplicationOutlet::new();
    let _inbox = outlet.claim().unwrap();
    assert!(matches!(
        outlet.claim(),
        Err(Error::ApplicationInboxClaimed)
    ));
}

#[test]
fn test_a_retirement_withdraws_its_unread_admission() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let (kept, withdrawn) = (peer(), peer());
    outlet.announce(SwarmEvent::PeerAdmitted { peer: kept });
    outlet.announce(SwarmEvent::PeerAdmitted { peer: withdrawn });
    outlet.announce(SwarmEvent::PeerRetired { peer: withdrawn });
    assert_eq!(
        inbox.try_next(),
        Some(ApplicationDelivery::Event(SwarmEvent::PeerAdmitted {
            peer: kept
        }))
    );
    assert_eq!(inbox.try_next(), None);
}

#[test]
fn test_a_read_admission_keeps_its_retirement_before_a_readmission() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let peer = peer();
    outlet.announce(SwarmEvent::PeerAdmitted { peer });
    assert!(inbox.try_next().is_some());
    outlet.announce(SwarmEvent::PeerRetired { peer });
    outlet.announce(SwarmEvent::PeerAdmitted { peer });
    outlet.announce(SwarmEvent::PeerRetired { peer });
    outlet.announce(SwarmEvent::PeerAdmitted { peer });
    let read: Vec<_> = std::iter::from_fn(|| inbox.try_next()).collect();
    assert_eq!(read, [
        ApplicationDelivery::Event(SwarmEvent::PeerRetired { peer }),
        ApplicationDelivery::Event(SwarmEvent::PeerAdmitted { peer }),
    ]);
}

/// One order: a peer fact never overtakes an earlier message.
#[test]
fn test_messages_and_facts_are_read_in_the_order_they_were_produced() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let earlier = message(1);
    let peer = earlier.from;
    outlet.deliver(earlier.clone());
    outlet.announce(SwarmEvent::PeerAdmitted { peer });
    assert_eq!(
        inbox.try_next(),
        Some(ApplicationDelivery::Message(earlier))
    );
    assert_eq!(
        inbox.try_next(),
        Some(ApplicationDelivery::Event(SwarmEvent::PeerAdmitted {
            peer
        }))
    );
}

#[test]
fn test_an_unclaimed_inbox_discards_messages_reserves_nothing_and_keeps_facts() {
    let outlet = ApplicationOutlet::new();
    let peer = peer();
    outlet.deliver(message(1));
    assert!(outlet.try_reserve(message(1)).is_none());
    outlet.announce(SwarmEvent::PeerAdmitted { peer });
    let inbox = outlet.claim().unwrap();
    assert_eq!(
        inbox.try_next(),
        Some(ApplicationDelivery::Event(SwarmEvent::PeerAdmitted {
            peer
        }))
    );
    assert_eq!(inbox.try_next(), None);
}

/// A full inbox drops a live message at once and counts it; nothing waits, and reading makes
/// room again.
#[test]
fn test_a_full_inbox_drops_and_counts_a_live_message_without_waiting() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    outlet.deliver(message(APPLICATION_INBOX_WEIGHT));
    outlet.deliver(message(1));
    assert_eq!(inbox.dropped(), 1);
    assert!(matches!(
        inbox.try_next(),
        Some(ApplicationDelivery::Message(_))
    ));
    assert_eq!(inbox.try_next(), None);
    outlet.deliver(message(1));
    assert!(inbox.try_next().is_some());
    assert_eq!(inbox.dropped(), 1);
}

/// An empty message still weighs its own size, so empty messages cannot fill memory unbounded.
///
/// The bound is reached after hundreds of thousands of deliveries, all copies of one message:
/// weight depends on neither sender nor transaction, and a fresh sender per copy would cost a key
/// generation each.
#[test]
fn test_an_empty_message_still_counts_against_the_bound() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let empty = message(0);
    let mut delivered = 0usize;
    while inbox.dropped() == 0 {
        outlet.deliver(empty.clone());
        delivered += 1;
    }
    let queued = std::iter::from_fn(|| inbox.try_next()).count();
    assert_eq!(queued + 1, delivered);
    assert!(queued > 0 && queued <= APPLICATION_INBOX_WEIGHT);
}

/// One weighing rule for every kind of application data: its length, not its allocation.
#[test]
fn test_a_message_weighs_the_length_of_its_data_not_its_capacity() {
    let mut spare = Vec::with_capacity(APPLICATION_INBOX_WEIGHT);
    spare.push(0);
    let roomy = ApplicationMessage {
        data: ApplicationData::Custom(CustomMessage(spare)),
        ..message(0)
    };
    assert_eq!(roomy.weight(), message(1).weight());
}

#[test]
fn test_an_empty_inbox_admits_a_message_heavier_than_its_bound() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    outlet.deliver(message(APPLICATION_INBOX_WEIGHT + 1));
    assert!(inbox.try_next().is_some());
    assert_eq!(inbox.dropped(), 0);
}

#[test]
fn test_a_dropped_reservation_returns_its_room() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    outlet.deliver(message(1));
    let room = outlet
        .try_reserve(message(APPLICATION_INBOX_WEIGHT - 1024))
        .unwrap();
    assert!(outlet.try_reserve(message(1024)).is_none());
    drop(room);
    assert!(outlet
        .try_reserve(message(APPLICATION_INBOX_WEIGHT - 1024))
        .is_some());
    assert!(inbox.try_next().is_some());
}

/// A filled room queues the message it reserved, under the weight it reserved: once taken,
/// the inbox holds no weight and admits a message of its whole bound again.
#[test]
fn test_a_filled_room_queues_its_message_under_its_reserved_weight() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let mail = message(APPLICATION_INBOX_WEIGHT / 2);
    let room = outlet.try_reserve(mail.clone()).unwrap();
    assert_eq!(room.message(), &mail);
    let _taken = room.fill();
    assert!(outlet
        .try_reserve(message(APPLICATION_INBOX_WEIGHT))
        .is_none());
    assert_eq!(inbox.try_next(), Some(ApplicationDelivery::Message(mail)));
    assert!(outlet
        .try_reserve(message(APPLICATION_INBOX_WEIGHT))
        .is_some());
}

/// A message filled into a room fires its consumption signal when the application takes it,
/// and a released inbox drops the signal unfired.
#[test]
fn test_consumption_fires_only_when_the_application_takes_the_message() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let mut taken = outlet.try_reserve(message(1)).unwrap().fill();
    assert_eq!(taken.try_recv(), Ok(None));
    assert!(inbox.try_next().is_some());
    assert_eq!(taken.try_recv(), Ok(Some(())));

    let mut taken = outlet.try_reserve(message(1)).unwrap().fill();
    drop(inbox);
    assert!(taken.try_recv().is_err());
}

/// A room filled after the inbox was released discards its message and cancels its signal.
#[test]
fn test_a_room_filled_after_a_release_cancels_its_signal() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let room = outlet.try_reserve(message(1)).unwrap();
    drop(inbox);
    assert!(room.fill().try_recv().is_err());
}

#[test]
fn test_a_released_inbox_cannot_be_claimed_again() {
    let outlet = ApplicationOutlet::new();
    drop(outlet.claim().unwrap());
    outlet.deliver(message(1));
    assert!(outlet.try_reserve(message(1)).is_none());
    assert!(matches!(
        outlet.claim(),
        Err(Error::ApplicationInboxClaimed)
    ));
}

#[test]
fn test_the_inbox_ends_once_the_outlet_is_gone_and_drained() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    outlet.deliver(message(1));
    drop(outlet);
    assert!(matches!(
        inbox.next().now_or_never(),
        Some(Some(ApplicationDelivery::Message(_)))
    ));
    assert_eq!(inbox.next().now_or_never(), Some(None));
}

/// Law (#969): a drop is told once while one waits after the peer's last admission or
/// retirement, and a drop after a later admission is told again: a waiting drop of an earlier
/// generation does not absorb it.
#[test]
fn test_a_drop_is_told_once_per_generation_while_it_waits() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let peer = peer();
    outlet.announce(SwarmEvent::PeerAdmitted { peer });
    assert!(inbox.try_next().is_some());
    outlet.announce(SwarmEvent::MessageDropped { peer });
    outlet.announce(SwarmEvent::MessageDropped { peer });
    outlet.announce(SwarmEvent::PeerRetired { peer });
    outlet.announce(SwarmEvent::PeerAdmitted { peer });
    outlet.announce(SwarmEvent::MessageDropped { peer });
    outlet.announce(SwarmEvent::MessageDropped { peer });
    let read: Vec<_> = std::iter::from_fn(|| inbox.try_next()).collect();
    assert_eq!(read, [
        ApplicationDelivery::Event(SwarmEvent::MessageDropped { peer }),
        ApplicationDelivery::Event(SwarmEvent::PeerRetired { peer }),
        ApplicationDelivery::Event(SwarmEvent::PeerAdmitted { peer }),
        ApplicationDelivery::Event(SwarmEvent::MessageDropped { peer }),
    ]);
    outlet.announce(SwarmEvent::MessageDropped { peer });
    assert_eq!(
        inbox.try_next(),
        Some(ApplicationDelivery::Event(SwarmEvent::MessageDropped {
            peer
        }))
    );
}

/// Law (#969): a retirement that withdraws an unread admission withdraws the drops after it, so
/// the application never reads a drop of an admission it did not read; other peers' facts and
/// messages stay.
#[test]
fn test_a_withdrawn_admission_takes_its_drops_with_it() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let (kept, withdrawn) = (peer(), peer());
    let earlier = message(1);
    outlet.announce(SwarmEvent::PeerAdmitted { peer: withdrawn });
    outlet.announce(SwarmEvent::MessageDropped { peer: withdrawn });
    outlet.announce(SwarmEvent::PeerAdmitted { peer: kept });
    outlet.deliver(earlier.clone());
    outlet.announce(SwarmEvent::MessageDropped { peer: kept });
    outlet.announce(SwarmEvent::PeerRetired { peer: withdrawn });
    let read: Vec<_> = std::iter::from_fn(|| inbox.try_next()).collect();
    assert_eq!(read, [
        ApplicationDelivery::Event(SwarmEvent::PeerAdmitted { peer: kept }),
        ApplicationDelivery::Message(earlier),
        ApplicationDelivery::Event(SwarmEvent::MessageDropped { peer: kept }),
    ]);
}

/// `deliver` reports what it did, so its caller can tell a drop (#969).
#[test]
fn test_delivery_reports_whether_a_message_was_queued_discarded_or_dropped() {
    let outlet = ApplicationOutlet::new();
    assert_eq!(outlet.deliver(message(1)), Delivered::Discarded);
    let _inbox = outlet.claim().unwrap();
    assert_eq!(
        outlet.deliver(message(APPLICATION_INBOX_WEIGHT)),
        Delivered::Queued
    );
    assert_eq!(outlet.deliver(message(1)), Delivered::Dropped);
}

/// One step of an inbox equivalence trace: a fact of a peer, or a read.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// `SwarmEvent::PeerAdmitted` of peer `0` or `1`.
    Admit(usize),
    /// `SwarmEvent::PeerRetired` of the peer.
    Retire(usize),
    /// `SwarmEvent::MessageDropped` of the peer.
    Drop(usize),
    /// The application reads one item.
    Take,
}

/// The facts as the queue scanned them before #938: the reference the indexed queue must equal.
#[derive(Default)]
struct ScannedFacts(std::collections::VecDeque<SwarmEvent>);

impl ScannedFacts {
    /// The position of `peer`'s last queued admission or retirement, by a scan.
    fn last_lifecycle(&self, peer: Did) -> Option<usize> {
        self.0.iter().rposition(|event| match event {
            SwarmEvent::PeerAdmitted { peer: fact } | SwarmEvent::PeerRetired { peer: fact } => {
                *fact == peer
            }
            SwarmEvent::MessageDropped { .. } => false,
        })
    }

    /// Record `event` by scanning the queue.
    fn record(&mut self, event: SwarmEvent) {
        match event {
            SwarmEvent::PeerRetired { peer } => {
                let unread = self
                    .last_lifecycle(peer)
                    .filter(|at| matches!(self.0[*at], SwarmEvent::PeerAdmitted { .. }));
                if let Some(at) = unread {
                    let dropped = SwarmEvent::MessageDropped { peer };
                    let after = self.0.split_off(at);
                    self.0
                        .extend(after.into_iter().skip(1).filter(|fact| *fact != dropped));
                    return;
                }
            }
            SwarmEvent::MessageDropped { peer } => {
                let since = self.last_lifecycle(peer).map_or(0, |at| at + 1);
                if self.0.iter().skip(since).any(|fact| *fact == event) {
                    return;
                }
            }
            SwarmEvent::PeerAdmitted { .. } => {}
        }
        self.0.push_back(event);
    }
}

/// Run `steps` on a fresh inbox and on the scanning reference, asserting every read and the
/// final contents equal.
fn assert_equivalent(steps: &[Step], peers: [Did; 2]) {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let mut reference = ScannedFacts::default();
    let read = |delivery: Option<ApplicationDelivery>| match delivery {
        Some(ApplicationDelivery::Event(event)) => Some(event),
        Some(ApplicationDelivery::Message(_)) => panic!("{steps:?} queued no message"),
        None => None,
    };
    for step in steps {
        let event = match *step {
            Step::Admit(peer) => SwarmEvent::PeerAdmitted { peer: peers[peer] },
            Step::Retire(peer) => SwarmEvent::PeerRetired { peer: peers[peer] },
            Step::Drop(peer) => SwarmEvent::MessageDropped { peer: peers[peer] },
            Step::Take => {
                assert_eq!(read(inbox.try_next()), reference.0.pop_front(), "{steps:?}");
                continue;
            }
        };
        outlet.announce(event);
        reference.record(event);
    }
    let rest = std::iter::from_fn(|| read(inbox.try_next())).collect::<Vec<_>>();
    assert_eq!(rest, Vec::from(reference.0), "{steps:?}");
}

/// The indexed queue records facts exactly as the scanning queue did, over every trace of up
/// to six steps of two peers: indexing changed the cost, not one delivery.
#[test]
fn test_indexed_facts_equal_the_scanning_reference() {
    const STEPS: [Step; 7] = [
        Step::Admit(0),
        Step::Admit(1),
        Step::Retire(0),
        Step::Retire(1),
        Step::Drop(0),
        Step::Drop(1),
        Step::Take,
    ];
    let peers = [peer(), peer()];
    let mut traces = vec![Vec::new()];
    for _ in 0..6 {
        traces = traces
            .into_iter()
            .flat_map(|trace: Vec<Step>| {
                STEPS.iter().map(move |step| {
                    let mut longer = trace.clone();
                    longer.push(*step);
                    longer
                })
            })
            .collect();
        traces
            .iter()
            .for_each(|trace| assert_equivalent(trace, peers));
    }
}
