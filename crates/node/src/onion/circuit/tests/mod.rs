//! Law tests of the loop data plane (#843), one module per law set of the parent documentation.
//!
//! Every test is deterministic: randomness comes from seeded [`StdRng`]s, hop keys from fixed
//! scalars, and time is an argument, so a failure reproduces bit for bit.

mod test_hop;
mod test_tags;
mod test_wire_golden;

use std::num::NonZeroUsize;

use rand::rngs::StdRng;
use rand::SeedableRng;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;

use super::admission::OnionAdmissionLink;
use super::admission::OnionAdmissionState;
use super::admission::OnionReplayFilterKey;
use super::hop::hop;
use super::hop::OnionHopOutcome;
use super::OnionExpiry;
use crate::onion::session::OnionSessionArguments;
use crate::onion::session::OnionSessionId;
use crate::onion::session::OnionTargetDigest;
use crate::onion::sphinx::builder::build_loop;
use crate::onion::sphinx::builder::OnionApplication;
use crate::onion::sphinx::builder::OnionBuiltLoop;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::header::OnionLoopTag;
use crate::onion::OnionLoop;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRouteHop;
use crate::onion::OnionServiceName;

/// The process epoch every fixture hop runs.
const EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([0x43; 16]);

/// The arrival instant of every fixture cell: `2Q`.
const NOW_MS: u128 = 60_000;

/// The DID the fixture client receives at.
const CLIENT: u32 = 99;

/// The fixture loop's expiry `x = 5Q`, admissible at [`NOW_MS`].
fn expiry() -> OnionExpiry {
    OnionExpiry::from_ms(150_000).expect("on the grid")
}

/// One hop of the fixture loop: its key and its admission state.
struct Hop {
    /// `d_i`, the hop's session key.
    key: DelegateeKey,
    /// The hop's admission state, with its predecessor's link live.
    admission: OnionAdmissionState,
}

impl Hop {
    /// The hop with the fixed scalar `(seed)·0x0101…01`, receiving from `from`.
    fn new(seed: u8, from: Did) -> Self {
        let secret =
            SecretKey::try_from(format!("{seed:02x}").repeat(32).as_str()).expect("fixture scalar");
        let mut admission = OnionAdmissionState::new(
            EPOCH,
            OnionReplayFilterKey::new([seed; 32]),
            NonZeroUsize::MIN,
        );
        admission
            .link_opened(NOW_MS, OnionAdmissionLink {
                did: from,
                generation: 0,
            })
            .expect("an empty table admits a link");
        Self {
            key: DelegateeKey::new_with_seckey(&secret).expect("fixture delegation"),
            admission,
        }
    }

    /// The hop's DID.
    fn did(&self) -> Did {
        self.key.delegator_did()
    }

    /// The hop's route entry.
    fn route_hop(&self) -> OnionRouteHop {
        OnionRouteHop::new(self.did(), self.key.delegatee_public_key(), EPOCH)
    }

    /// `Hop_i` of `cell` from `from` at [`NOW_MS`], as a relaying node that owns no loop tags.
    fn step(&mut self, from: Did, cell: OnionCell) -> OnionHopOutcome {
        hop(
            &mut self.admission,
            &self.key,
            true,
            |_| false,
            from,
            NOW_MS,
            cell,
        )
    }
}

/// The fixture loop `client → g → r₀₂ → h → r₁₁ → g → client`: its four hops, `[g, r₀₂, h,
/// r₁₁]`, each with its predecessor's link live. The guard receives from both the client and
/// `r₁₁`, so its table holds both links.
struct Fixture {
    /// `[g, r₀₂, h, r₁₁]`.
    hops: [Hop; 4],
}

impl Fixture {
    /// The fixture's hops.
    fn new() -> Self {
        let client = Did::from(CLIENT);
        let mut guard = Hop::new(1, client);
        let r02 = Hop::new(2, guard.did());
        let h = Hop::new(3, r02.did());
        let r11 = Hop::new(4, h.did());
        guard
            .admission
            .link_opened(NOW_MS, OnionAdmissionLink {
                did: r11.did(),
                generation: 0,
            })
            .expect("the guard's table holds two links");
        Self {
            hops: [guard, r02, h, r11],
        }
    }

    /// The route `g, r₀₂, h, r₁₁, g`.
    fn route(&self) -> OnionLoop<OnionRouteHop> {
        let mut relays = [0, 1, 3].into_iter();
        OnionLoop::try_unfold(Vec::new(), self.hops[2].route_hop(), |_| {
            relays
                .next()
                .map(|index| self.hops[index].route_hop())
                .ok_or(crate::error::Error::InvalidData)
        })
        .expect("the fixture loop")
    }

    /// A seeded loop applying `tcp` at `h` to `value`.
    fn build(&self, seed: u64, value: &[u8]) -> OnionBuiltLoop {
        build_loop(
            &self.route(),
            &[OnionApplication {
                symbol: OnionServiceName::tcp(),
                arguments: arguments().encode(),
            }],
            Did::from(CLIENT),
            OnionLoopClass::DEFAULT,
            expiry(),
            value,
            &mut StdRng::seed_from_u64(seed),
        )
        .expect("build the fixture loop")
    }

    /// Run the forward half `g, r₀₂` and return the cell as `h` receives it.
    fn run_forward_segment(&mut self, cell: OnionCell) -> OnionCell {
        let client = Did::from(CLIENT);
        let guard = self.hops[0].did();
        let cell = relayed(self.hops[0].step(client, cell), self.hops[1].did());
        relayed(self.hops[1].step(guard, cell), self.hops[2].did())
    }

    /// Run the return half `r₁₁, g` of a reply cell produced at `h`, and return it as the
    /// client receives it.
    fn run_return_segment(&mut self, cell: OnionCell) -> OnionCell {
        let h = self.hops[2].did();
        let r11 = self.hops[3].did();
        let cell = relayed(self.hops[3].step(h, cell), self.hops[0].did());
        relayed(self.hops[0].step(r11, cell), Did::from(CLIENT))
    }
}

/// The fixture session's arguments.
fn arguments() -> OnionSessionArguments {
    OnionSessionArguments {
        session: OnionSessionId::new([0x51; 16]),
        digest: OnionTargetDigest::of(b"example.com:443"),
    }
}

/// The cell of a `Relayed` outcome, which must go to `next`.
fn relayed(outcome: OnionHopOutcome, next: Did) -> OnionCell {
    match outcome {
        OnionHopOutcome::Relayed { next: to, cell } => {
            assert_eq!(to, next);
            cell
        }
        other => panic!("expected a relay to {next}, got {other:?}"),
    }
}

/// Whether `tag` is `expected`, as a client's tag table would answer.
fn is_tag(expected: OnionLoopTag) -> impl Fn(&OnionLoopTag) -> bool {
    move |tag| *tag == expected
}
