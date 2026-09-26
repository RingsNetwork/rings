//! Law tests of the session algebra (#834 D2′, D8; #843 Q2, Q5, Q7), one section per module.
//!
//! Reply blocks are built by the client's builder over a fixture return path, so the pool and the
//! machines handle real `OnionSurb`s; nothing here touches a network or a clock.

use bytes::Bytes;
use rand::rngs::StdRng;
use rand::SeedableRng;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;

use super::client::keep_alive_due;
use super::client::OnionClientCredit;
use super::client::OnionClientEvent;
use super::client::OnionClientSession;
use super::client::OnionCreditWindow;
use super::exit::OnionExitEffect;
use super::exit::OnionExitSession;
use super::exit::OnionWorldRead;
use super::frame::OnionFrame;
use super::frame::OnionFrameError;
use super::frame::OnionSequence;
use super::order::OnionReorder;
use super::order::OnionSequenceGap;
use super::pool::OnionSurbPool;
use super::pool::ONION_SURB_POOL_CAPACITY;
use super::OnionSessionArguments;
use super::OnionSessionId;
use super::OnionTargetDigest;
use crate::onion::circuit::OnionExpiry;
use crate::onion::circuit::ONION_FORWARD_MAX_VALIDITY_MS;
use crate::onion::sphinx::builder::build_surb;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRouteHop;

/// The arrival instant of the fixtures, `2Q`.
const NOW_MS: u128 = 60_000;

/// The quantum `Q`.
const QUANTUM_MS: u128 = 30_000;

/// The target of the fixture sessions.
const TARGET: &[u8] = b"example.com:443";

/// The class of every fixture loop.
const CLASS: OnionLoopClass = OnionLoopClass::DEFAULT;

/// The fixture return path: two relays with fixed keys.
fn return_path() -> [OnionRouteHop; 2] {
    [1_u8, 2].map(|byte| {
        let secret =
            SecretKey::try_from(format!("{byte:02x}").repeat(32).as_str()).expect("fixture scalar");
        let key = DelegateeKey::new_with_seckey(&secret).expect("fixture delegation");
        OnionRouteHop::new(
            key.delegator_did(),
            key.delegatee_public_key(),
            OnionProcessEpoch::new([1; 16]),
        )
    })
}

/// `count` reply blocks expiring at `(3 + offset)·Q`, from a seeded RNG.
fn surbs(seed: u64, count: usize, offset: u128) -> Vec<OnionSurb> {
    let mut rng = StdRng::seed_from_u64(seed);
    let expiry = OnionExpiry::from_ms((3 + offset) * QUANTUM_MS).expect("on the grid");
    (0..count)
        .map(|_| {
            build_surb(
                return_path().iter(),
                Did::from(9_u32),
                CLASS,
                expiry,
                &mut rng,
            )
            .expect("a reply block")
            .0
        })
        .collect()
}

/// One reply block expiring at `3Q`.
fn surb(seed: u64) -> OnionSurb {
    surbs(seed, 1, 0).pop().expect("one block")
}

/// The fixture session arguments.
fn arguments() -> OnionSessionArguments {
    OnionSessionArguments {
        session: OnionSessionId([5; 16]),
        digest: OnionTargetDigest::of(TARGET),
    }
}

/// The encoding round trip of `frame`: `decode ∘ encode` and, for the result, `encode` again.
fn round_trip(frame: &OnionFrame) -> Vec<u8> {
    let bytes = frame.encode(CLASS).expect("the frame fits").to_vec();
    let decoded = OnionFrame::decode(CLASS, &bytes).expect("decodes");
    assert_eq!(
        decoded.encode(CLASS).expect("re-encodes").as_slice(),
        bytes.as_slice(),
        "canonical"
    );
    bytes
}

// ---- frame -------------------------------------------------------------------------------------

