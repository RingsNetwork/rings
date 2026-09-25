//! Laws of the L9 admission step, checked against injected time and a seeded RNG, one file per
//! proposition: the window and at-most-once ([`window`]), the unit budgets ([`budget`]), links and
//! their ledgers ([`links`]), and the replay store's geometry and memory ([`bloom`]). This module
//! holds the shared vocabulary: the shell pipeline `charge ; admit` and the constructors of links,
//! layers and expiries.

mod bloom;
mod budget;
mod links;
mod window;

use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::ops::Range;

use rand::rngs::StdRng;
use rand::Rng;
use rings_core::dht::Did;

use super::OnionAdmissionLayer;
use super::OnionAdmissionLink;
use super::OnionAdmissionRejection;
use super::OnionAdmissionState;
use super::OnionAdmissionUnits;
use super::OnionChargeRejection;
use super::OnionExpiry;
use super::OnionReplayFilterKey;
use super::ADMISSION_WINDOW_QUANTA_WIDE;
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::circuit::ONION_FORWARD_EXPIRY_QUANTUM_MS;
use crate::onion::OnionProcessEpoch;

/// `Q` in milliseconds.
pub(super) const Q: u128 = ONION_FORWARD_EXPIRY_QUANTUM_MS;

/// An aligned origin far from zero, so no window arithmetic saturates.
pub(super) const ORIGIN_MS: u128 = 1_000 * Q;

/// The epoch of the process under test.
pub(super) const EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([7; 16]);

/// The verdict of one cell through the shell's pipeline: the charge, then the admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Verdict {
    /// The cell was admitted.
    Admitted,
    /// The charge was refused. Nothing was charged and the cell was not decrypted.
    Unpaid(OnionChargeRejection),
    /// The cell was charged and then rejected.
    Rejected(OnionAdmissionRejection),
}

/// A connection-registry capacity `R ≥ 1`; zero saturates to one.
pub(super) fn registry(r: usize) -> NonZeroUsize {
    NonZeroUsize::MIN.saturating_add(r.saturating_sub(1))
}

/// Generation `generation` of a link from DID `did`.
pub(super) fn generation(did: u32, generation: u64) -> OnionAdmissionLink {
    OnionAdmissionLink {
        did: Did::from(did),
        generation,
    }
}

/// The first generation of a link from DID `did`.
pub(super) fn link(did: u32) -> OnionAdmissionLink {
    generation(did, 0)
}

/// A fresh state for `epoch` with registry capacity `R`, a probe key drawn from `rng`, and the
/// first link generation of each DID in `linked` opened at time zero.
pub(super) fn state_with(
    rng: &mut StdRng,
    epoch: OnionProcessEpoch,
    r: usize,
    linked: Range<u32>,
) -> OnionAdmissionState {
    let mut admission =
        OnionAdmissionState::new(epoch, OnionReplayFilterKey::new(rng.gen()), registry(r));
    for sender in linked {
        open(&mut admission, 0, link(sender));
    }
    admission
}

/// A fresh state for [`EPOCH`] with `R = 64` and live links from DIDs `0..=64`.
pub(super) fn state(rng: &mut StdRng) -> OnionAdmissionState {
    state_with(rng, EPOCH, 64, 0..65)
}

/// Open `link` at `now_ms`, which the test expects the table to accept.
pub(super) fn open(admission: &mut OnionAdmissionState, now_ms: u128, link: OnionAdmissionLink) {
    admission
        .link_opened(now_ms, link)
        .expect("the table has room for this link");
}

/// The on-grid expiry `k · Q`, through the parser, the only way from a wire instant to an expiry.
pub(super) fn expiry(quantum: u128) -> OnionExpiry {
    OnionExpiry::from_ms(quantum * Q).expect("a multiple of Q lies on the grid")
}

/// The latest admissible expiry at `now_ms`: the greatest grid point in `(now, now + V]`.
pub(super) fn latest_expiry(now_ms: u128) -> OnionExpiry {
    expiry(now_ms / Q + ADMISSION_WINDOW_QUANTA_WIDE)
}

/// `n ≥ 1` units; zero saturates to one unit.
pub(super) fn units(n: u32) -> OnionAdmissionUnits {
    OnionAdmissionUnits::new(NonZeroU32::MIN.saturating_add(n.saturating_sub(1)))
}

/// A layer for the current epoch.
pub(super) fn layer(expiry: OnionExpiry, tag: u128) -> OnionAdmissionLayer {
    OnionAdmissionLayer {
        epoch: EPOCH,
        expiry,
        tag: OnionForwardNonce::new(tag.to_le_bytes()),
    }
}

/// One cell of `n` units on `link` with a `γ`-valid `layer`, through the shell's pipeline: charge,
/// then admit with the token.
pub(super) fn send_on(
    admission: &mut OnionAdmissionState,
    now_ms: u128,
    link: OnionAdmissionLink,
    n: u32,
    layer: OnionAdmissionLayer,
) -> Verdict {
    match admission.charge_units(now_ms, link, units(n)) {
        Err(rejection) => Verdict::Unpaid(rejection),
        Ok(token) => match admission.admit(now_ms, token, layer) {
            Ok(()) => Verdict::Admitted,
            Err(rejection) => Verdict::Rejected(rejection),
        },
    }
}

/// One cell of `n` units on the first link generation of `from`.
pub(super) fn send(
    admission: &mut OnionAdmissionState,
    now_ms: u128,
    from: u32,
    n: u32,
    layer: OnionAdmissionLayer,
) -> Verdict {
    send_on(admission, now_ms, link(from), n, layer)
}

/// The units charged to `from` in the window ending at the quantum of `now_ms`, if it has a
/// ledger.
pub(super) fn sender_load(admission: &OnionAdmissionState, from: u32, now_ms: u128) -> Option<u32> {
    admission
        .senders
        .get(&Did::from(from))
        .map(|sender| sender.ledger.load(now_ms / Q))
}

/// The live filter keys in expiry order.
pub(super) fn live_filters(admission: &OnionAdmissionState) -> Vec<OnionExpiry> {
    admission.replay.filters().keys().copied().collect()
}
