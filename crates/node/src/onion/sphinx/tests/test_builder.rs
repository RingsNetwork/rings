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
    let admitted_h1 = admitted(cell, &keys[2]);
    let segment = *admitted_h1.layer().outbound.as_bytes();
    let OnionStep::Consumed {
        arguments: first,
        value,
        surb,
        ..
    } = admitted_h1.step().expect("strong key")
    else {
        panic!("h1 consumes");
    };
    let value = value.as_slice().to_vec();
    assert_eq!(
        surb.outbound().as_bytes(),
        &segment,
        "h1's reply block carries σ₁"
    );
    assert_eq!(first, OnionArguments::new([1; 64]));
    assert_eq!(value, b"v0");
    let (next, cell) = surb.produce(b"v1").expect("h1 produces");
    assert_eq!(next, did(3));
    let cell = cell.into_bytes();
    let at_relay = admitted(cell.clone(), &keys[3]);
    assert_ne!(
        at_relay.layer().outbound.as_bytes(),
        &segment,
        "a relay's σ_out is uniform, never the segment seed"
    );
    let (next, cell) = relay(cell, &keys[3]);
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

/// The loop positions of the two-symbol fixture, as indices into its keys: `g, r, h₁, r, r, h₂,
/// r, g`.
const TWO_SYMBOL_POSITIONS: [usize; 8] = [0, 1, 2, 3, 4, 5, 6, 0];

/// What position `i` of a loop sees: its whole decrypted layer `λ_i` (encoded with a fixed MAC,
/// so head and both seeds are compared), the width of the cell it received and its class, and
/// at a symbol hop the value it opens and its reply block's `(next, class, x)`.
#[derive(Debug, Eq, PartialEq)]
struct View {
    /// `enc(λ_i)`.
    layer: Vec<u8>,
    /// `|χ_i| + |y_i|`, the received cell's width.
    width: usize,
    /// The received cell's class.
    class: OnionLoopClass,
    /// `v̂_i` and the reply block's fields, at a symbol hop.
    consumed: Option<(
        Vec<u8>,
        Did,
        OnionLoopClass,
        crate::onion::circuit::OnionExpiry,
    )>,
}

/// Run a built two-symbol loop to position `stop`, with `h₁` producing `outputs[0]` and `h₂`
/// producing `outputs[1]`, and return what `stop` sees.
fn view_at(keys: &[DelegateeKey; 7], cell: Vec<u8>, outputs: [&[u8]; 2], stop: usize) -> View {
    let mac = crate::onion::sphinx::header::OnionHeaderMac::new([0; 16]);
    let mut cell = cell;
    for (position, key) in TWO_SYMBOL_POSITIONS
        .iter()
        .map(|index| &keys[*index])
        .enumerate()
    {
        let width = cell.len();
        let received = OnionCell::parse(&cell).expect("a cell");
        let class = received.class();
        let admitted = admitted(cell, key);
        let layer = admitted.layer().encode(&mac).to_vec();
        match admitted.step().expect("strong key") {
            OnionStep::Relayed { cell: next, .. } => {
                if position == stop {
                    return View {
                        layer,
                        width,
                        class,
                        consumed: None,
                    };
                }
                cell = next.into_bytes();
            }
            OnionStep::Consumed { value, surb, .. } => {
                if position == stop {
                    return View {
                        layer,
                        width,
                        class,
                        consumed: Some((
                            value.as_slice().to_vec(),
                            surb.next(),
                            surb.class(),
                            surb.expiry(),
                        )),
                    };
                }
                let output = if position == 2 {
                    outputs[0]
                } else {
                    outputs[1]
                };
                cell = surb.produce(output).expect("produce").1.into_bytes();
            }
        }
    }
    panic!("position {stop} is not on the loop");
}

/// Draw a value of 0 to 99 bytes.
fn draw_value(rng: &mut impl RngCore) -> Vec<u8> {
    let mut value = vec![0; usize::try_from(rng.next_u32() % 100).expect("small")];
    rng.fill_bytes(&mut value);
    value
}

/// Non-interference (L5, #834 Tests; #895 B-M3, B2-M2), as a seeded property over drawn
/// applications and values: for every position `i` of a two-symbol loop, two loops built from
/// the same seed that differ in every `ā_j, v_j` with `j ≠ i` (and agree on `i`'s own `ā_i` and
/// input) give `i` the same view: its whole layer `λ_i`, the width and class of its cell, and at
/// a symbol hop the value it opens and its reply block's `(next, class, x)`.
///
/// Ciphertext bytes are compared for length only: L5 is computational, and under a shared seed
/// the ciphertexts differ exactly by the plaintext difference, so byte equality is not the law.
/// What the property rules out is a builder that writes another position's data into `λ_i`, or
/// whose draws depend on values, either of which changes some compared field.
#[test]
fn test_no_position_sees_the_applications_of_another() {
    let keys = keys_of_two();
    let mut draws = fixture_rng(83);
    for case in 0..8_u64 {
        for stop in 0..TWO_SYMBOL_POSITIONS.len() {
            let [a1, a2, b1, b2] = [(); 4].map(|()| {
                let mut arguments = [0; 64];
                draws.fill_bytes(&mut arguments);
                arguments
            });
            let [v0, w0, v1, w1, v2, w2] = [(); 6].map(|()| draw_value(&mut draws));
            // Keep what position `stop` owns: h₁'s ā₁ and input v₀, h₂'s ā₂ and input v₁.
            let (b1, w0) = if stop == 2 {
                (a1, v0.clone())
            } else {
                (b1, w0)
            };
            let (b2, w1) = if stop == 5 {
                (a2, v1.clone())
            } else {
                (b2, w1)
            };
            let seed = 1_000 + case * 16 + u64::try_from(stop).expect("small");
            let first = build_two(&keys, &two_applications(a1, a2), &v0, seed);
            let second = build_two(&keys, &two_applications(b1, b2), &w0, seed);

            assert_eq!(
                view_at(&keys, first.cell.into_bytes(), [&v1, &v2], stop),
                view_at(&keys, second.cell.into_bytes(), [&w1, &w2], stop),
                "case {case}, position {stop}"
            );
        }
    }
}