/// Every frame round-trips canonically, the widest `data` is exactly `C₀ − 7` bytes of stream
/// (`C₀ − 9 − |t|` under `T`), and a `credit` frame holds exactly `k = 4` blocks at 16 KiB.
#[test]
fn test_frames_round_trip_at_their_width_bounds() {
    let capacity = OnionFrame::data_capacity(CLASS);
    assert_eq!(capacity, CLASS.value_capacity() - 6);
    let widest = Bytes::from(vec![0xa5; capacity]);
    round_trip(&OnionFrame::Data {
        sequence: OnionSequence::new(7),
        target: None,
        payload: widest.clone(),
    });
    round_trip(&OnionFrame::Data {
        sequence: OnionSequence::FIRST,
        target: Some(Bytes::from_static(TARGET)),
        payload: Bytes::from(vec![1; capacity - 2 - TARGET.len()]),
    });
    round_trip(&OnionFrame::Fin {
        sequence: OnionSequence::new(u32::MAX),
    });
    let credit = OnionFrame::Credit(surbs(10, OnionFrame::credit_capacity(CLASS), 0));
    assert_eq!(OnionFrame::credit_capacity(CLASS), 4);
    round_trip(&credit);

    let overwide = OnionFrame::Data {
        sequence: OnionSequence::FIRST,
        target: None,
        payload: Bytes::from(vec![0; capacity + 1]),
    };
    assert!(overwide.encode(CLASS).is_err());
    assert!(OnionFrame::Credit(surbs(11, 5, 0)).encode(CLASS).is_err());
}

/// Closure: unknown tags, reserved flags, truncated fields, trailing bytes after `fin`, and
/// `credit` frames of no or partial blocks are rejected.
#[test]
fn test_malformed_frames_are_rejected() {
    let fin = OnionFrame::Fin {
        sequence: OnionSequence::FIRST,
    }
    .encode(CLASS)
    .expect("fits");
    let mut credit = OnionFrame::Credit(surbs(12, 1, 0))
        .encode(CLASS)
        .expect("fits")
        .to_vec();
    credit.pop();
    for (bytes, error) in [
        (Vec::new(), OnionFrameError::Tag),
        (vec![0x03], OnionFrameError::Tag),
        (vec![0x00, 0, 0, 0, 0, 0x02], OnionFrameError::Flags(0x02)),
        (vec![0x00, 0, 0, 0], OnionFrameError::Malformed),
        (
            vec![0x00, 0, 0, 0, 0, 0x01, 0, 9, b'x'],
            OnionFrameError::Malformed,
        ),
        ([fin.as_slice(), &[0]].concat(), OnionFrameError::Malformed),
        (vec![0x02], OnionFrameError::Malformed),
        (credit, OnionFrameError::Malformed),
    ] {
        assert_eq!(
            OnionFrame::decode(CLASS, &bytes).err(),
            Some(error),
            "{bytes:02x?}"
        );
    }
}

/// `ā = ς ‖ d ‖ 0^16` round-trips, and a non-zero padding byte is not a session application.
#[test]
fn test_session_arguments_round_trip_canonically() {
    let encoded = arguments().encode();
    assert_eq!(OnionSessionArguments::decode(&encoded), Some(arguments()));
    let mut bytes = *encoded.as_bytes();
    bytes[63] = 1;
    assert_eq!(
        OnionSessionArguments::decode(&crate::onion::sphinx::layer::OnionArguments::new(bytes)),
        None
    );
}

// ---- pool --------------------------------------------------------------------------------------

