//! The loop builder (D4, D5, D6′, D7): a built loop runs through paid hops end to end.
//!
//! ```text
//! client ─σ₀─▶ g ─▶ r₀₂ ─▶ h (consume v₀, reply v₁ under σ₁) ─▶ r₁₁ ─▶ g ─▶ client (t_⋄, k_{c₁})
//! ```
//!
//! Laws: every layer names the DID of the next position and the last names the client (L6,
//! D6′), every hop reads its own epoch and the loop's one expiry (D6), the seeds the builder
//! placed let each consumer open exactly its producer's value (D7), and a reply block built for
//! credit over the return path reaches the client with its own tag and key (D8).

use rand::Rng;
use rand::RngCore;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;

use super::admitted;
use super::fixture_expiry;
use super::fixture_rng;
use super::hop_key;
use super::FIXTURE_EPOCH;
use crate::onion::sphinx::builder::build_loop;
use crate::onion::sphinx::builder::build_surb;
use crate::onion::sphinx::builder::OnionApplication;
use crate::onion::sphinx::builder::OnionBuildError;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::cell::OnionStep;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::layer::OnionArguments;
use crate::onion::sphinx::layer::OnionLayerApplication;
use crate::onion::OnionLoop;
use crate::onion::OnionLoopRelay;
use crate::onion::OnionRouteHop;
use crate::onion::OnionServiceName;

/// The DID the client of the fixture loop receives at.
fn client() -> Did {
    Did::from(99_u32)
}

/// The four hop keys of the fixture loop. A key's session half is drawn per call of `hop_key`,
/// so each test holds one set and uses it for both the route and the peels.
fn keys() -> [DelegateeKey; 4] {
    [0, 1, 2, 3].map(hop_key)
}

/// The route hop of `key`.
fn route_hop(key: &DelegateeKey) -> OnionRouteHop {
    OnionRouteHop::new(
        key.delegator_did(),
        key.delegatee_public_key(),
        FIXTURE_EPOCH,
    )
}

/// The session loop `g = 0, r₀₂ = 1, h = 2, r₁₁ = 3, g = 0` of `keys`.
fn session_loop(keys: &[DelegateeKey; 4]) -> OnionLoop<OnionRouteHop> {
    let mut relays = [0, 1, 3].into_iter();
    OnionLoop::try_unfold(Vec::new(), route_hop(&keys[2]), |relay| {
        let index = relays.next().ok_or(crate::error::Error::InvalidData)?;
        assert_eq!(relay == OnionLoopRelay::Guard, index == 0);
        Ok(route_hop(&keys[index]))
    })
    .expect("a session loop")
}

/// Relay a cell through `key`, checking the relay's layer; return the cell and the DID it goes
/// to.
fn relay(bytes: Vec<u8>, key: &DelegateeKey) -> (Did, Vec<u8>) {
    let admitted = admitted(bytes, key);
    assert_eq!(admitted.head().epoch, FIXTURE_EPOCH);
    assert_eq!(admitted.head().expiry, fixture_expiry(0));
    assert_eq!(admitted.head().application, OnionLayerApplication::Relay);
    let expected_next = admitted.head().next;
    let OnionStep::Relayed { next, cell } = admitted.step().expect("strong key") else {
        panic!("a relay position");
    };
    assert_eq!(next, expected_next);
    (next, cell.into_bytes())
}

/// A built loop carries the client's value to `h`, whose reply reaches the client under the
/// reply key, every layer naming the next position.
#[test]
fn test_built_loop_runs_through_paid_hops() {
    let mut rng = fixture_rng(70);
    let keys = keys();
    let hops = session_loop(&keys);
    let mut arguments = [0; 64];
    rng.fill_bytes(&mut arguments);
    let arguments = OnionArguments::new(arguments);
    let application = OnionApplication {
        symbol: OnionServiceName::tcp(),
        arguments,
    };
    let input = rng.gen::<[u8; 32]>();
    let output = rng.gen::<[u8; 24]>();
    let built = build_loop(
        &hops,
        std::slice::from_ref(&application),
        client(),
        OnionLoopClass::DEFAULT,
        fixture_expiry(0),
        &input,
        &mut rng,
    )
    .expect("build the loop");

    let did = |index: usize| keys[index].delegator_did();
    assert_eq!(built.guard, did(0));
    let (next, cell) = relay(built.cell.into_bytes(), &keys[0]);
    assert_eq!(next, did(1));
    let (next, cell) = relay(cell, &keys[1]);
    assert_eq!(next, did(2));
    let OnionStep::Consumed {
        symbol,
        arguments: consumed,
        value,
        surb,
    } = admitted(cell, &keys[2]).step().expect("strong key")
    else {
        panic!("the symbol position");
    };
    assert_eq!(symbol, application.symbol);
    assert_eq!(consumed, arguments);
    assert_eq!(value.as_slice(), input);
    assert_eq!(surb.expiry(), fixture_expiry(0));
    let (next, reply) = surb.produce(&output).expect("produce the reply");
    assert_eq!(next, did(3));
    let (next, reply) = relay(reply.into_bytes(), &keys[3]);
    assert_eq!(next, did(0));
    let (next, reply) = relay(reply, &keys[0]);
    assert_eq!(next, client());

    let returned = OnionCell::parse(&reply).expect("a cell");
    assert_eq!(returned.loop_tag(), built.reply.tag);
    assert_eq!(built.reply.expiry, fixture_expiry(0));
    assert_eq!(
        returned
            .open(&built.reply.key)
            .expect("the reply opens")
            .as_slice(),
        output
    );
}

