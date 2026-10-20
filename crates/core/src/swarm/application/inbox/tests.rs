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
#[test]
fn test_an_empty_message_still_counts_against_the_bound() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let mut delivered = 0usize;
    while inbox.dropped() == 0 {
        outlet.deliver(message(0));
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
