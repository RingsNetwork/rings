//! Law Fail closed of the exit session (#834 D2′ `abort`), end to end over the fixture loop: the
//! frame a failing exit replies is the `abort(n)` the client opens, never a `fin`, and a session
//! that closed with no block left is answered with `abort` at the client's next loop.

use futures::channel::mpsc;

use super::arguments;
use super::Fixture;
use super::NOW_MS;
use crate::onion::circuit::hop::OnionHopOutcome;
use crate::onion::circuit::OnionClientTags;
use crate::onion::circuit::OnionReply;
use crate::onion::session::client::OnionClientEvent;
use crate::onion::session::client::OnionClientSession;
use crate::onion::session::exit::reply_to_unheld;
use crate::onion::session::exit::seal;
use crate::onion::session::exit::OnionExitEffect;
use crate::onion::session::exit::OnionExitSession;
use crate::onion::session::frame::OnionFrame;
use crate::onion::session::frame::OnionSequence;
use crate::onion::sphinx::builder::OnionReplyKey;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::class::OnionLoopClass;

/// The fixture session's opening frame: `data(0, T, t, ε)`.
fn opening() -> OnionFrame {
    OnionFrame::Data {
        sequence: OnionSequence::FIRST,
        target: Some(bytes::Bytes::from_static(b"example.com:443")),
        payload: bytes::Bytes::new(),
    }
}

/// Run `count` seeded loops to `h`: the reply blocks `h` holds and the reply keys the client
/// keeps, in loop order.
fn delivered(fixture: &mut Fixture, count: u64) -> (Vec<OnionSurb>, Vec<OnionReplyKey>) {
    (0..count)
        .map(|seed| {
            let built = fixture.build(40 + seed, b"value");
            let cell = fixture.run_forward_segment(built.cell);
            let from = fixture.hops[1].did();
            let OnionHopOutcome::Consumed { surb, .. } = fixture.hops[2].step(from, cell) else {
                panic!("h consumes");
            };
            (*surb, built.reply)
        })
        .unzip()
}

/// The frames the client opens from the `Reply` effects of `effects`, spending the reply keys
/// of `keys` in order, and whether `effects` ends in `Close`.
fn opened_replies(
    fixture: &mut Fixture,
    effects: Vec<OnionExitEffect>,
    keys: &mut impl Iterator<Item = OnionReplyKey>,
) -> (Vec<Vec<u8>>, bool) {
    let guard = fixture.hops[0].did();
    let closes = matches!(effects.last(), Some(OnionExitEffect::Close));
    let frames = effects
        .into_iter()
        .filter_map(|effect| match effect {
            OnionExitEffect::Reply { frame, surb } => {
                Some(seal(&frame, *surb).expect("a sealable reply").1)
            }
            _ => None,
        })
        .map(|cell| {
            let reply = keys.next().expect("a reply key per reply");
            let tag = reply.tag;
            let tags = OnionClientTags::default();
            let (sink, mut replies) = mpsc::channel::<OnionReply>(1);
            tags.register(NOW_MS, guard, reply, sink).expect("register");
            let cell = fixture.run_return_segment(cell);
            tags.deliver(NOW_MS, guard, &tag, cell).expect("authentic");
            replies
                .try_recv()
                .expect("a reply")
                .frame
                .encode(OnionLoopClass::DEFAULT)
                .expect("fits")
                .to_vec()
        })
        .collect();
    (frames, closes)
}

/// `enc(frame)` in the default class.
fn encoded(frame: OnionFrame) -> Vec<u8> {
    frame
        .encode(OnionLoopClass::DEFAULT)
        .expect("fits")
        .to_vec()
}