/// The pool holds at most `Q_max` blocks, spends the least expiry first, never returns a block
/// twice, and drops a block exactly when its `x` passes.
#[test]
fn test_surb_pool_is_bounded_ordered_and_single_use() {
    let mut pool = OnionSurbPool::default();
    let late = surbs(20, 1, 2).pop().expect("a block");
    assert!(pool.add(NOW_MS, late));
    for block in surbs(21, ONION_SURB_POOL_CAPACITY - 1, 0) {
        assert!(pool.add(NOW_MS, block));
    }
    assert_eq!(pool.len(NOW_MS), ONION_SURB_POOL_CAPACITY);
    assert!(
        !pool.add(NOW_MS, surb(22)),
        "credit beyond Q_max is dropped"
    );

    let first = pool.take(NOW_MS).expect("a block");
    assert_eq!(first.expiry().as_ms(), 3 * QUANTUM_MS, "least x first");
    assert_eq!(
        pool.len(NOW_MS),
        ONION_SURB_POOL_CAPACITY - 1,
        "taken blocks leave"
    );

    // At 3Q every block of x = 3Q has expired; only the later one remains.
    assert_eq!(pool.len(3 * QUANTUM_MS), 1);
    let last = pool.take(3 * QUANTUM_MS).expect("the later block");
    assert_eq!(last.expiry().as_ms(), 5 * QUANTUM_MS);
    assert!(pool.take(3 * QUANTUM_MS).is_none());
    assert!(
        !pool.add(3 * QUANTUM_MS, surb(23)),
        "an expired block is refused"
    );
}

// ---- order -------------------------------------------------------------------------------------

/// Any permutation inside the window releases `0 … m` in order, each once; a duplicate releases
/// nothing.
#[test]
fn test_reorder_releases_every_permutation_in_order() {
    let mut rng = StdRng::seed_from_u64(30);
    for _ in 0..50 {
        let mut order = (0..32_u32).collect::<Vec<_>>();
        rand::seq::SliceRandom::shuffle(order.as_mut_slice(), &mut rng);
        let mut reorder = OnionReorder::default();
        let released = order
            .iter()
            .flat_map(|n| {
                reorder
                    .accept(NOW_MS, OnionSequence::new(*n), *n)
                    .expect("inside the window")
            })
            .collect::<Vec<_>>();
        assert_eq!(released, (0..32).collect::<Vec<_>>());
        assert!(reorder
            .accept(NOW_MS, OnionSequence::new(3), 3)
            .expect("a duplicate is not a gap")
            .is_empty());
    }
}

/// A frame beyond the window, or a missing frame awaited for `V` after a later one, fails the
/// direction closed, after which it releases nothing.
#[test]
fn test_reorder_fails_closed_on_a_persisting_gap() {
    let mut beyond = OnionReorder::default();
    let window = u32::try_from(ONION_SURB_POOL_CAPACITY).expect("small");
    assert_eq!(
        beyond.accept(NOW_MS, OnionSequence::new(window), ()).err(),
        Some(OnionSequenceGap { missing: 0 })
    );
    assert!(beyond.accept(NOW_MS, OnionSequence::FIRST, ()).is_err());

    let mut stalled = OnionReorder::default();
    assert!(stalled
        .accept(NOW_MS, OnionSequence::new(1), ())
        .expect("inside the window")
        .is_empty());
    assert!(stalled
        .expire(NOW_MS + ONION_FORWARD_MAX_VALIDITY_MS - 1)
        .is_ok());
    assert_eq!(
        stalled.expire(NOW_MS + ONION_FORWARD_MAX_VALIDITY_MS).err(),
        Some(OnionSequenceGap { missing: 0 })
    );
}

// ---- exit --------------------------------------------------------------------------------------

/// The kinds of `effects`, for comparison: the payloads of writes and the reply count.
fn kinds(effects: &[OnionExitEffect]) -> Vec<String> {
    effects
        .iter()
        .map(|effect| match effect {
            OnionExitEffect::Open { target } => format!("open {}", String::from_utf8_lossy(target)),
            OnionExitEffect::Write(bytes) => format!("write {}", String::from_utf8_lossy(bytes)),
            OnionExitEffect::ShutdownWrite => "shutdown".to_string(),
            OnionExitEffect::Reply { .. } => "reply".to_string(),
            OnionExitEffect::Close => "close".to_string(),
        })
        .collect()
}

