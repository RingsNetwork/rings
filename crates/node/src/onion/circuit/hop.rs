//! `Hop_i`, one position of a loop, as a pure step over the hop's admission state (#834 D6,
//! L9; paper Algorithm Hop).
//!
//! ```text
//! cell from `from` at arr
//!   │ link ← the live link of `from`;  tk ← charge(link, u(b))        else Refused (no ECDH)
//!   ▼
//! γ = a live client tag t_⋄ ? ──yes──▶ Returned(t_⋄, cell)            position H + 1 (D6′)
//!   │ no
//!   ▼
//! peel χ under d_i (α, ECDH, γ)                                        else Dropped(Peel)
//! admit(tk, e, x, ν)                                                   else Dropped(Admission)
//! relay ∧ this node relays nothing                                     ⇒ Dropped(NotRelay)
//! step (AEZ layer off, or the consumer's open)                         else Dropped(Step)
//!   ├─ relay   ⇒ Relayed(next, cell)          if next is a live link, else Dropped(NextNotLive)
//!   └─ f ∈ Σ_W ⇒ Consumed(f, ā, v, υ)
//! ```
//!
//! The step has no effect: time, the key and the client's tag table are arguments, and the only
//! state it changes is the admission state it is handed, so a trace of inputs determines the
//! trace of outcomes. The shell performs each outcome (send, deliver, dispatch).
//!
//! Laws (tested in `circuit::tests::test_hop`):
//!
//! - **Paid.** Every outcome but `Refused` charged `u(b)` exactly once, before any ECDH; a
//!   `Refused` cell was neither charged nor peeled.
//! - **At most once** (L9). A layer `(x, ν)` reaches `Relayed` or `Consumed` at most once per
//!   epoch; its replay is `Dropped(Admission(Replayed))`.
//! - **Identity** (L1). A `Relayed` cell has the received class and length, and its carry is the
//!   received carry with one AEZ layer removed.
//! - **Adjacency.** A `Relayed` cell's `next` is a live link, so a layer cannot make this node
//!   emit toward a peer it has no link to.

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;

use super::admission::OnionAdmissionRejection;
use super::admission::OnionAdmissionState;
use super::admission::OnionChargeRejection;
use crate::onion::sphinx::carry::OnionCarryValue;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::cell::OnionStep;
use crate::onion::sphinx::cell::OnionStepError;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::header::OnionLoopTag;
use crate::onion::sphinx::header::OnionPeelError;
use crate::onion::sphinx::layer::OnionArguments;
use crate::onion::sphinx::layer::OnionLayerApplication;
use crate::onion::OnionServiceName;

/// What one received cell became at this hop.
#[derive(Debug)]
pub(crate) enum OnionHopOutcome {
    /// The cell was not charged, so it was not decrypted either.
    Refused(OnionChargeRejection),
    /// The cell was charged and then dropped.
    Dropped(OnionHopDrop),
    /// A relay layer: forward the cell to `next`.
    Relayed {
        /// `next_i`.
        next: Did,
        /// The cell, one layer peeled, in the received buffer.
        cell: OnionCell,
    },
    /// A symbol layer: the application `(f, ā)`, its input and the reply block of its output.
    Consumed {
        /// `f`.
        symbol: OnionServiceName,
        /// `ā`.
        arguments: OnionArguments,
        /// `v`.
        value: OnionCarryValue,
        /// `υ`.
        surb: Box<OnionSurb>,
    },
    /// A cell for this node as the client of a loop: its tag `t_⋄` names a live entry.
    Returned {
        /// `t_⋄`.
        tag: OnionLoopTag,
        /// The returning cell, undecrypted: its entry holds the key.
        cell: OnionCell,
    },
}

/// Why a charged cell was dropped.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OnionHopDrop {
    /// Its header did not peel (an invalid `α` or `γ`, cover included).
    #[error(transparent)]
    Peel(#[from] OnionPeelError),
    /// Admission refused its layer (epoch, window or replay).
    #[error("admission refused the layer: {0:?}")]
    Admission(OnionAdmissionRejection),
    /// A relay layer reached a node that registers no `relay`.
    #[error("relay layer at a node that relays nothing")]
    NotRelay,
    /// A relay layer names a `next` that is not a live link of this node.
    #[error("relay layer names a next hop that is not a live link")]
    NextNotLive,
    /// Its carry step failed (a weak key, or the consumer's check).
    #[error(transparent)]
    Step(#[from] OnionStepError),
}

/// Run `Hop_i` on one cell received from `from` at `now`; see the module diagram. `relays` is
/// whether this node registers `relay`, and `is_client_tag` recognises the tags of its own loops.
pub(crate) fn hop(
    admission: &mut OnionAdmissionState,
    key: &DelegateeKey,
    relays: bool,
    is_client_tag: impl Fn(&OnionLoopTag) -> bool,
    from: Did,
    now_ms: u128,
    cell: OnionCell,
) -> OnionHopOutcome {
    let Some(link) = admission.live_link(from) else {
        return OnionHopOutcome::Refused(OnionChargeRejection::LinkNotLive);
    };
    let charged = match admission.charge(now_ms, link, cell) {
        Ok(charged) => charged,
        Err(rejection) => return OnionHopOutcome::Refused(rejection),
    };
    let tag = charged.value().loop_tag();
    if is_client_tag(&tag) {
        return OnionHopOutcome::Returned {
            tag,
            cell: charged.settle(),
        };
    }
    let admitted = match charged.peel(key) {
        Ok(peeled) => match peeled.admit(admission, now_ms) {
            Ok(admitted) => admitted,
            Err(rejection) => return OnionHopOutcome::Dropped(OnionHopDrop::Admission(rejection)),
        },
        Err(error) => return OnionHopOutcome::Dropped(error.into()),
    };
    if !relays && admitted.head().application == OnionLayerApplication::Relay {
        return OnionHopOutcome::Dropped(OnionHopDrop::NotRelay);
    }
    match admitted.step() {
        Ok(OnionStep::Relayed { next, .. }) if admission.live_link(next).is_none() => {
            OnionHopOutcome::Dropped(OnionHopDrop::NextNotLive)
        }
        Ok(OnionStep::Relayed { next, cell }) => OnionHopOutcome::Relayed { next, cell },
        Ok(OnionStep::Consumed {
            symbol,
            arguments,
            value,
            surb,
        }) => OnionHopOutcome::Consumed {
            symbol,
            arguments,
            value,
            surb,
        },
        Err(error) => OnionHopOutcome::Dropped(error.into()),
    }
}
