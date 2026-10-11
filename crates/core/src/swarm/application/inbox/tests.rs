use futures::FutureExt;

use super::ApplicationOutlet;
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

/// Queue `message` through a reservation if it fits now; returns whether room was found.
fn try_queue(outlet: &ApplicationOutlet, message: ApplicationMessage) -> bool {
    match outlet.try_reserve(&message) {
        Some(room) => {
            room.fill(message);
            true
        }
        None => false,
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

#[test]
fn test_an_unclaimed_inbox_discards_messages_and_keeps_events() {
    let outlet = ApplicationOutlet::new();
    let peer = peer();
    assert!(try_queue(&outlet, message(1)));
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

#[test]
fn test_a_full_inbox_holds_deliver_until_the_application_reads() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    assert!(try_queue(&outlet, message(APPLICATION_INBOX_WEIGHT)));
    assert!(!try_queue(&outlet, message(1)));
    let mut waiting = Box::pin(outlet.deliver(message(1)));
    assert!(waiting.as_mut().now_or_never().is_none());
    assert!(matches!(
        inbox.try_next(),
        Some(ApplicationDelivery::Message(_))
    ));
    assert!(waiting.as_mut().now_or_never().is_some());
    assert!(matches!(
        inbox.try_next(),
        Some(ApplicationDelivery::Message(_))
    ));
}

#[test]
fn test_an_empty_inbox_admits_a_message_heavier_than_its_bound() {
    let outlet = ApplicationOutlet::new();
    let _inbox = outlet.claim().unwrap();
    assert!(try_queue(&outlet, message(APPLICATION_INBOX_WEIGHT + 1)));
}

#[test]
fn test_a_released_inbox_frees_a_waiting_deliver() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    assert!(try_queue(&outlet, message(APPLICATION_INBOX_WEIGHT)));
    let mut waiting = Box::pin(outlet.deliver(message(1)));
    assert!(waiting.as_mut().now_or_never().is_none());
    drop(inbox);
    assert!(waiting.as_mut().now_or_never().is_some());
    assert!(matches!(
        outlet.claim(),
        Err(Error::ApplicationInboxClaimed)
    ));
}

#[test]
fn test_the_inbox_ends_once_the_outlet_is_gone_and_drained() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    assert!(try_queue(&outlet, message(1)));
    drop(outlet);
    assert!(matches!(
        inbox.next().now_or_never(),
        Some(Some(ApplicationDelivery::Message(_)))
    ));
    assert_eq!(inbox.next().now_or_never(), Some(None));
}

#[test]
fn test_a_dropped_reservation_returns_its_room() {
    let outlet = ApplicationOutlet::new();
    let inbox = outlet.claim().unwrap();
    assert!(try_queue(&outlet, message(1)));
    let held = message(APPLICATION_INBOX_WEIGHT - 1);
    let room = outlet.try_reserve(&held).unwrap();
    assert!(outlet.try_reserve(&message(1)).is_none());
    drop(room);
    assert!(outlet.try_reserve(&held).is_some());
    assert!(inbox.try_next().is_some());
}

#[test]
fn test_room_reserved_before_the_claim_discards_its_message() {
    let outlet = ApplicationOutlet::new();
    let unowed = message(1);
    let room = outlet.try_reserve(&unowed).unwrap();
    let inbox = outlet.claim().unwrap();
    room.fill(unowed);
    assert_eq!(inbox.try_next(), None);
}