/// A data frame of the client session at `sequence`, with the target inline.
fn opening(sequence: u32, payload: &'static [u8]) -> OnionFrame {
    OnionFrame::Data {
        sequence: OnionSequence::new(sequence),
        target: Some(Bytes::from_static(TARGET)),
        payload: Bytes::from_static(payload),
    }
}

/// The happy path: a `T` loop opens the world, the ack is the first reply, world bytes spend one
/// block each, and `fin` both ways closes the session.
#[test]
fn test_exit_session_opens_acks_replies_and_closes() {
    let mut blocks = surbs(40, 6, 0).into_iter();
    let mut session = OnionExitSession::new(arguments().digest, NOW_MS);
    let mut next = || blocks.next().expect("a fixture block");

    assert_eq!(
        kinds(&session.forward(NOW_MS, opening(0, b"hello"), next())),
        ["open example.com:443", "write hello"]
    );
    let credit = OnionFrame::Credit(vec![next()]);
    assert!(kinds(&session.forward(NOW_MS, credit, next())).is_empty());
    assert_eq!(
        session.reply_capacity(NOW_MS),
        None,
        "no read before the ack"
    );
    assert_eq!(
        kinds(&session.opened(NOW_MS, true)),
        ["reply"],
        "the ack spends a block"
    );
    for chunk in [b"one", b"two"] {
        assert_eq!(
            session.reply_capacity(NOW_MS),
            Some(OnionFrame::data_capacity(CLASS))
        );
        assert_eq!(
            kinds(&session.world(NOW_MS, OnionWorldRead::Bytes(Bytes::from_static(chunk)))),
            ["reply"]
        );
    }
    assert_eq!(session.reply_capacity(NOW_MS), None, "Q = ∅ stops reading");

    let fin = OnionFrame::Fin {
        sequence: OnionSequence::new(1),
    };
    assert_eq!(kinds(&session.forward(NOW_MS, fin, next())), ["shutdown"]);
    assert_eq!(kinds(&session.world(NOW_MS, OnionWorldRead::Eof)), [
        "reply", "close"
    ]);
    assert!(session.is_closed());
}

/// An opened and acked session over `blocks`: the `T` loop and a credit loop of one block leave
/// three blocks, and the ack spends one.
fn acked(blocks: &mut impl Iterator<Item = OnionSurb>) -> OnionExitSession {
    let mut session = OnionExitSession::new(arguments().digest, NOW_MS);
    let mut next = || blocks.next().expect("a fixture block");
    session.forward(NOW_MS, opening(0, b""), next());
    session.forward(NOW_MS, OnionFrame::Credit(vec![next()]), next());
    assert_eq!(kinds(&session.opened(NOW_MS, true)), ["reply"]);
    session
}

/// Totality (#895 B-H1): world bytes that arrive after every block has expired are held, not
/// dropped, the world is not read meanwhile, and the held bytes are replied, in order, as soon
/// as credit returns.
#[test]
fn test_exit_session_holds_world_bytes_until_credit_returns() {
    let mut blocks = surbs(45, 3, 0).into_iter();
    let mut session = acked(&mut blocks);
    let expired_ms = 3 * QUANTUM_MS;

    assert!(session.reply_capacity(NOW_MS).is_some());
    assert!(kinds(&session.world(
        expired_ms,
        OnionWorldRead::Bytes(Bytes::from_static(b"late"))
    ))
    .is_empty());
    assert!(!session.is_closed());
    assert_eq!(
        session.reply_capacity(expired_ms),
        None,
        "held bytes pause reads"
    );

    let mut fresh = surbs(46, 2, 2).into_iter();
    let credit = OnionFrame::Credit(vec![fresh.next().expect("a block")]);
    assert_eq!(
        kinds(&session.forward(expired_ms, credit, fresh.next().expect("a block"))),
        ["reply"]
    );
    assert!(session.reply_capacity(expired_ms).is_some());
}

