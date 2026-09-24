//! `Next` of the rerouting model: which actions are enabled, and what each does.
//!
//! ```text
//! Env      ≜ Dial ∨ Glare ∨ Withdraw ∨ Die ∨ Disconnect ∨ Reroute(p) ∨ Congest
//!          ∨ AcceptThenFail(a)                                  \* each spends Churn
//! Protocol ≜ Admit ∨ Recover ∨ Close ∨ Release                   \* WF: readiness, close, release
//!          ∨ Send ∨ Accept ∨ Refuse(r) ∨ Wake                    \* the automaton
//! ```
//!
//! The send path's lemmas fix which resolutions an in-flight send has (`resolutions`): a send
//! bound to a generation that lost its slot, or that cannot make progress, is refused before
//! acceptance; a send to a usable hop meets exhausted capacity or is accepted, and only an
//! accepted send can fail ambiguously. Every resolution is a production value classified by
//! production code.

use super::super::CapacityStamp;
use super::super::Verdict;
use super::carrier::admit;
use super::carrier::awaiting;
use super::carrier::transition;
use super::carrier::Action;
use super::carrier::Ambiguity;
use super::carrier::Hop;
use super::carrier::Model;
use super::carrier::Mutation;
use super::carrier::Phase;
use super::carrier::Preference;
use super::carrier::Refusal;
use super::carrier::State;
use super::carrier::AMBIGUITIES;
use crate::error::SendDeferral;

/// Every route preference, in action order.
const PREFERENCES: [Preference; 3] = [Preference::Target, Preference::Alternate, Preference::Local];

/// Whether `refusal` waits for released capacity rather than a link change: the model's own
/// reading of the trigger, independent of `send_class`.
const fn waits_for_capacity(refusal: Refusal) -> bool {
    matches!(refusal, Refusal::AdmissionTimeout | Refusal::QueueTimeout)
}

/// The resolutions of a send to `hop` bound to `generation` in `state`: refusals, and
/// whether acceptance is possible.
///
/// ```text
/// Target, no generation bound         ─▶ { Missing }
/// Target, generation lost its slot    ─▶ { Superseded, PermitRevoked, Cancelled }
/// Target, generation not ready        ─▶ { NotReady, PermitRevoked, Missing }
/// usable, capacity exhausted          ─▶ { AdmissionTimeout, QueueTimeout }
/// usable                              ─▶ accept
/// ```
fn resolutions(state: &State, hop: Hop, generation: Option<u64>) -> (Vec<Refusal>, bool) {
    let sendable = state
        .registry
        .sendable_attempt(Hop::Target.did())
        .map(|attempt| attempt.generation());
    let refusals = match (hop, generation) {
        (Hop::Target, None) => vec![Refusal::Missing],
        (Hop::Target, Some(bound)) if sendable != Some(bound) => {
            vec![
                Refusal::Superseded,
                Refusal::PermitRevoked,
                Refusal::Cancelled,
            ]
        }
        (Hop::Target, Some(_)) if !state.ready => {
            vec![Refusal::NotReady, Refusal::PermitRevoked, Refusal::Missing]
        }
        (Hop::Target | Hop::Alternate, _) if state.congested => {
            vec![Refusal::AdmissionTimeout, Refusal::QueueTimeout]
        }
        (Hop::Target | Hop::Alternate, _) => return (Vec::new(), true),
    };
    (refusals, false)
}

