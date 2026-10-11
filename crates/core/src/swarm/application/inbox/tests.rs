use futures::channel::oneshot;
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
    assert_eq!(outlet.deliver(earlier.clone()), Delivered::Queued);
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
    assert_eq!(outlet.deliver(message(1)), Delivered::Discarded);
    assert!(outlet.try_reserve(&message(1)).is_none());
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
    assert_eq!(
        outlet.deliver(message(APPLICATION_INBOX_WEIGHT)),
        Delivered::Queued
    );
    assert_eq!(outlet.deliver(message(1)), Delivered::Dropped);
    assert_eq!(inbox.dropped(), 1);
    assert!(matches!(
        inbox.try_next(),
        Some(ApplicationDelivery::Message(_))
    ));
    assert_eq!(outlet.deliver(message(1)), Delivered::Queued);
}

/// An empty message still weighs its own size, so empty messages cannot fill memory unbounded
/// (#913 R10 M1).
#[test]
fn test_an_empty_message_still_counts_against_the_bound() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let mut queued = 0usize;
    while outlet.deliver(message(0)) == Delivered::Queued {
        queued += 1;
    }
    assert!(queued > 0 && queued <= APPLICATION_INBOX_WEIGHT);
    assert_eq!(inbox.dropped(), 1);
}

#[test]
fn test_an_empty_inbox_admits_a_message_heavier_than_its_bound() {
    let outlet = ApplicationOutlet::new();
    let _inbox = outlet.claim().unwrap();
    assert_eq!(
        outlet.deliver(message(APPLICATION_INBOX_WEIGHT + 1)),
        Delivered::Queued
    );
}

#[test]
fn test_a_dropped_reservation_returns_its_room() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    assert_eq!(outlet.deliver(message(1)), Delivered::Queued);
    let held = message(APPLICATION_INBOX_WEIGHT - 1024);
    let room = outlet.try_reserve(&held).unwrap();
    assert!(outlet.try_reserve(&message(1024)).is_none());
    drop(room);
    assert!(outlet.try_reserve(&held).is_some());
    assert!(inbox.try_next().is_some());
}

/// A message filled into a room fires its consumption signal when the application takes it,
/// and a released inbox drops the signal unfired (#913 R10 H3).
#[test]
fn test_consumption_fires_only_when_the_application_takes_the_message() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    let mail = message(1);
    let (consumed, mut taken) = oneshot::channel();
    assert!(outlet
        .try_reserve(&mail)
        .unwrap()
        .fill(mail, Some(consumed)));
    assert_eq!(taken.try_recv(), Ok(None));
    assert!(inbox.try_next().is_some());
    assert_eq!(taken.try_recv(), Ok(Some(())));

    let mail = message(1);
    let (consumed, mut taken) = oneshot::channel();
    assert!(outlet
        .try_reserve(&mail)
        .unwrap()
        .fill(mail, Some(consumed)));
    drop(inbox);
    assert!(taken.try_recv().is_err());
}

#[test]
fn test_a_released_inbox_cannot_be_claimed_again() {
    let outlet = ApplicationOutlet::new();
    drop(outlet.claim().unwrap());
    assert_eq!(outlet.deliver(message(1)), Delivered::Discarded);
    assert!(matches!(
        outlet.claim(),
        Err(Error::ApplicationInboxClaimed)
    ));
}

#[test]
fn test_the_inbox_ends_once_the_outlet_is_gone_and_drained() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    assert_eq!(outlet.deliver(message(1)), Delivered::Queued);
    drop(outlet);
    assert!(matches!(
        inbox.next().now_or_never(),
        Some(Some(ApplicationDelivery::Message(_)))
    ));
    assert_eq!(inbox.next().now_or_never(), Some(None));
}