/// Fail closed (#895 B-M2): a gap spends a remaining block on `fin` before the session closes.
#[test]
fn test_exit_session_aborts_a_gap_with_fin() {
    let mut blocks = surbs(47, 4, 0).into_iter();
    let mut session = acked(&mut blocks);
    let far = OnionFrame::Data {
        sequence: OnionSequence::new(10_000),
        target: None,
        payload: Bytes::from_static(b"x"),
    };

    assert_eq!(
        kinds(&session.forward(NOW_MS, far, blocks.next().expect("a block"))),
        ["reply", "close"]
    );
    assert!(session.is_closed());
}

/// Fail closed (#895 C-M4): a world failure replies `fin` if a block is left, and closes either
/// way.
#[test]
fn test_exit_session_fails_closed_with_fin_while_credit_lasts() {
    let mut blocks = surbs(48, 3, 0).into_iter();
    let mut session = acked(&mut blocks);
    assert_eq!(kinds(&session.fail(NOW_MS)), ["reply", "close"]);

    let mut blocks = surbs(49, 3, 0).into_iter();
    let mut drained = acked(&mut blocks);
    for chunk in [b"y", b"z"] {
        assert_eq!(
            kinds(&drained.world(NOW_MS, OnionWorldRead::Bytes(Bytes::from_static(chunk)))),
            ["reply"]
        );
    }
    assert_eq!(kinds(&drained.fail(NOW_MS)), ["close"]);
}

/// Binding (#895 C-L4): a later `T` frame naming another target aborts a bound session.
#[test]
fn test_exit_session_refuses_to_rebind_its_target() {
    let mut blocks = surbs(50, 4, 0).into_iter();
    let mut session = acked(&mut blocks);
    let other = OnionFrame::Data {
        sequence: OnionSequence::new(1),
        target: Some(Bytes::from_static(b"other.example:443")),
        payload: Bytes::from_static(b"x"),
    };

    assert_eq!(
        kinds(&session.forward(NOW_MS, other, blocks.next().expect("a block"))),
        ["reply", "close"]
    );
}

/// The pool refuses a block whose expiry is not admissible now (#895 B-L2): passed, or beyond
/// `now + V`, whose reply the first relay would refuse.
#[test]
fn test_surb_pool_refuses_inadmissible_expiries() {
    let mut pool = OnionSurbPool::default();
    let beyond = surbs(51, 1, 10).pop().expect("a block");
    let passed = surb(52);

    assert!(!pool.add(NOW_MS, beyond));
    assert!(!pool.add(4 * QUANTUM_MS, passed));
    assert!(pool.add(NOW_MS, surb(53)));
}

/// The sequence space ends cleanly (#895 B-L1, B2-L3): the frame at `u32::MAX` is released,
/// the direction does not fail by itself (a tick after it passes), and only a further frame
/// fails it.
#[test]
fn test_reorder_releases_the_last_sequence_then_fails() {
    let mut reorder = OnionReorder::<u32>::default();
    reorder.next_for_test(OnionSequence::new(u32::MAX));

    assert_eq!(
        reorder.accept(NOW_MS, OnionSequence::new(u32::MAX), 7),
        Ok(vec![7])
    );
    assert_eq!(
        reorder.expire(NOW_MS + ONION_FORWARD_MAX_VALIDITY_MS),
        Ok(())
    );
    assert!(reorder.accept(NOW_MS, OnionSequence::new(0), 8).is_err());
}

/// `count` reply blocks of class `class` expiring at `(3 + offset)·Q`, from a seeded RNG.
fn surbs_of(class: OnionLoopClass, seed: u64, count: usize, offset: u128) -> Vec<OnionSurb> {
    let mut rng = StdRng::seed_from_u64(seed);
    let expiry = OnionExpiry::from_ms((3 + offset) * QUANTUM_MS).expect("on the grid");
    (0..count)
        .map(|_| {
            build_surb(
                return_path().iter(),
                Did::from(9_u32),
                class,
                expiry,
                &mut rng,
            )
            .expect("a reply block")
            .0
        })
        .collect()
}

