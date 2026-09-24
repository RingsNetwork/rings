//! `Next` of the rerouting model: which actions are enabled, and what each does.
//!
//! ```text
//! Env      ≜ Dial ∨ Glare ∨ Withdraw ∨ Die ∨ Disconnect ∨ Reroute(p) ∨ Congest ∨ Jam(h)
//!          ∨ AcceptThenFail(a)                                      \* each spends Churn
//! Protocol ≜ Admit ∨ Recover ∨ Close ∨ Release ∨ Drain(h)            \* WF
//!          ∨ Send ∨ Accept ∨ Refuse(r) ∨ Wake                        \* the automaton
//! ```
//!
//! The send path's lemmas fix which resolutions an in-flight send has (`resolutions`): a send
//! bound to a generation that lost its slot, or that cannot make progress, is refused before
//! acceptance; a send to a usable hop is refused by exhausted capacity, may time out in a
//! jammed channel, or is accepted; only an accepted send can fail ambiguously. Every
//! resolution is a production value classified by production code. The refused transfer's own
//! capacity release is not an event here: production excludes it from both stamps.

use super::super::CapacityStamp;
use super::super::Observation;
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
use super::carrier::Resolved;
use super::carrier::State;
use super::carrier::Trigger;
use super::carrier::AMBIGUITIES;
use crate::error::SendDeferral;

/// Every route preference, in action order.
const PREFERENCES: [Preference; 3] = [Preference::Target, Preference::Alternate, Preference::Local];

/// Both hops, in action order.
const HOPS: [Hop; 2] = [Hop::Target, Hop::Alternate];

/// The resolutions of a send to `hop` bound to `generation` in `state`: refusals, and
/// whether acceptance is possible.
///
/// ```text
/// Target, no generation bound         ─▶ { Missing }
/// Target, generation lost its slot    ─▶ { Superseded, PermitRevoked, Cancelled }
/// Target, generation not ready        ─▶ { NotReady, PermitRevoked, Missing }
/// usable, capacity exhausted          ─▶ { AdmissionTimeout }
/// usable, channel jammed              ─▶ { QueueTimeout } and accept
/// usable                              ─▶ accept
/// ```
fn resolutions(state: &State, hop: Hop, generation: Option<u64>) -> (Vec<Refusal>, bool) {
    match (hop, generation) {
        (Hop::Target, None) => (vec![Refusal::Missing], false),
        (Hop::Target, Some(_)) if state.generation(Hop::Target) != generation => (
            vec![
                Refusal::Superseded,
                Refusal::PermitRevoked,
                Refusal::Cancelled,
            ],
            false,
        ),
        (Hop::Target, Some(_)) if !state.ready => (
            vec![Refusal::NotReady, Refusal::PermitRevoked, Refusal::Missing],
            false,
        ),
        (Hop::Target | Hop::Alternate, _) if state.congested => {
            (vec![Refusal::AdmissionTimeout], false)
        }
        (Hop::Target | Hop::Alternate, _) if state.jam[hop.index()] > 0 => {
            (vec![Refusal::QueueTimeout], true)
        }
        (Hop::Target | Hop::Alternate, _) => (Vec::new(), true),
    }
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
        actions.extend(
            HOPS.into_iter()
                .filter(|hop| state.jam[hop.index()] > 0)
                .map(Action::Drain),
        );
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
                generation,
                stamp,
                cause,
            } => {
                let observation = Observation {
                    capacity: CapacityStamp(state.capacity),
                    idle: state.is_idle(hop),
                };
                let triggered = awaiting(deferrals, hop, generation, stamp, cause)
                    .is_triggered(state.link_route(), observation);
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
        if churn.jams > 0 {
            actions.extend(HOPS.into_iter().map(Action::Jam));
        }
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
            Action::Jam(hop) => {
                next.churn.jams -= 1;
                next.jam[hop.index()] += 1;
            }
            Action::Admit => {
                let pending = next.registry.pending_attempt(target)?;
                admit(&mut next.registry, pending);
                next.ready = true;
            }
            Action::Recover => next.ready = true,
            Action::Close => close(&mut next),
            Action::Release => {
                next.congested = false;
                next.capacity += 1;
            }
            Action::Drain(hop) => next.jam[hop.index()] -= 1,
            Action::Send => send(&mut next),
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
                            generation,
                            cause: SendDeferral::cancelled(hop.did()),
                        },
                        Some(Refusal::Cancelled),
                    ),
                    Mutation::Faithful | Mutation::WakeUntriggered => (
                        Verdict::remote(hop.did(), generation, Err(ambiguity.error(hop.did()))),
                        None,
                    ),
                }
            }
            Resolution::Refuse(refusal) => {
                state.last_refusal = Some(refusal);
                let outcome = refusal.outcome(hop.did(), generation.unwrap_or_default());
                (
                    Verdict::remote(hop.did(), generation, outcome),
                    Some(refusal),
                )
            }
        };
        let resolved = Resolved { hop, stamp };
        state.phase = transition(deferrals, resolved, verdict, refusal);
    }
}

/// `Close`: the dead generation retires; its channel's other transfers are cancelled and
/// release their capacity, and the channel is gone.
fn close(state: &mut State) {
    let target = Hop::Target.did();
    let Some(admitted) = state.registry.active_attempt(target) else {
        return;
    };
    let retired = state.registry.retire_active_if(admitted, |_| Ok(Some(())));
    assert!(
        retired.is_ok_and(|outcome| outcome.is_retired()),
        "a dead generation retires"
    );
    state.jam[Hop::Target.index()] = 0;
}

/// `Compute`: settle locally, or bind a send to the routed hop.
fn send(state: &mut State) {
    let Phase::Compute { deferrals } = state.phase else {
        return;
    };
    let Some(hop) = state.route() else {
        // A local settlement is the placement's effect; `δ` ignores where a local verdict
        // stood, so any hop stands in for it.
        state.effects += 1;
        let resolved = Resolved {
            hop: Hop::Alternate,
            stamp: state.capacity,
        };
        state.phase = transition(deferrals, resolved, Verdict::local(Ok(())), None);
        return;
    };
    state.sends += 1;
    state.phase = Phase::InFlight {
        deferrals,
        hop,
        generation: state.generation(hop),
        stamp: state.capacity,
    };
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
/// Fresh ≜ route ≠ hop
///       ∨ (Link     ∧ usable(hop))
///       ∨ (Capacity ∧ capacity > stamp)
///       ∨ (Drain    ∧ (generation(hop) ≠ bound ∨ idle(hop)))
/// ```
fn wake(state: &mut State) {
    let Phase::Waiting {
        deferrals,
        hop,
        generation,
        stamp,
        cause,
    } = state.phase
    else {
        return;
    };
    let moved = state.route() != Some(hop);
    let released = state.capacity > stamp;
    let fresh = moved
        || match cause.trigger() {
            Trigger::Link => state.usable(hop),
            Trigger::Capacity => released,
            Trigger::Drain => state.generation(hop) != generation || state.is_idle(hop),
        };
    state.stale_retry |= !fresh;
    state.woke_on_route |= moved;
    state.woke_on_capacity |= !moved && released && cause.trigger() == Trigger::Capacity;
    state.woke_on_drain |= !moved && cause.trigger() == Trigger::Drain;
    state.phase = Phase::Compute {
        deferrals: awaiting(deferrals, hop, generation, stamp, cause)
            .resume()
            .deferrals,
    };
}
