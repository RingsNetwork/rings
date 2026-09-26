//! Laws of the client's tag table (`circuit::tags`): single use, expiry, and reply
//! authentication.

use bytes::Bytes;
use futures::channel::mpsc;
use rings_core::dht::Did;

use super::super::tags::OnionReplyDropped;
use super::super::OnionClientTags;
use super::super::OnionReply;
use super::expiry;
use super::Fixture;
use super::NOW_MS;
use crate::onion::circuit::hop::OnionHopOutcome;
use crate::onion::session::frame::OnionFrame;
use crate::onion::session::frame::OnionSequence;
use crate::onion::sphinx::builder::OnionReplyKey;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::class::OnionLoopClass;

/// The reply frame `h` sends in the fixture.
fn reply_frame() -> OnionFrame {
    OnionFrame::Data {
        sequence: OnionSequence::FIRST,
        target: None,
        payload: Bytes::from_static(b"reply"),
    }
}

/// Run a fixture loop to `h`, reply with [`reply_frame`], and return the loop's reply key and
/// the cell the client receives from its guard, [`guard`].
fn returned(seed: u64) -> (OnionReplyKey, OnionCell) {
    let mut fixture = Fixture::new();
    let built = fixture.build(seed, b"value");
    let cell = fixture.run_forward_segment(built.cell);
    let from = fixture.hops[1].did();
    let OnionHopOutcome::Consumed { surb, .. } = fixture.hops[2].step(from, cell) else {
        panic!("h consumes");
    };
    let frame = reply_frame().encode(OnionLoopClass::DEFAULT).expect("fits");
    let (_, reply) = surb.produce(frame.as_slice()).expect("produce");
    (built.reply, fixture.run_return_segment(reply))
}

/// The fixture loop's guard, the peer every returning cell arrives from.
fn guard() -> Did {
    Fixture::new().hops[0].did()
}

/// A tag table with the reply key of `reply` registered at [`NOW_MS`], and its session's queue.
fn registered(reply: OnionReplyKey) -> (OnionClientTags, mpsc::Receiver<OnionReply>) {
    let tags = OnionClientTags::default();
    let (sink, replies) = mpsc::channel(4);
    tags.register(NOW_MS, guard(), reply, sink)
        .expect("register");
    (tags, replies)
}

/// Single use: the reply is delivered once, decoded, and its replay finds no entry.
#[test]
fn test_a_reply_is_delivered_once() {
    let (reply, cell) = returned(10);
    let tag = reply.tag;
    let bytes = cell.into_bytes();
    let cell = OnionCell::parse(&bytes).expect("width");
    let replay = OnionCell::parse(&bytes).expect("width");
    let (tags, mut replies) = registered(reply);

    assert!(tags.expects(guard(), &tag));
    assert_eq!(tags.deliver(NOW_MS, guard(), &tag, cell), Ok(()));
    let OnionReply {
        frame,
        received_at_ms,
    } = replies.try_recv().expect("a reply");
    assert_eq!(received_at_ms, NOW_MS);
    assert_eq!(
        frame.encode(OnionLoopClass::DEFAULT),
        reply_frame().encode(OnionLoopClass::DEFAULT)
    );
    assert!(!tags.expects(guard(), &tag));
    assert_eq!(
        tags.deliver(NOW_MS, guard(), &tag, replay),
        Err(OnionReplyDropped::UnknownTag)
    );
}

/// Reply authentication (#834 Prop. Reply authentication): flipping any one bit of the carry
/// makes the reply inauthentic, and the spent entry is gone either way.
#[test]
fn test_a_one_bit_flip_of_the_carry_is_inauthentic() {
    let width = OnionLoopClass::DEFAULT.cell_bytes();
    for position in [2919, 2919 + width / 3, width - 1] {
        let (reply, cell) = returned(11);
        let tag = reply.tag;
        let mut bytes = cell.into_bytes();
        bytes[position] ^= 1;
        let (tags, _replies) = registered(reply);

        assert_eq!(
            tags.deliver(
                NOW_MS,
                guard(),
                &tag,
                OnionCell::parse(&bytes).expect("width")
            ),
            Err(OnionReplyDropped::Inauthentic)
        );
        assert!(!tags.expects(guard(), &tag));
    }
}

/// Expiry: an entry whose `x` has passed delivers nothing, and `purge` leaves no such entry.
#[test]
fn test_expired_entries_deliver_nothing_and_are_purged() {
    let (reply, cell) = returned(12);
    let tag = reply.tag;
    let x = expiry().as_ms();
    let (tags, _replies) = registered(reply);

    tags.purge(x - 1);
    assert_eq!(tags.len(), 1);
    assert_eq!(
        tags.deliver(x, guard(), &tag, cell),
        Err(OnionReplyDropped::UnknownTag)
    );

    let (reply, _) = returned(13);
    let (tags, _replies) = registered(reply);
    tags.purge(x);
    assert_eq!(tags.len(), 0);
}

/// A reply for a session whose queue is gone is dropped, and its entry spent.
#[test]
fn test_a_reply_to_a_gone_session_is_dropped() {
    let (reply, cell) = returned(14);
    let tag = reply.tag;
    let (tags, replies) = registered(reply);
    drop(replies);

    assert_eq!(
        tags.deliver(NOW_MS, guard(), &tag, cell),
        Err(OnionReplyDropped::SessionGone)
    );
    assert!(!tags.expects(guard(), &tag));
}

/// Guard binding: a cell with a live tag from any peer but the loop's guard is not the client's,
/// and it leaves the entry for the real reply.
#[test]
fn test_a_tag_from_another_peer_is_not_the_clients() {
    let (reply, cell) = returned(15);
    let tag = reply.tag;
    let (tags, _replies) = registered(reply);
    let other = Did::from(7_u32);

    assert!(!tags.expects(other, &tag));
    let bytes = cell.into_bytes();
    assert_eq!(
        tags.deliver(
            NOW_MS,
            other,
            &tag,
            OnionCell::parse(&bytes).expect("width")
        ),
        Err(OnionReplyDropped::UnknownTag)
    );
    assert!(tags.expects(guard(), &tag));
    assert_eq!(
        tags.deliver(
            NOW_MS,
            guard(),
            &tag,
            OnionCell::parse(&bytes).expect("width")
        ),
        Ok(())
    );
}
