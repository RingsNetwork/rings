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
    let OnionStep::Relayed { head, cell } = admitted.step().expect("strong key") else {
        panic!("a relay position");
    };
    assert_eq!(head.application, OnionLayerApplication::Relay);
    (head.next, cell.into_bytes())
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
    let OnionStep::Consumed { head, value, surb } =
        admitted(cell, &keys[2]).step().expect("strong key")
    else {
        panic!("the symbol position");
    };
    assert_eq!(head.application, OnionLayerApplication::Apply {
        symbol: application.symbol,
        arguments,
    });
    assert_eq!(value.as_slice(), input);
    assert_eq!(surb.expiry(), fixture_expiry(0));
    let (next, reply) = surb.produce(&output).expect("produce the reply");
    assert_eq!(next, did(3));
    let (next, reply) = relay(reply.into_bytes(), &keys[3]);
    assert_eq!(next, did(0));
    let (next, reply) = relay(reply, &keys[0]);
    assert_eq!(next, client());

    let returned = OnionCell::parse(reply).expect("a cell");
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
    let returned = OnionCell::parse(cell).expect("a cell");
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
