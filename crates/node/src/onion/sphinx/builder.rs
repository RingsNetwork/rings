//! The client's loop builder: a route's positions, one application per symbol hop and a value
//! become the loop's first cell and the key its reply is opened with (#834 D4, D5, D6′, D7).
//!
//! The builder is the only owner of the segment seeds. For a loop of `n` symbol hops it draws
//! `σ_0 … σ_n` and derives from each `σ_k`, by [`OnionSegmentSeed::seeds`], the seeds the layers of
//! segment `k` carry, and from `σ_0` the keys it seals the first value with, so a layer's seed and
//! the producer's key cannot disagree (#845 L3). The positions are read in loop order, and the
//! seeds are assigned by the role of each position:
//!
//! ```text
//! σ ← σ_0 (drawn);  j ← 1                                    segment 0, produced by the client
//! for i = 1 … H, in loop order:
//!   relay     λ_i = (relay, (), next_i, e_i, x, ν_i, σ_in = σ_{k,j}, σ_out uniform);  j ← j + 1
//!   symbol k  σ_k ← drawn;
//!             λ_i = (f_k, ā_k, next_i, e_i, x, ν_i, σ_in = σ_{k−1,c}, σ_out = σ_k)
//!             σ ← σ_k;  j ← 1
//! next_H = the client (D6′);  (χ_1, t_⋄) ← header over (pk_i, λ_i)_{i=1…H}
//! cell = χ_1 ‖ seal_{keys(σ_0)}(v);  reply key k_{c_n} = KDF₄₈(σ_{n,c})
//! ```
//!
//! Laws (tested in `sphinx::tests::test_builder`):
//!
//! - **Seeds.** Every relay of segment `k` carries `KDF₃₂(σ_k, j)`, its consumer `KDF₃₂(σ_k, c)`,
//!   and the producer of segment `k` carries `σ_k` as `σ_out`; each relay's own `σ_out` is uniform.
//! - **Loop.** The layer at position `i` names the DID of position `i + 1`, the last names the
//!   client, and every layer carries the one expiry `x` and the epoch of its own hop.
//! - **Reply.** The client opens the returning value under the key derived from `σ_n`, and the
//!   loop returns with the tag `t_⋄` the builder hands back.
//!
//! A reply block for batched credit (D8) is the same construction over the return path alone:
//! one fresh segment `σ_υ` whose relays are the positions after `hₙ` and whose consumer is the
//! client, with its own fresh tag.

use std::array;

use rand::CryptoRng;
use rand::Rng;
use rand::RngCore;
use rings_aez::KeyError;
use rings_core::dht::Did;

use super::cell::OnionCell;
use super::cell::OnionClientError;
use super::cell::OnionSurb;
use super::class::OnionLoopClass;
use super::header::OnionHeader;
use super::header::OnionHeaderHop;
use super::header::OnionHeaderRoute;
use super::header::OnionHeaderRouteError;
use super::header::OnionLoopTag;
use super::layer::OnionArguments;
use super::layer::OnionLayer;
use super::layer::OnionLayerApplication;
use super::layer::OnionLayerHead;
use super::seed::OnionCarryKey;
use super::seed::OnionCarrySeed;
use super::seed::OnionSegmentSeed;
use crate::onion::circuit::OnionExpiry;
use crate::onion::circuit::OnionReplayNonce;
use crate::onion::OnionLoop;
use crate::onion::OnionLoopRole;
use crate::onion::OnionRouteHop;
use crate::onion::OnionServiceName;
use crate::onion::ONION_SEGMENT_RELAYS;

/// The application `(f_k, ā_k)` a loop's symbol hop `h_k` evaluates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnionApplication {
    /// `f_k`.
    pub(crate) symbol: OnionServiceName,
    /// `ā_k`, encoded to `A` bytes.
    pub(crate) arguments: OnionArguments,
}

/// What the client keeps for one returning cell (D6′): the tag it arrives with, the key its
/// value opens under, and the expiry after which it can no longer arrive.
pub(crate) struct OnionReplyKey {
    /// `t_⋄`.
    pub(crate) tag: OnionLoopTag,
    /// `k_{c_n}`.
    pub(crate) key: OnionCarryKey,
    /// `x`: the entry is dropped once it has passed.
    pub(crate) expiry: OnionExpiry,
}

/// A loop's first cell, where it goes, and the reply key of its return.
pub(crate) struct OnionBuiltLoop {
    /// The guard, position 1, which the client hands the cell to.
    pub(crate) guard: Did,
    /// `χ_1 ‖ y_0`.
    pub(crate) cell: OnionCell,
    /// The key the returning value opens under.
    pub(crate) reply: OnionReplyKey,
}