/// A reply block built over the return path alone reaches the client with a fresh tag and opens
/// under its own key, and two blocks never share a tag.
#[test]
fn test_credit_surb_reaches_the_client_under_its_own_key() {
    let mut rng = fixture_rng(71);
    let keys = keys();
    let hops = session_loop(&keys);
    let (surb, reply) = build_surb(
        hops.return_path(),
        client(),
        OnionLoopClass::DEFAULT,
        fixture_expiry(0),
        &mut rng,
    )
    .expect("build a reply block");
    let (_, other) = build_surb(
        hops.return_path(),
        client(),
        OnionLoopClass::DEFAULT,
        fixture_expiry(0),
        &mut rng,
    )
    .expect("build a reply block");
    assert_ne!(reply.tag, other.tag);

    let (next, cell) = surb.produce(b"credit reply").expect("produce");
    assert_eq!(next, keys[3].delegator_did());
    let (_, cell) = relay(cell.into_bytes(), &keys[3]);
    let (next, cell) = relay(cell, &keys[0]);
    assert_eq!(next, client());
    let returned = OnionCell::parse(&cell).expect("a cell");
    assert_eq!(returned.loop_tag(), reply.tag);
    assert_eq!(
        returned.open(&reply.key).expect("opens").as_slice(),
        b"credit reply"
    );
}

/// The applications must match the loop's symbol positions, one per symbol hop.
#[test]
fn test_applications_must_match_the_symbol_positions() {
    let mut rng = fixture_rng(72);
    let application = OnionApplication {
        symbol: OnionServiceName::https(),
        arguments: OnionArguments::new([0; 64]),
    };
    for applications in [Vec::new(), vec![application.clone(), application]] {
        assert_eq!(
            build_loop(
                &session_loop(&keys()),
                &applications,
                client(),
                OnionLoopClass::DEFAULT,
                fixture_expiry(0),
                b"",
                &mut rng,
            )
            .err(),
            Some(OnionBuildError::Shape)
        );
    }
}

/// The seven hop keys of the two-symbol fixture loop.
fn keys_of_two() -> [DelegateeKey; 7] {
    [0, 1, 2, 3, 4, 5, 6].map(hop_key)
}

/// The loop `g = 0, r = 1, h₁ = 2, r = 3, r = 4, h₂ = 5, r = 6, g = 0` of `keys` (n = 2, H = 8).
fn two_symbol_loop(keys: &[DelegateeKey; 7]) -> OnionLoop<OnionRouteHop> {
    let mut relays = [0, 1, 3, 4, 6].into_iter();
    OnionLoop::try_unfold(vec![route_hop(&keys[2])], route_hop(&keys[5]), |relay| {
        let index = relays.next().ok_or(crate::error::Error::InvalidData)?;
        assert_eq!(relay == OnionLoopRelay::Guard, index == 0);
        Ok(route_hop(&keys[index]))
    })
    .expect("a two-symbol loop")
}

/// The applications `tcp(ā₁)` at `h₁` and `tcp(ā₂)` at `h₂`.
fn two_applications(first: [u8; 64], second: [u8; 64]) -> [OnionApplication; 2] {
    [first, second].map(|arguments| OnionApplication {
        symbol: OnionServiceName::tcp(),
        arguments: OnionArguments::new(arguments),
    })
}

/// Build the two-symbol loop over `keys` with `applications` and `value`, from seed `seed`.
fn build_two(
    keys: &[DelegateeKey; 7],
    applications: &[OnionApplication; 2],
    value: &[u8],
    seed: u64,
) -> crate::onion::sphinx::builder::OnionBuiltLoop {
    build_loop(
        &two_symbol_loop(keys),
        applications,
        client(),
        OnionLoopClass::DEFAULT,
        fixture_expiry(0),
        value,
        &mut fixture_rng(seed),
    )
    .expect("build the two-symbol loop")
}

/// Consume a cell at a symbol position under `key`: its application, its value and its reply
/// block.
fn consume(
    bytes: Vec<u8>,
    key: &DelegateeKey,
) -> (
    OnionServiceName,
    OnionArguments,
    Vec<u8>,
    Box<crate::onion::sphinx::cell::OnionSurb>,
) {
    let OnionStep::Consumed {
        symbol,
        arguments,
        value,
        surb,
    } = admitted(bytes, key).step().expect("strong key")
    else {
        panic!("a symbol position");
    };
    (symbol, arguments, value.as_slice().to_vec(), surb)
}