/// Totality over widths (#895 B2-L4): held bytes wider than one block are cut to each block's
/// own capacity, across blocks of different classes, and a held end of stream is replied after
/// the data; nothing is replied while the pool is empty.
#[test]
fn test_held_bytes_are_cut_to_each_blocks_capacity_then_fin() {
    let mut blocks = surbs(55, 3, 0).into_iter();
    let mut session = acked(&mut blocks);
    let small = OnionFrame::data_capacity(CLASS);
    let expired_ms = 3 * QUANTUM_MS;
    let wide = Bytes::from(vec![0x61; 2 * small + 5]);

    assert!(kinds(&session.world(expired_ms, OnionWorldRead::Bytes(wide))).is_empty());
    assert!(kinds(&session.world(expired_ms, OnionWorldRead::Eof)).is_empty());

    let large = OnionLoopClass::from(crate::onion::circuit::OnionCellBucket::KiB64);
    assert!(OnionFrame::data_capacity(large) > small + 5);
    // The loop's own 16 KiB block arrives first and carries one 16 KiB cut; the 64 KiB block of
    // its credit carries the rest, which exceeds a 16 KiB block; the held fin waits for a block.
    let mut fresh = surbs(56, 2, 2).into_iter();
    let mut wider = surbs_of(large, 57, 1, 2).into_iter();
    assert_eq!(
        kinds(&session.forward(
            expired_ms,
            OnionFrame::Credit(vec![wider.next().expect("a wide block")]),
            fresh.next().expect("a block"),
        )),
        ["reply", "reply"]
    );
    assert_eq!(
        session.reply_capacity(expired_ms),
        None,
        "the fin is still held"
    );
    assert_eq!(
        kinds(&session.forward(
            expired_ms,
            OnionFrame::Credit(vec![fresh.next().expect("a block")]),
            surbs(58, 1, 2).pop().expect("a block"),
        )),
        ["reply"],
        "the held fin leaves with the next block"
    );
}

/// Forward frames reach the world in sequence order although their loops arrive reversed, and
/// the target is bound by the first released `T` frame.
#[test]
fn test_exit_session_writes_in_sequence_order() {
    let mut blocks = surbs(41, 3, 0).into_iter();
    let mut session = OnionExitSession::new(arguments().digest, NOW_MS);
    let mut next = || blocks.next().expect("a fixture block");

    assert!(kinds(&session.forward(NOW_MS, opening(2, b"c"), next())).is_empty());
    assert!(kinds(&session.forward(NOW_MS, opening(1, b"b"), next())).is_empty());
    assert_eq!(kinds(&session.forward(NOW_MS, opening(0, b"a"), next())), [
        "open example.com:443",
        "write a",
        "write b",
        "write c"
    ]);
}

/// A `T` frame whose target does not hash to `d`, or stream bytes before any target, is no
/// session: it closes without opening.
#[test]
fn test_exit_session_rejects_an_unbound_or_mismatching_target() {
    let mut mismatched = OnionExitSession::new(OnionTargetDigest::of(b"other:443"), NOW_MS);
    assert_eq!(
        kinds(&mismatched.forward(NOW_MS, opening(0, b""), surb(42))),
        ["close"]
    );

    let mut untargeted = OnionExitSession::new(arguments().digest, NOW_MS);
    let bare = OnionFrame::Data {
        sequence: OnionSequence::FIRST,
        target: None,
        payload: Bytes::from_static(b"x"),
    };
    assert_eq!(kinds(&untargeted.forward(NOW_MS, bare, surb(43))), [
        "close"
    ]);
}