impl Model {
    /// The actions enabled at `state`, in a fixed order; none once the placement is done.
    pub(super) fn actions(&self, state: &State) -> Vec<Action> {
        if matches!(state.phase, Phase::Done(_)) {
            return Vec::new();
        }
        let mut actions = self.environment_actions(state);
        let target = Hop::Target.did();
        let admitted = state.registry.active_attempt(target);
        let sendable = state.registry.sendable_attempt(target);
        let pending = state.registry.pending_attempt(target);
        actions.extend(pending.map(|_| Action::Admit));
        actions.extend(sendable.filter(|_| !state.ready).map(|_| Action::Recover));
        actions.extend(
            admitted
                .filter(|_| sendable.is_none())
                .map(|_| Action::Close),
        );
        actions.extend(state.congested.then_some(Action::Release));
        match state.phase {
            Phase::Compute { .. } => actions.push(Action::Send),
            Phase::InFlight {
                hop, generation, ..
            } => {
                let (refusals, acceptable) = resolutions(state, hop, generation);
                actions.extend(refusals.into_iter().map(Action::Refuse));
                actions.extend(acceptable.then_some(Action::Accept));
            }
            Phase::Waiting {
                deferrals,
                hop,
                stamp,
                cause,
            } => {
                let triggered = awaiting(deferrals, hop, stamp, cause)
                    .is_triggered(state.link_route(), CapacityStamp(state.capacity));
                if triggered || self.mutation == Mutation::WakeUntriggered {
                    actions.push(Action::Wake);
                }
            }
            Phase::Done(_) => {}
        }
        actions
    }

    /// The environment's enabled actions: each needs budget and a state it applies to.
    fn environment_actions(&self, state: &State) -> Vec<Action> {
        let churn = state.churn;
        let target = Hop::Target.did();
        let absent = !state.registry.contains(target);
        let pending = state.registry.pending_attempt(target).is_some();
        let sendable = state.registry.sendable_attempt(target).is_some();
        let mut actions = Vec::new();
        actions.extend((absent && churn.reservations > 0).then_some(Action::Dial));
        actions.extend(
            (pending && churn.glare > 0 && churn.reservations > 0).then_some(Action::Glare),
        );
        actions.extend((pending && churn.withdrawals > 0).then_some(Action::Withdraw));
        actions.extend((sendable && churn.deaths > 0).then_some(Action::Die));
        actions.extend(
            (sendable && state.ready && churn.disconnects > 0).then_some(Action::Disconnect),
        );
        if churn.reroutes > 0 {
            actions.extend(
                PREFERENCES
                    .into_iter()
                    .filter(|preference| *preference != state.preference)
                    .map(Action::Reroute),
            );
        }
        actions.extend((!state.congested && churn.congestions > 0).then_some(Action::Congest));
        if let Phase::InFlight {
            hop, generation, ..
        } = state.phase
        {
            if resolutions(state, hop, generation).1 && churn.ambiguities > 0 {
                actions.extend(AMBIGUITIES.into_iter().map(Action::AcceptThenFail));
            }
        }
        actions
    }

    /// The state `action` leads to from `state`.
    ///
    /// Pre: `action ∈ actions(state)`; every arm below is total on its enabling condition.
    pub(super) fn next_state(&self, state: &State, action: &Action) -> Option<State> {
        let mut next = state.clone();
        let target = Hop::Target.did();
        match action {
            Action::Dial => {
                next.churn.reservations -= 1;
                next.registry.reserve(target, 0).ok()?;
            }
            Action::Glare => {
                let pending = next.registry.pending_attempt(target)?;
                next.churn.glare -= 1;
                next.churn.reservations -= 1;
                next.registry.remove_unadmitted(pending);
                next.registry.reserve(target, 0).ok()?;
            }
            Action::Withdraw => {
                let pending = next.registry.pending_attempt(target)?;
                next.churn.withdrawals -= 1;
                next.registry.remove_unadmitted(pending);
            }
            Action::Die => {
                let sendable = next.registry.sendable_attempt(target)?;
                next.churn.deaths -= 1;
                next.registry.mark_send_terminal(sendable);
                next.ready = false;
            }
            Action::Disconnect => {
                next.churn.disconnects -= 1;
                next.ready = false;
            }
            Action::Reroute(preference) => {
                next.churn.reroutes -= 1;
                next.preference = *preference;
            }
            Action::Congest => {
                next.churn.congestions -= 1;
                next.congested = true;
            }
            Action::Admit => {
                let pending = next.registry.pending_attempt(target)?;
                admit(&mut next.registry, pending);
                next.ready = true;
            }
            Action::Recover => next.ready = true,
            Action::Close => {
                let admitted = next.registry.active_attempt(target)?;
                let retired = next
                    .registry
                    .retire_active_if(admitted, |_| Ok(Some(())))
                    .ok()?;
                assert!(retired.is_retired(), "a dead generation retires");
            }
            Action::Release => {
                next.congested = false;
                next.capacity += 1;
            }
            Action::Send => self.send(&mut next),
            Action::Accept => self.resolve(&mut next, Resolution::Accept),
            Action::Refuse(refusal) => self.resolve(&mut next, Resolution::Refuse(*refusal)),
            Action::AcceptThenFail(ambiguity) => {
                next.churn.ambiguities -= 1;
                self.resolve(&mut next, Resolution::AcceptThenFail(*ambiguity));
            }
            Action::Wake => wake(&mut next),
        }
        Some(next)
    }