/// The seed law at `n = 2` (#895 B-M4): the segment hand-over at `h₁` runs, so `h₁` opens the
/// client's value, `h₂` opens exactly what `h₁` produced, and the client opens what `h₂`
/// produced, each under the key the builder placed; every layer names the next position.
#[test]
fn test_a_two_symbol_loop_hands_each_segment_to_its_consumer() {
    let keys = keys_of_two();
    let did = |index: usize| keys[index].delegator_did();
    let built = build_two(&keys, &two_applications([1; 64], [2; 64]), b"v0", 80);

    let (next, cell) = relay(built.cell.into_bytes(), &keys[0]);
    assert_eq!(next, did(1));
    let (next, cell) = relay(cell, &keys[1]);
    assert_eq!(next, did(2));
    let (_, first, value, surb) = consume(cell, &keys[2]);
    assert_eq!(first, OnionArguments::new([1; 64]));
    assert_eq!(value, b"v0");
    let (next, cell) = surb.produce(b"v1").expect("h1 produces");
    assert_eq!(next, did(3));
    let (next, cell) = relay(cell.into_bytes(), &keys[3]);
    assert_eq!(next, did(4));
    let (next, cell) = relay(cell, &keys[4]);
    assert_eq!(next, did(5));
    let (_, second, value, surb) = consume(cell, &keys[5]);
    assert_eq!(second, OnionArguments::new([2; 64]));
    assert_eq!(value, b"v1", "h2 opens exactly what h1 produced");
    let (next, cell) = surb.produce(b"v2").expect("h2 produces");
    assert_eq!(next, did(6));
    let (next, cell) = relay(cell.into_bytes(), &keys[6]);
    assert_eq!(next, did(0));
    let (next, cell) = relay(cell, &keys[0]);
    assert_eq!(next, client());

    let returned = OnionCell::parse(&cell).expect("a cell");
    assert_eq!(returned.loop_tag(), built.reply.tag);
    assert_eq!(
        returned.open(&built.reply.key).expect("opens").as_slice(),
        b"v2"
    );
}

/// Non-interference (L5, #834 Tests; #895 B-M3): what position `i` sees does not depend on the
/// applications or values of other positions. Two loops built from the same seed, differing
/// only in `ā_j, v_j` for `j ≠ i`, give position `i` the same head, the same cell width and,
/// at a symbol hop, the same application, input and reply block shape; the builder draws its
/// secrets in an order that no value influences, so equal seeds make these views equal.
#[test]
fn test_no_position_sees_the_applications_of_another() {
    let keys = keys_of_two();
    let width = OnionLoopClass::DEFAULT.cell_bytes();

    // Relays before h₁ see nothing of ā₁, ā₂ or v₀.
    let a = build_two(&keys, &two_applications([1; 64], [2; 64]), b"one value", 81);
    let b = build_two(&keys, &two_applications([7; 64], [8; 64]), b"another", 81);
    let (mut cell_a, mut cell_b) = (a.cell.into_bytes(), b.cell.into_bytes());
    for position in [0, 1] {
        assert_eq!(cell_a.len(), width);
        assert_eq!(cell_b.len(), width);
        let (view_a, view_b) = (
            admitted(cell_a, &keys[position]),
            admitted(cell_b, &keys[position]),
        );
        assert_eq!(view_a.head(), view_b.head(), "position {position}");
        let (OnionStep::Relayed { cell: next_a, .. }, OnionStep::Relayed { cell: next_b, .. }) = (
            view_a.step().expect("strong"),
            view_b.step().expect("strong"),
        ) else {
            panic!("a relay position");
        };
        (cell_a, cell_b) = (next_a.into_bytes(), next_b.into_bytes());
    }

    // h₁ sees its own ā₁ and v₀, never ā₂.
    let a = build_two(&keys, &two_applications([1; 64], [2; 64]), b"v0", 82);
    let b = build_two(&keys, &two_applications([1; 64], [9; 64]), b"v0", 82);
    let (mut cell_a, mut cell_b) = (a.cell.into_bytes(), b.cell.into_bytes());
    for position in [0, 1] {
        cell_a = relay(cell_a, &keys[position]).1;
        cell_b = relay(cell_b, &keys[position]).1;
    }
    let view_a = admitted(cell_a, &keys[2]);
    let view_b = admitted(cell_b, &keys[2]);
    assert_eq!(view_a.head(), view_b.head());
    let (
        OnionStep::Consumed {
            value: value_a,
            surb: surb_a,
            ..
        },
        OnionStep::Consumed {
            value: value_b,
            surb: surb_b,
            ..
        },
    ) = (
        view_a.step().expect("strong"),
        view_b.step().expect("strong"),
    )
    else {
        panic!("the symbol position");
    };
    assert_eq!(value_a.as_slice(), value_b.as_slice());
    assert_eq!(surb_a.expiry(), surb_b.expiry());
    assert_eq!(surb_a.class(), surb_b.class());
}