/// Why the builder produced no loop. Each failure has negligible probability or is a caller
/// error; nothing partial escapes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionBuildError {
    /// Every draw of a segment seed was weak (probability `≈ 2^−499`).
    #[error(transparent)]
    Key(#[from] KeyError),
    /// The positions are not a header route (a key off the curve, or a length out of range).
    #[error(transparent)]
    Route(#[from] OnionHeaderRouteError),
    /// The value is too wide, or a blinding factor was zero.
    #[error(transparent)]
    Client(#[from] OnionClientError),
    /// The applications do not match the loop's symbol positions, or a segment has more relays
    /// than `s`: the loop and the pipeline disagree.
    #[error("the applications do not match the loop's symbol positions")]
    Shape,
}

/// The seeds of the segment being built: `σ_k`'s relay seeds, still to be placed in visiting
/// order, and its consumer seed.
struct OnionOpenSegment {
    /// `σ_{k,j}` for the relays not yet placed, in visiting order.
    relays: array::IntoIter<OnionCarrySeed, ONION_SEGMENT_RELAYS>,
    /// `σ_{k,c}`.
    consumer: OnionCarrySeed,
}

impl OnionOpenSegment {
    /// The open segment of `σ_k`.
    fn of(seed: &OnionSegmentSeed) -> Self {
        let seeds = seed.seeds();
        Self {
            relays: seeds.relays.into_iter(),
            consumer: seeds.consumer,
        }
    }
}

/// Build the loop over `hops` that applies `applications` (one per symbol hop, in pipeline
/// order) and carries `value` into segment 0, in class `class`, expiring at `expiry`; the last
/// position hands the cell to `client`. See the module documentation for the construction.
///
/// # Errors
///
/// The [`OnionBuildError`] of the step that failed.
pub(crate) fn build_loop(
    hops: &OnionLoop<OnionRouteHop>,
    applications: &[OnionApplication],
    client: Did,
    class: OnionLoopClass,
    expiry: OnionExpiry,
    value: &[u8],
    rng: &mut (impl CryptoRng + RngCore),
) -> Result<OnionBuiltLoop, OnionBuildError> {
    let (first, first_keys) = OnionSegmentSeed::draw(rng)?;
    let mut segment = OnionOpenSegment::of(&first);
    let mut last = first;
    let roles = hops.roles().collect::<Vec<_>>();
    let nexts = roles
        .iter()
        .skip(1)
        .map(|(_, hop)| hop.did)
        .chain(core::iter::once(client));
    let mut positions = Vec::with_capacity(roles.len());
    for ((role, hop), next) in roles.iter().zip(nexts) {
        let (application, inbound, outbound) = match role {
            OnionLoopRole::Relay => (
                OnionLayerApplication::Relay,
                segment.relays.next().ok_or(OnionBuildError::Shape)?,
                OnionSegmentSeed::random(rng),
            ),
            OnionLoopRole::Symbol(k) => {
                let application = k
                    .checked_sub(1)
                    .and_then(|index| applications.get(index))
                    .ok_or(OnionBuildError::Shape)?;
                let (produced, _) = OnionSegmentSeed::draw(rng)?;
                let inbound = core::mem::replace(&mut segment, OnionOpenSegment::of(&produced));
                let outbound = OnionSegmentSeed::new(*produced.as_bytes());
                last = produced;
                (
                    OnionLayerApplication::Apply {
                        symbol: application.symbol.clone(),
                        arguments: application.arguments,
                    },
                    inbound.consumer,
                    outbound,
                )
            }
        };
        positions.push(OnionHeaderHop {
            public_key: hop.delegatee_public_key,
            layer: layer(application, next, hop, expiry, inbound, outbound, rng),
        });
    }
    if hops.shape().symbols() != applications.len() {
        return Err(OnionBuildError::Shape);
    }
    let route = OnionHeaderRoute::new(positions)?;
    let (cell, tag) = OnionCell::client(&route, class, &first_keys, value, rng)?;
    Ok(OnionBuiltLoop {
        guard: hops.guard().did,
        cell,
        reply: OnionReplyKey {
            tag,
            key: last.seeds().consumer.key()?,
            expiry,
        },
    })
}

/// Build a reply block over the return path `path` (the positions after `hₙ`, the guard last)
/// for batched credit (D8): one fresh segment `σ_υ` whose relays are `path` and whose consumer
/// is `client`, with a fresh tag, in class `class`, expiring at `expiry`.
///
/// # Errors
///
/// The [`OnionBuildError`] of the step that failed.
pub(crate) fn build_surb<'h>(
    path: impl IntoIterator<Item = &'h OnionRouteHop>,
    client: Did,
    class: OnionLoopClass,
    expiry: OnionExpiry,
    rng: &mut (impl CryptoRng + RngCore),
) -> Result<(OnionSurb, OnionReplyKey), OnionBuildError> {
    let path = path.into_iter().collect::<Vec<_>>();
    let (seed, _) = OnionSegmentSeed::draw(rng)?;
    let mut segment = OnionOpenSegment::of(&seed);
    let first = path
        .first()
        .map(|hop| hop.did)
        .ok_or(OnionBuildError::Shape)?;
    let nexts = path
        .iter()
        .skip(1)
        .map(|hop| hop.did)
        .chain(core::iter::once(client));
    let positions = path
        .iter()
        .zip(nexts)
        .map(|(hop, next)| {
            let inbound = segment.relays.next().ok_or(OnionBuildError::Shape)?;
            Ok(OnionHeaderHop {
                public_key: hop.delegatee_public_key,
                layer: layer(
                    OnionLayerApplication::Relay,
                    next,
                    hop,
                    expiry,
                    inbound,
                    OnionSegmentSeed::random(rng),
                    rng,
                ),
            })
        })
        .collect::<Result<Vec<_>, OnionBuildError>>()?;
    let (header, tag) = OnionHeader::build(&OnionHeaderRoute::new(positions)?, class, rng)
        .map_err(OnionClientError::from)?;
    let key = seed.seeds().consumer.key()?;
    Ok((
        OnionSurb::new(class, first, header, seed, expiry),
        OnionReplyKey { tag, key, expiry },
    ))
}

/// The layer of one position: its application, the next DID, its hop's epoch, the loop's
/// expiry, a fresh replay nonce, and its two seeds.
fn layer(
    application: OnionLayerApplication,
    next: Did,
    hop: &OnionRouteHop,
    expiry: OnionExpiry,
    inbound: OnionCarrySeed,
    outbound: OnionSegmentSeed,
    rng: &mut (impl CryptoRng + RngCore),
) -> OnionLayer {
    OnionLayer {
        head: OnionLayerHead {
            application,
            next,
            epoch: hop.process_epoch,
            expiry,
            nonce: OnionReplayNonce::new(rng.gen()),
        },
        inbound,
        outbound,
    }
}