/// A refused open replies `fin(0)` and closes; an ack without a block waits for credit, and the
/// credit resumes the session.
#[test]
fn test_exit_session_refusal_and_credit_resumption() {
    let mut refused = OnionExitSession::new(arguments().digest, NOW_MS);
    refused.forward(NOW_MS, opening(0, b""), surb(44));
    assert_eq!(kinds(&refused.opened(NOW_MS, false)), ["reply", "close"]);

    let mut starved = OnionExitSession::new(arguments().digest, NOW_MS);
    let mut blocks = surbs(45, 1, 0);
    starved.forward(NOW_MS, opening(0, b""), blocks.remove(0));
    // The open ack spends the session's only block.
    assert_eq!(kinds(&starved.opened(NOW_MS, true)), ["reply"]);
    assert_eq!(starved.reply_capacity(NOW_MS), None);
    let credit = OnionFrame::Credit(surbs(46, 4, 0));
    assert!(kinds(&starved.forward(NOW_MS, credit, surb(47))).is_empty());
    assert_eq!(
        starved.reply_capacity(NOW_MS),
        Some(OnionFrame::data_capacity(CLASS)),
        "new credit resumes reading"
    );
}

/// `V` without a forward loop closes the session.
#[test]
fn test_exit_session_closes_after_v_without_forward_loops() {
    let mut session = OnionExitSession::new(arguments().digest, NOW_MS);
    session.forward(NOW_MS, opening(0, b""), surb(48));
    assert!(session
        .tick(NOW_MS + ONION_FORWARD_MAX_VALIDITY_MS - 1)
        .is_empty());
    assert_eq!(
        kinds(&session.tick(NOW_MS + ONION_FORWARD_MAX_VALIDITY_MS)),
        ["close"]
    );
}

// ---- client ------------------------------------------------------------------------------------

/// `T` is set on every data frame until the first reply and on none after it, sequences count
/// up, and the first released reply decides the open.
#[test]
fn test_client_session_sets_t_until_the_first_reply() {
    let mut session = OnionClientSession::new(Bytes::from_static(TARGET));
    assert_eq!(
        session.data_capacity(CLASS),
        OnionFrame::data_capacity(CLASS) - 2 - TARGET.len()
    );
    for expected in 0..3 {
        let OnionFrame::Data {
            sequence, target, ..
        } = session.data(Bytes::new()).expect("sequence left")
        else {
            panic!("a data frame");
        };
        assert_eq!(sequence.value(), expected);
        assert_eq!(target.as_deref(), Some(TARGET));
    }

    let ack = OnionFrame::Data {
        sequence: OnionSequence::FIRST,
        target: None,
        payload: Bytes::new(),
    };
    assert_eq!(
        session.reply(NOW_MS, ack),
        Ok(vec![OnionClientEvent::Opened])
    );
    let OnionFrame::Data { target, .. } = session.data(Bytes::new()).expect("sequence left") else {
        panic!("a data frame");
    };
    assert_eq!(target, None);
    assert_eq!(
        session.data_capacity(CLASS),
        OnionFrame::data_capacity(CLASS)
    );

    let data = OnionFrame::Data {
        sequence: OnionSequence::new(1),
        target: None,
        payload: Bytes::from_static(b"bytes"),
    };
    let fin = OnionFrame::Fin {
        sequence: OnionSequence::new(2),
    };
    assert_eq!(session.reply(NOW_MS, fin), Ok(Vec::new()));
    assert_eq!(
        session.reply(NOW_MS, data),
        Ok(vec![
            OnionClientEvent::Data(Bytes::from_static(b"bytes")),
            OnionClientEvent::Fin
        ])
    );
}

/// A `fin` before any data is a refusal.
#[test]
fn test_client_session_reads_a_first_fin_as_a_refusal() {
    let mut session = OnionClientSession::new(Bytes::from_static(TARGET));
    let fin = OnionFrame::Fin {
        sequence: OnionSequence::FIRST,
    };
    assert_eq!(
        session.reply(NOW_MS, fin),
        Ok(vec![OnionClientEvent::Refused])
    );
}

