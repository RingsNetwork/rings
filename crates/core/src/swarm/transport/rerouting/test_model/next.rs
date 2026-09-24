//! `Next` of the rerouting model: which actions are enabled, and what each does.
//!
//! ```text
//! Env      ≜ Dial ∨ Glare ∨ Withdraw ∨ Die ∨ Disconnect ∨ Reroute(p) ∨ Congest ∨ Jam(h)
//!          ∨ FillPeer(h) ∨ Enqueue ∨ AcceptThenFail(a)              \* each spends Churn
//! Protocol ≜ Admit ∨ Recover ∨ Close ∨ Release ∨ Drain(h) ∨ Dequeue  \* WF
//!          ∨ Send ∨ Accept ∨ Refuse(r) ∨ Wake                        \* the automaton
//! ```
//!
//! The send path's lemmas fix which resolutions an in-flight send has (`resolutions`): a send
//! bound to a generation that lost its slot, or that cannot make progress, is refused before
//! acceptance; a send to a usable hop is refused by exhausted capacity, may time out in a
//! jammed channel, or is accepted; only an accepted send can fail ambiguously. Every
//! resolution is a production value classified by production code. Releases are scoped as in
//! production: a hop's `Drain` (and a `Close`) frees the hop's own capacity and advances its
//! peer epoch without clearing shared congestion; only `Release` clears shared congestion.
//! Capacity waits read `Room` (`State::has_room`), channel waits the peer's progress against
//! the stamp read after the refusal, so the refused send's own release is not an event.

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
use crate::swarm::transport::outbound::TransferDemand;

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
/// usable, the hop's capacity full     ─▶ { PeerFull }
/// usable, shared capacity exhausted
///   or a waiter queued on it          ─▶ { AdmissionTimeout }
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
        (Hop::Target | Hop::Alternate, _) if state.full[hop.index()] => {
            (vec![Refusal::PeerFull], false)
        }
        (Hop::Target | Hop::Alternate, _) if state.congested || state.queued => {
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
        actions.extend(state.queued.then_some(Action::Dequeue));
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
                peer,
                cause,
            } => {
                let observation = Observation {
                    room: state.has_room(hop),
                    peer: state.progress(hop, peer),
                };
                let triggered = awaiting(deferrals, hop, generation, cause)
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
        actions.extend((!state.queued && churn.queues > 0).then_some(Action::Enqueue));
        if churn.jams > 0 {
            actions.extend(HOPS.into_iter().map(Action::Jam));
        }
        if churn.fills > 0 {
            actions.extend(
                HOPS.into_iter()
                    .filter(|hop| state.jam[hop.index()] > 0 && !state.full[hop.index()])
                    .map(Action::FillPeer),
            );
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
            Action::FillPeer(hop) => {
                next.churn.fills -= 1;
                next.full[hop.index()] = true;
            }
            Action::Admit => {
                let pending = next.registry.pending_attempt(target)?;
                admit(&mut next.registry, pending);
                next.ready = true;
            }
            Action::Recover => next.ready = true,
            Action::Close => close(&mut next),
            Action::Release => next.congested = false,
            Action::Enqueue => {
                next.churn.queues -= 1;
                next.queued = true;
            }
            Action::Dequeue => next.queued = false,
            Action::Drain(hop) => release_hop(&mut next, *hop, 1),
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
                            demand: TransferDemand::for_test(),
                            cause: SendDeferral::cancelled(hop.did()),
                        },
                        Some(Refusal::Cancelled),
                    ),
                    Mutation::Faithful | Mutation::WakeUntriggered => (
                        Verdict::remote(
                            hop.did(),
                            generation,
                            TransferDemand::for_test(),
                            Err(ambiguity.error(hop.did())),
                        ),
                        None,
                    ),
                }
            }
            Resolution::Refuse(refusal) => {
                state.last_refusal = Some(refusal);
                let outcome = refusal.outcome(hop.did(), generation.unwrap_or_default());
                (
                    Verdict::remote(hop.did(), generation, TransferDemand::for_test(), outcome),
                    Some(refusal),
                )
            }
        };
        let resolved = Resolved {
            hop,
            peer: state.released[hop.index()],
        };
        state.phase = transition(deferrals, resolved, verdict, refusal);
    }
}

/// `count` other transfers of `hop` end: each releases its peer and global capacity, which
/// frees the hop's own capacity but not shared congestion.
fn release_hop(state: &mut State, hop: Hop, count: u8) {
    if count == 0 {
        return;
    }
    state.jam[hop.index()] -= count;
    state.full[hop.index()] = false;
    state.released[hop.index()] += u64::from(count);
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
    release_hop(state, Hop::Target, state.jam[Hop::Target.index()]);
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
            peer: 0,
        };
        state.phase = transition(deferrals, resolved, Verdict::local(Ok(())), None);
        return;
    };
    state.sends += 1;
    state.phase = Phase::InFlight {
        deferrals,
        hop,
        generation: state.generation(hop),
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
///       ∨ (Link           ∧ usable(hop))
///       ∨ (PeerCapacity   ∧ the hop's own capacity has room)
///       ∨ (GlobalCapacity ∧ the shared capacity has room and no waiter is queued on it)
///       ∨ (Drain          ∧ (generation(hop) ≠ bound ∨ the hop released ∨ idle(hop)))
/// ```
///
/// The capacity disjuncts are scoped by the refusal the model knows; `FreshHop` checks that
/// production's `Room` never wakes a capacity refusal whose own scope is still exhausted.
fn wake(state: &mut State) {
    let Phase::Waiting {
        deferrals,
        hop,
        generation,
        peer,
        cause,
    } = state.phase
    else {
        return;
    };
    let moved = state.route() != Some(hop);
    let progress = state.progress(hop, peer);
    let fresh = moved
        || match cause.trigger() {
            Trigger::Link => state.usable(hop),
            Trigger::PeerCapacity => !state.full[hop.index()],
            Trigger::GlobalCapacity => !state.congested && !state.queued,
            Trigger::Drain => {
                state.generation(hop) != generation || progress.released || progress.idle
            }
        };
    let capacity = matches!(
        cause.trigger(),
        Trigger::PeerCapacity | Trigger::GlobalCapacity
    );
    state.stale_retry |= !fresh;
    state.woke_on_route |= moved;
    state.woke_on_capacity |= !moved && capacity;
    state.woke_on_drain |= !moved && cause.trigger() == Trigger::Drain;
    state.phase = Phase::Compute {
        deferrals: awaiting(deferrals, hop, generation, cause)
            .resume()
            .deferrals,
    };
}
