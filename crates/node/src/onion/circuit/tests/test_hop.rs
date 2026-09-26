//! Laws of `Hop_i` (`circuit::hop`): paid, at most once, identity, and the client position.

use rings_core::dht::Did;

use super::super::admission::OnionAdmissionRejection;
use super::super::admission::OnionChargeRejection;
use super::super::hop::hop;
use super::super::hop::OnionHopDrop;
use super::super::hop::OnionHopOutcome;
use super::arguments;
use super::is_tag;
use super::Fixture;
use super::CLIENT;
use super::NOW_MS;
use crate::onion::session::OnionSessionArguments;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionServiceName;

/// A cell from a sender with no live link is refused before it is charged or peeled (Paid).
#[test]
fn test_a_cell_off_a_live_link_is_refused_unpeeled() {
    let mut fixture = Fixture::new();
    let built = fixture.build(1, b"value");

    assert!(matches!(
        fixture.hops[0].step(Did::from(7_u32), built.cell),
        OnionHopOutcome::Refused(OnionChargeRejection::LinkNotLive)
    ));
}

/// A cell of the right width whose header is noise is charged and dropped at the peel: cover
/// and garbage cost their sender the same as a real cell (Paid).
#[test]
fn test_noise_of_a_class_width_is_charged_then_dropped_at_the_peel() {
    let mut fixture = Fixture::new();
    let noise = OnionCell::parse(vec![0x5a; OnionLoopClass::DEFAULT.cell_bytes()]).expect("width");

    assert!(matches!(
        fixture.hops[0].step(Did::from(CLIENT), noise),
        OnionHopOutcome::Dropped(OnionHopDrop::Peel(_))
    ));
}

/// A layer is relayed once; the same cell again is its replay, charged and dropped (L9), and
/// the relayed cell keeps the received class and width (L1).
#[test]
fn test_a_relayed_layer_is_admitted_at_most_once_and_keeps_its_width() {
    let mut fixture = Fixture::new();
    let bytes = fixture.build(2, b"value").cell.into_bytes();
    let received = OnionCell::parse(bytes.clone()).expect("width");
    let class = received.class();

    let OnionHopOutcome::Relayed { next, cell } = fixture.hops[0].step(Did::from(CLIENT), received)
    else {
        panic!("the guard relays");
    };
    assert_eq!(next, fixture.hops[1].did());
    assert_eq!(cell.class(), class);
    assert_eq!(cell.into_bytes().len(), bytes.len());
    assert!(matches!(
        fixture.hops[0].step(Did::from(CLIENT), OnionCell::parse(bytes).expect("width")),
        OnionHopOutcome::Dropped(OnionHopDrop::Admission(OnionAdmissionRejection::Replayed))
    ));
}

/// A node that registers no `relay` drops a relay layer after admission (D1′).
#[test]
fn test_a_relay_layer_at_a_non_relay_is_dropped() {
    let mut fixture = Fixture::new();
    let built = fixture.build(3, b"value");
    let guard = &mut fixture.hops[0];

    assert!(matches!(
        hop(
            &mut guard.admission,
            &guard.key,
            false,
            |_| false,
            Did::from(CLIENT),
            NOW_MS,
            built.cell,
        ),
        OnionHopOutcome::Dropped(OnionHopDrop::NotRelay)
    ));
}

/// The loop reaches `h` as its application and the client's value, and the reply `h` produces
/// returns through `r₁₁, g` to the client, which recognises its tag without a peel (D6′).
#[test]
fn test_a_loop_runs_to_its_symbol_and_returns_to_the_client() {
    let mut fixture = Fixture::new();
    let built = fixture.build(4, b"client value");
    let cell = fixture.run_forward_segment(built.cell);
    let from = fixture.hops[1].did();

    let OnionHopOutcome::Consumed {
        symbol,
        arguments: consumed,
        value,
        surb,
    } = fixture.hops[2].step(from, cell)
    else {
        panic!("h consumes");
    };
    assert_eq!(symbol, OnionServiceName::tcp());
    assert_eq!(OnionSessionArguments::decode(&consumed), Some(arguments()));
    assert_eq!(value.as_slice(), b"client value");

    let (next, reply) = surb.produce(b"reply value").expect("produce the reply");
    assert_eq!(next, fixture.hops[3].did());
    let returned = fixture.run_return_segment(reply);
    assert_eq!(returned.loop_tag(), built.reply.tag);

    // The client, as a node, sees the cell arrive from its guard with a live tag: `Returned`,
    // even under a key that could not peel it.
    let mut client = super::Hop::new(0x63, fixture.hops[0].did());
    let guard = fixture.hops[0].did();
    let OnionHopOutcome::Returned { tag, cell } = hop(
        &mut client.admission,
        &client.key,
        true,
        is_tag(built.reply.tag),
        guard,
        NOW_MS,
        returned,
    ) else {
        panic!("the client's own tag returns");
    };
    assert_eq!(tag, built.reply.tag);
    assert_eq!(
        cell.open(&built.reply.key).expect("opens").as_slice(),
        b"reply value"
    );
}