    /// `Compute`: settle locally, or bind a send to the routed hop.
    fn send(&self, state: &mut State) {
        let Phase::Compute { deferrals } = state.phase else {
            return;
        };
        let Some(hop) = state.route() else {
            // A local settlement is the placement's effect; `δ` ignores the hop of a local
            // verdict, so any hop stands in for it.
            state.effects += 1;
            let verdict = Verdict::local(Ok(()));
            state.phase = transition(deferrals, Hop::Alternate, state.capacity, verdict, None);
            return;
        };
        let generation = match hop {
            Hop::Target => state
                .registry
                .sendable_attempt(Hop::Target.did())
                .map(|attempt| attempt.generation()),
            Hop::Alternate => None,
        };
        state.sends += 1;
        state.phase = Phase::InFlight {
            deferrals,
            hop,
            generation,
            stamp: state.capacity,
        };
    }

    /// Resolve the in-flight send and run the production `δ` on its verdict.
    fn resolve(&self, state: &mut State, resolution: Resolution) {
        let Phase::InFlight {
            deferrals,
            hop,
            generation,
            stamp,
        } = state.phase
        else {
            return;
        };
        let (verdict, refusal) = match resolution {
            Resolution::Accept => {
                state.effects += 1;
                state.reached_replacement |=
                    hop == Hop::Target && generation > Some(1) && state.last_refusal.is_some();
                (Verdict::Accepted, None)
            }
            Resolution::AcceptThenFail(ambiguity) => {
                state.effects += 1;
                match self.mutation {
                    Mutation::RetryAmbiguous => (
                        Verdict::Deferred {
                            hop: hop.did(),
                            cause: SendDeferral::cancelled(hop.did()),
                        },
                        Some(Refusal::Cancelled),
                    ),
                    Mutation::Faithful | Mutation::WakeUntriggered => (
                        Verdict::remote(hop.did(), Err(ambiguity.error(hop.did()))),
                        None,
                    ),
                }
            }
            Resolution::Refuse(refusal) => {
                state.last_refusal = Some(refusal);
                let outcome = refusal.outcome(hop.did(), generation.unwrap_or_default());
                (Verdict::remote(hop.did(), outcome), Some(refusal))
            }
        };
        state.phase = transition(deferrals, hop, stamp, verdict, refusal);
    }
}

/// How an in-flight send resolves.
#[derive(Clone, Copy)]
enum Resolution {
    /// The backend accepted.
    Accept,
    /// Refused before acceptance.
    Refuse(Refusal),
    /// Accepted, then failed ambiguously.
    AcceptThenFail(Ambiguity),
}

/// `Waiting → Compute`, recording whether the model's own freshness condition held.
///
/// ```text
/// Fresh ≜ route ≠ hop ∨ (capacity trigger ∧ capacity > stamp) ∨ (link trigger ∧ usable(hop))
/// ```
fn wake(state: &mut State) {
    let Phase::Waiting {
        deferrals,
        hop,
        stamp,
        cause,
    } = state.phase
    else {
        return;
    };
    let moved = state.route() != Some(hop);
    let released = state.capacity > stamp;
    let fresh = moved
        || if waits_for_capacity(cause) {
            released
        } else {
            state.usable(hop)
        };
    state.stale_retry |= !fresh;
    state.woke_on_route |= moved;
    state.woke_on_capacity |= !moved && released && waits_for_capacity(cause);
    state.phase = Phase::Compute {
        deferrals: awaiting(deferrals, hop, stamp, cause).resume().deferrals,
    };
}
