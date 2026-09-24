//! The rerouting driver: a [`Placement`] routes and settles, [`reroute`] performs the sends
//! and the waits the automaton decides.

use std::pin::pin;
use std::sync::Arc;

use futures::future::select;

use super::shell::observation;
use super::Awaiting;
use super::LinkRoute;
use super::Rerouting;
use super::Step;
use super::Verdict;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::message::types::Message;
use crate::swarm::transport::SwarmTransport;

/// Where one placement goes under the topology now.
pub(crate) enum Route<Local> {
    /// This node settles the placement with `Local`.
    Local(Local),
    /// Send `message` toward `next`.
    Remote {
        /// The node the message is addressed to.
        next: Did,
        /// The placement's message under this route, boxed so a route is small beside a local
        /// settlement (serialization is transparent through the box).
        message: Box<Message>,
    },
}

/// One remote placement of a user DHT operation: the unit that reroutes.
pub(crate) trait Placement {
    /// What a placement settled here carries.
    type Local;

    /// `Compute`: route this placement under the topology (and local storage) now.
    async fn route(&self, dht: &PeerRing) -> Result<Route<Self::Local>>;

    /// Settle a placement whose route is local.
    async fn settle(&self, transport: &Arc<SwarmTransport>, local: Self::Local) -> Result<()>;
}

/// How many attempts a placement may make.
///
/// Clone law: a clone shares the stop source of the original (`StopToken`), so every placement
/// of one operation observes the same stop.
#[derive(Clone)]
pub(crate) enum Attempts {
    /// Reroute within `REROUTING_BUDGET`; a wait ends once the stop is requested.
    Rerouted(StopToken),
    /// One attempt, as before #859: a refusal ends the placement with `ReroutingExhausted`
    /// carrying it, and nothing waits. For writes originated on the inbound path (relay holds,
    /// inbox retirement), which must never hold an inbound lane on a rerouting wait.
    Single,
}

/// Drive `placement` from its planned `first` route to completion.
///
/// ```text
/// reroute(P, first, attempts):
///   R ← start (Rerouted) | spent (Single) ; route ← first
///   loop
///     verdict ← route = Local(l)        ⇒ Verdict::local(P.settle(l))
///               route = Remote(next, m) ⇒ attempt_remote(m, next)
///     case δ(R, verdict) of
///       Complete  ⇒ return Ok
///       Fail(e)   ⇒ return Err(e)                \* fatal, ambiguous, or exhausted
///       Await(A)  ⇒ route ← await_trigger(P, A)  \* or Err(ReroutingStopped)
///                   R ← A.resume
/// ```
///
/// Post: `Ok(())` iff one attempt was accepted or settled locally; every earlier attempt was
/// refused before acceptance (S1). `Err(ReroutingExhausted { .. })` after
/// `REROUTING_BUDGET + 1` refused sends (S3), or after the one refused send of
/// `Attempts::Single`; `Err(ReroutingStopped)` as [`await_trigger`] states.
pub(crate) async fn reroute<P: Placement>(
    transport: &Arc<SwarmTransport>,
    placement: &P,
    first: Route<P::Local>,
    attempts: Attempts,
) -> Result<()> {
    let (mut rerouting, stop) = match attempts {
        Attempts::Rerouted(stop) => (Rerouting::start(), stop),
        Attempts::Single => (Rerouting::spent(), StopToken::never()),
    };
    let mut route = first;
    loop {
        let verdict = match route {
            Route::Local(local) => Verdict::local(placement.settle(transport, local).await),
            Route::Remote { next, message } => transport.attempt_remote(message, next).await,
        };
        let awaiting = match rerouting.after(verdict) {
            Step::Complete => return Ok(()),
            Step::Fail(error) => return Err(error),
            Step::Await(awaiting) => awaiting,
        };
        route = await_trigger(transport, placement, &awaiting, &stop).await?;
        rerouting = awaiting.resume();
    }
}

/// `Waiting → Compute`: the first route of `placement`, computed after listening, under which
/// `awaiting` is triggered.
///
/// ```text
/// peer ← stamp of A's hop                         \* after the refusal was published
/// loop
///   stop requested       ⇒ return Err(ReroutingStopped)
///   C ← capacity view of A's hop                   \* one reading per iteration
///   listen(A, peer, C) ; r ← P.route(topology now)
///   A.is_triggered(r, observation(A, peer, C)) ⇒ return r
///   await first(listened event, stop)
/// ```
///
/// Post: `Err(ReroutingStopped)` iff `stop` was requested before an iteration began; no send
/// follows a stop observed here. `stop` is observed only here, where no send is in flight and
/// no other await is pending, so stopping is cooperative: nothing but the wait is interrupted.
/// The capacity view is dropped before the wait awaits, so a waiting placement keeps no
/// capacity alive.
async fn await_trigger<P: Placement>(
    transport: &Arc<SwarmTransport>,
    placement: &P,
    awaiting: &Awaiting,
    stop: &StopToken,
) -> Result<Route<P::Local>> {
    let peer = transport.peer_stamp(awaiting);
    loop {
        if stop.should_stop() {
            return Err(Error::ReroutingStopped);
        }
        let (listeners, observed, fresh) = {
            let capacity = transport.capacity_view(awaiting);
            let listeners = transport.rerouting_listeners(awaiting, &peer, &capacity);
            let fresh = placement.route(&transport.dht).await?;
            (listeners, observation(awaiting, &peer, &capacity), fresh)
        };
        let link = match &fresh {
            Route::Local(_) => LinkRoute::Local,
            Route::Remote { next, .. } => LinkRoute::Remote(transport.link_hop(*next)?),
        };
        if awaiting.is_triggered(link, observed) {
            return Ok(fresh);
        }
        select(pin!(listeners.notified()), pin!(stop.stopped())).await;
    }
}