/// The credit window asks `⌊D / (k + 1)⌋` full loops, so it never exceeds `min(W, Q_max)` and
/// sends nothing until it is `k + 1` short (#895 N-H2).
#[test]
fn test_credit_window_fills_to_its_target() {
    let window = OnionCreditWindow::DEFAULT;

    assert_eq!(window.credit_loops(0, CLASS), 12);
    assert_eq!(window.credit_loops(59, CLASS), 1);
    assert_eq!(window.credit_loops(60, CLASS), 0);
    assert_eq!(window.credit_loops(64, CLASS), 0);
    assert_eq!(window.credit_loops(300, CLASS), 0);
    assert_eq!(
        OnionCreditWindow::new(10_000).credit_loops(0, CLASS),
        ONION_SURB_POOL_CAPACITY / 5
    );
}

/// Batching (Prop. SURB batching, #843 acceptance): over a steady download of `n` replies, with
/// the window topped up after every reply, the client sends at most `⌈n / (k + 1)⌉ + 1` forward
/// loops after the open, so upload is `≈ 1/(k + 1)` of download.
#[test]
fn test_a_steady_download_costs_one_loop_per_k_plus_one_replies() {
    let window = OnionCreditWindow::DEFAULT;
    let k = OnionFrame::credit_capacity(CLASS);
    let x = OnionExpiry::from_ms(150_000).expect("on the grid");
    let mut credit = OnionClientCredit::default();
    credit.sent(NOW_MS, x, 1);
    for _ in 0..window.credit_loops(credit.count(NOW_MS), CLASS) {
        credit.sent(NOW_MS, x, k + 1);
    }

    let replies: usize = 10_000;
    let mut loops = 0;
    for _ in 0..replies {
        credit.replied(NOW_MS);
        for _ in 0..window.credit_loops(credit.count(NOW_MS), CLASS) {
            credit.sent(NOW_MS, x, k + 1);
            loops += 1;
        }
    }

    assert!(loops <= replies.div_ceil(k + 1) + 1, "{loops} loops");
    assert!(loops >= replies / (k + 1) - 1, "{loops} loops");
}

/// Idle (#895 N-H1): a session with no replies and no data, ticked every 10 s for 10 minutes,
/// sends one keep-alive loop per `V/2`: at most `⌈600 / 75⌉`.
#[test]
fn test_an_idle_session_sends_one_loop_per_half_window() {
    let mut last_forward_ms = 0;
    let mut loops = 0;
    for tick in 1..=60_u128 {
        let now_ms = tick * 10_000;
        if keep_alive_due(now_ms, last_forward_ms) {
            last_forward_ms = now_ms;
            loops += 1;
        }
    }

    assert!(loops <= 600_u32.div_ceil(75), "{loops} loops");
    assert!(loops >= 7, "{loops} loops");
}

/// The ledger saturates at `Q_max`, where `h`'s pool refuses further blocks (#895 H4): a long
/// upload does not leave phantom credit behind, so the following download is topped up again
/// once `h` has spent its blocks.
#[test]
fn test_credit_ledger_saturates_at_the_pool_bound() {
    let mut credit = OnionClientCredit::default();
    let x = OnionExpiry::from_ms(150_000).expect("on the grid");

    for _ in 0..700 {
        credit.sent(NOW_MS, x, 1);
    }
    assert_eq!(credit.count(NOW_MS), ONION_SURB_POOL_CAPACITY);
    for _ in 0..ONION_SURB_POOL_CAPACITY {
        credit.replied(NOW_MS);
    }
    assert_eq!(credit.count(NOW_MS), 0);
    assert!(OnionCreditWindow::DEFAULT.credit_loops(credit.count(NOW_MS), CLASS) > 0);
}