/// A refused open replies `abort(0)` and closes: the client learns the refusal as a failure.
#[test]
fn test_a_refused_open_replies_abort() {
    let mut fixture = Fixture::new();
    let (mut blocks, keys) = delivered(&mut fixture, 1);
    let mut session = OnionExitSession::new(arguments().digest, NOW_MS);
    session.forward(NOW_MS, opening(), blocks.remove(0));

    let effects = session.opened(NOW_MS, false);
    let (frames, closes) = opened_replies(&mut fixture, effects, &mut keys.into_iter());

    assert_eq!(frames, [encoded(OnionFrame::Abort {
        sequence: OnionSequence::FIRST
    })]);
    assert!(closes);
}

/// A world failure after the ack replies `abort(1)`, the next reply sequence, and closes; the
/// world bytes it held are dropped, so the client is never handed a truncated stream as whole.
#[test]
fn test_a_world_failure_replies_abort_never_fin() {
    let mut fixture = Fixture::new();
    let (mut blocks, keys) = delivered(&mut fixture, 2);
    let mut keys = keys.into_iter();
    let mut session = OnionExitSession::new(arguments().digest, NOW_MS);
    session.forward(NOW_MS, opening(), blocks.remove(0));
    let empty = OnionFrame::Data {
        sequence: OnionSequence::new(1),
        target: None,
        payload: bytes::Bytes::new(),
    };
    // The pool spends blocks of one expiry in arrival order, so replies spend `keys` in order.
    session.forward(NOW_MS, empty, blocks.remove(0));
    let (acked, _) = opened_replies(&mut fixture, session.opened(NOW_MS, true), &mut keys);
    assert_eq!(acked, [encoded(OnionFrame::Data {
        sequence: OnionSequence::FIRST,
        target: None,
        payload: bytes::Bytes::new(),
    })]);

    let (frames, closes) = opened_replies(&mut fixture, session.fail(NOW_MS), &mut keys);

    assert_eq!(frames, [encoded(OnionFrame::Abort {
        sequence: OnionSequence::new(1)
    })]);
    assert!(closes);
}

/// Liveness of failure (#895 R3-M2): a world failure with no block left closes the session
/// without a reply; the client's next loop is answered with `abort` under that loop's own block,
/// and the client reads it as `Aborted`, never as the end of its stream.
#[test]
fn test_a_failure_without_a_block_is_answered_at_the_next_loop() {
    let mut fixture = Fixture::new();
    let (mut blocks, keys) = delivered(&mut fixture, 2);
    let mut keys = keys.into_iter();
    let mut session = OnionExitSession::new(arguments().digest, NOW_MS);
    session.forward(NOW_MS, opening(), blocks.remove(0));
    let (acked, _) = opened_replies(&mut fixture, session.opened(NOW_MS, true), &mut keys);
    let mut client = OnionClientSession::new(bytes::Bytes::from_static(b"example.com:443"));
    let ack = OnionFrame::decode(OnionLoopClass::DEFAULT, &acked[0]).expect("a frame");
    assert_eq!(
        client.reply(NOW_MS, ack),
        Ok(vec![OnionClientEvent::Opened])
    );

    let closed = session.fail(NOW_MS);
    assert!(
        matches!(closed.as_slice(), [OnionExitEffect::Close]),
        "no block is left, so no reply"
    );
    // The client's `V/2` keep-alive: a credit frame, whose blocks the answer does not read.
    let keep_alive = OnionFrame::Credit(Vec::new());
    let answer = reply_to_unheld(&keep_alive).expect("an unheld loop is answered");
    let reply = vec![OnionExitEffect::Reply {
        frame: answer,
        surb: Box::new(blocks.remove(0)),
    }];
    let (frames, _) = opened_replies(&mut fixture, reply, &mut keys);
    let abort = OnionFrame::decode(OnionLoopClass::DEFAULT, &frames[0]).expect("a frame");

    assert_eq!(
        client.reply(NOW_MS, abort),
        Ok(vec![OnionClientEvent::Aborted])
    );
    assert!(
        reply_to_unheld(&OnionFrame::Abort {
            sequence: OnionSequence::FIRST
        })
        .is_none(),
        "a client that gave up is not answered"
    );
}
