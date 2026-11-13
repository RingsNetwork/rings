//! The exit's session shell: a world-facing symbol's sessions at `h`, each driven by the pure
//! machine of `session::exit` over one world (#834 D2′, D8; paper Algorithms Hop and Reply).
//!
//! ```text
//! ⟦f⟧(from, ā, v, υ):  (ς, d) ← ā;  frame ← dec(v)
//!                      ς known with d ?  ⇒ its driver ← (frame, υ)
//!                                          (queue full: sequenced frame ⇒ bury ς, unheld;
//!                                           credit ⇒ drop;  driver gone ⇒ unheld)
//!                      ς known, other d  ⇒ drop (D2′: a loop naming another target)
//!                      ς closed within V ⇒ unheld (a tombstone: a late loop never respawns ς)
//!                      ς new, frame T    ⇒ lease(from) ⇒ spawn a driver, driver ← (frame, υ)
//!                                          (no slot or no lease ⇒ unheld: a refusal)
//!                      ς new, other      ⇒ drop (it may overtake its own open)
//! unheld(frame, υ):    reply abort(0) sealed under υ, unless frame is the client's abort
//!
//! driver(ς):  loop select
//!               (frame, υ)  ─▶ machine.forward
//!               world read  ─▶ machine.world            (only while reply_capacity = Some;
//!                                                        the reader records its own bytes)
//!               tick        ─▶ machine.tick
//!             perform each effect in order:
//!               Open(t)     policy ∧ world.open(t) ─▶ machine.opened(ok)
//!               Write(w)    world ← w (counted against the byte policy)
//!               Reply(f, υ) link sender ← seal(f, υ), awaited: the world is read at the link's
//!                           rate; a seal that fails fails the session closed
//!               Close       end the driver, release the lease and the world
//!             inbound closed (buried by a fate below) ─▶ machine.fail: abort, then Close
//!
//! lose(p):    bury every live ς whose loops arrive from p (`Law (Circuit fate)`, #963, #966)
//! ```
//!
//! Laws: the pure machine's (Credit, Totality, Binding, Ack, Order, Fail closed), and:
//!
//! - **Isolation.** A session's reply blocks live in its own pool, so no two sessions share a
//!   block, and a session's inputs reach only its own driver.
//! - **Liveness of failure.** A loop of a session `h` closed or refused is answered with `abort`
//!   under its own block, so a client whose session closed without a reply (no block left)
//!   learns of it at its next loop, at the latest its `V/2` keep-alive, which lands inside the
//!   `V`-long tombstone; a session never opened is left to the client's open timeout. A
//!   sequenced frame is never dropped for queue room: the session fails closed instead. An
//!   idle session's return segment is witnessed by the machine's echo (Law Echo), which this
//!   shell sends as any other reply: a dead return link fails `ς` at the client by its
//!   deadline `Δ = 2V`, and the client's `abort` closes it here; a dead forward link closes it
//!   here after `V` without a forward loop, and at the client by its deadline.
//! - **Pause.** The world is read only when the machine reports a capacity, so a `tcp` session
//!   with no credit leaves its socket unread, and resumes on credit.
//! - **Fate.** A retired link, or a message dropped here from a peer whose link stays up, ends
//!   every session whose loops arrive from that peer: the session is buried, and its driver
//!   fails it closed, replying `abort` under a block it holds, so the client learns of it at
//!   once, or at its next loop, which the tombstone answers with `abort`. A session whose reply
//!   cannot leave fails closed the same way.
//! - **Bound.** Only a loop that can open a session (`data` or `fin` with `T`) creates one, and
//!   it is leased against its previous hop's share before its driver runs, so no previous hop
//!   holds more than its share of the table, bound or not; each driver's inbound queue holds at
//!   most `ONION_EXIT_SESSION_INBOUND` loops, since every loop goes through the one stored
//!   sender.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use futures::channel::mpsc;
use futures::future::Fuse;
use futures::future::FusedFuture;
use futures::FutureExt;
use futures::StreamExt;
use rings_core::dht::Did;
use rings_core::utils::SharedClock;
use rings_core::utils::SystemClock;
use rings_runtime::MaybeSend;
use rings_runtime::MaybeSendSync;

use super::exit::reply_to_unheld;
use super::exit::seal;
use super::exit::OnionExitEffect;
use super::exit::OnionExitSession;
use super::exit::OnionWorldRead;
use super::frame::OnionFrame;
use super::session_sleep;
use super::OnionSessionArguments;
use super::OnionSessionId;
use super::OnionTargetDigest;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::circuit::OnionApplicationInput;
use crate::onion::circuit::OnionInterpretation;
use crate::onion::circuit::OnionLink;
use crate::onion::circuit::OnionLinkSender;
use crate::onion::circuit::ONION_ADMISSION_WINDOW_MS;
use crate::onion::exit_accounting::OnionExitAccounting;
use crate::onion::exit_accounting::OnionExitLease;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionProxyTarget;
use crate::sync_lock::lock;

/// Most sessions one world-facing symbol keeps at once, open or not yet bound.
const ONION_EXIT_MAX_SESSIONS: usize = 1_024;

/// Forward loops buffered for one session driver: a full window of credit.
const ONION_EXIT_SESSION_INBOUND: usize = 256;

/// The period of a session's tick: its idle and gap checks run at least this often.
const ONION_EXIT_SESSION_TICK: Duration = Duration::from_secs(10);

/// Maximum duration one exit world open operation may stay pending before failing the session
/// closed (`Q = 30 s`, matching [`super::dial::ONION_SESSION_OPEN_TIMEOUT`]'s 30 s world-open budget).
const ONION_EXIT_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum duration one exit world write or shutdown operation may stay pending before failing
/// the session closed.
const ONION_EXIT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// The world one symbol's sessions talk to: sockets for `tcp`, a fetch for `https`.
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
pub(crate) trait OnionWorld: MaybeSendSync + 'static {
    /// The world's read half.
    type Reader: OnionWorldReader;
    /// The world's write half.
    type Writer: OnionWorldWriter;

    /// Open the world at `target`, already admitted by the exit policy.
    async fn open(&self, target: &OnionProxyTarget) -> Result<(Self::Reader, Self::Writer)>;
}

/// The read half of one session's world. It records every byte it reads against the exit's
/// byte policy itself, as the bytes stream (the shell records only writes: one place per byte),
/// and fails once the budget is spent.
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
pub(crate) trait OnionWorldReader: MaybeSend + 'static {
    /// Up to `max` bytes, or `None` at the end of the world's stream.
    async fn read(&mut self, max: usize) -> Result<Option<Bytes>>;
}

/// The write half of one session's world.
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
pub(crate) trait OnionWorldWriter: MaybeSend + 'static {
    /// Write stream bytes.
    async fn write(&mut self, bytes: Bytes) -> Result<()>;

    /// Close the write half: the client's stream has ended.
    async fn shutdown(&mut self) -> Result<()>;
}

/// One forward loop for a session's driver.
struct OnionExitInput {
    /// The loop's frame.
    frame: OnionFrame,
    /// The loop's reply block.
    surb: Box<OnionSurb>,
    /// The loop's arrival.
    received_at_ms: u128,
}

/// A live session's entry in the table.
struct OnionExitSessionHandle {
    /// The digest the session is bound to.
    digest: OnionTargetDigest,
    /// The previous hop its loops arrive from: its route is fixed, so the hop of its opening
    /// loop.
    from: Did,
    /// Its driver's inbound queue, the one sender every loop goes through: a full queue then
    /// refuses.
    inbound: mpsc::Sender<OnionExitInput>,
}

/// Where one forward loop goes; the output of [`OnionExitTable::route`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OnionExitRoute {
    /// To the live session's driver.
    Deliver,
    /// A new session: lease it, spawn its driver, then deliver.
    Open,
    /// Answer with `abort` under the loop's own block: the session is buried, or its open is
    /// refused (no table slot, no lease).
    Answer,
    /// Drop without an answer: a loop of a live session naming another target, or a loop of a
    /// session never opened that carries no target (it may be overtaking its own
    /// open, which the answer would refuse).
    Drop,
}

/// The exit's sessions of one symbol: the live ones, and the tombstones of the closed ones.
#[derive(Default)]
struct OnionExitTable {
    /// The live sessions.
    live: HashMap<OnionSessionId, OnionExitSessionHandle>,
    /// Sessions closed less than `V` ago, with the instant each tombstone lapses.
    closed: HashMap<OnionSessionId, u128>,
}

impl OnionExitTable {
    /// Record that `session` closed at `now`; tombstones past their lapse are dropped first,
    /// and a full set evicts the one nearest its lapse, so the set never holds more than
    /// `ONION_EXIT_MAX_SESSIONS` and the newest closes are always buried.
    fn bury(&mut self, session: OnionSessionId, now_ms: u128) {
        self.live.remove(&session);
        self.closed.retain(|_, lapse_ms| *lapse_ms > now_ms);
        if self.closed.len() >= ONION_EXIT_MAX_SESSIONS {
            let nearest = self
                .closed
                .iter()
                .min_by_key(|(_, lapse_ms)| **lapse_ms)
                .map(|(nearest, _)| *nearest);
            if let Some(nearest) = nearest {
                tracing::debug!("onion exit tombstones are full; evict the nearest to lapse");
                self.closed.remove(&nearest);
            }
        }
        self.closed
            .insert(session, now_ms + ONION_ADMISSION_WINDOW_MS);
    }

    /// Where a loop of `arguments`' session goes at `now`, one that `carries_target` or not: the
    /// pure routing of [`OnionExitSessions::evaluate`] (see the module diagram).
    fn route(
        &self,
        arguments: &OnionSessionArguments,
        carries_target: bool,
        now_ms: u128,
    ) -> OnionExitRoute {
        match self.live.get(&arguments.session) {
            Some(handle) if handle.digest == arguments.digest => OnionExitRoute::Deliver,
            // D2′: a loop of a live `ς` that names another target is rejected.
            Some(_) => OnionExitRoute::Drop,
            None if self.is_buried(&arguments.session, now_ms) => OnionExitRoute::Answer,
            None if !carries_target => OnionExitRoute::Drop,
            None if self.live.len() >= ONION_EXIT_MAX_SESSIONS => OnionExitRoute::Answer,
            None => OnionExitRoute::Open,
        }
    }

    /// `lose(p)`: bury, at `now`, every live session whose loops arrive from `peer`. Its handle,
    /// the driver's one sender, goes, so its driver fails it closed.
    fn lose(&mut self, peer: Did, now_ms: u128) {
        let riding = self
            .live
            .iter()
            .filter(|(_, handle)| handle.from == peer)
            .map(|(session, _)| *session)
            .collect::<Vec<_>>();
        for session in riding {
            self.bury(session, now_ms);
        }
    }

    /// Whether `session` closed less than `V` before `now`.
    fn is_buried(&self, session: &OnionSessionId, now_ms: u128) -> bool {
        self.closed
            .get(session)
            .is_some_and(|lapse_ms| *lapse_ms > now_ms)
    }
}

/// What the drivers of one symbol share.
struct OnionExitShared<W> {
    /// The world.
    world: W,
    /// The exit policy the targets are admitted under.
    policy: OnionExitPolicy,
    /// The node-wide exit accounting (sessions and bytes).
    accounting: OnionExitAccounting,
    /// The link emitter replies leave through.
    link_sender: OnionLinkSender,
    /// Injected epoch clock capability.
    clock: SharedClock,
    /// The sessions.
    sessions: Mutex<OnionExitTable>,
    /// Test-only clock override (milliseconds since epoch) for deterministic expiry tests.
    #[cfg(all(test, rings_native))]
    clock_ms: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// Test-only tick period override for deterministic expiry tests.
    #[cfg(all(test, rings_native))]
    tick_period: Duration,
    /// Test-only per-write timeout override for deterministic stalled-write tests.
    #[cfg(all(test, rings_native))]
    write_timeout: Duration,
}

/// The interpretation of one world-facing session symbol over the world `W`; see the module
/// diagram.
pub(crate) struct OnionExitSessions<W> {
    shared: Arc<OnionExitShared<W>>,
}

impl<W: OnionWorld> OnionExitSessions<W> {
    /// The sessions of one symbol over `world`, under `policy` and the node's `accounting`,
    /// replying through `link_sender`.
    #[allow(dead_code)]
    pub(crate) fn new(
        world: W,
        policy: OnionExitPolicy,
        accounting: OnionExitAccounting,
        link_sender: OnionLinkSender,
    ) -> Self {
        Self::new_with_clock(
            world,
            policy,
            accounting,
            link_sender,
            Arc::new(SystemClock),
        )
    }

    /// [`Self::new`] driven by an explicit `clock` capability.
    pub(crate) fn new_with_clock(
        world: W,
        policy: OnionExitPolicy,
        accounting: OnionExitAccounting,
        link_sender: OnionLinkSender,
        clock: SharedClock,
    ) -> Self {
        Self {
            shared: Arc::new(OnionExitShared {
                world,
                policy,
                accounting,
                link_sender,
                clock,
                sessions: Mutex::new(OnionExitTable::default()),
                #[cfg(all(test, rings_native))]
                clock_ms: None,
                #[cfg(all(test, rings_native))]
                tick_period: ONION_EXIT_SESSION_TICK,
                #[cfg(all(test, rings_native))]
                write_timeout: ONION_EXIT_WRITE_TIMEOUT,
            }),
        }
    }
}

#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
impl<W: OnionWorld> OnionInterpretation for OnionExitSessions<W> {
    async fn evaluate(&self, scope: &Scope, input: OnionApplicationInput) -> Result<()> {
        let OnionApplicationInput {
            from,
            arguments,
            value,
            surb,
            received_at_ms,
        } = input;
        let arguments = OnionSessionArguments::decode(&arguments).ok_or(Error::InvalidData)?;
        let frame =
            OnionFrame::decode(surb.class(), value.as_slice()).map_err(|_| Error::InvalidData)?;
        let loop_input = OnionExitInput {
            frame,
            surb,
            received_at_ms,
        };
        let mut sessions = lock(&self.shared.sessions)?;
        let route = sessions.route(
            &arguments,
            loop_input.frame.carries_target(),
            received_at_ms,
        );
        match route {
            OnionExitRoute::Drop => return Ok(()),
            OnionExitRoute::Answer => {
                drop(sessions);
                self.answer_unheld(scope, loop_input);
                return Ok(());
            }
            OnionExitRoute::Open => {
                let Ok(lease) = self.shared.accounting.admit(&self.shared.policy, from) else {
                    drop(sessions);
                    tracing::debug!(%from, "onion exit session share is full; refuse an open");
                    self.answer_unheld(scope, loop_input);
                    return Ok(());
                };
                let (inbound, received) = mpsc::channel(ONION_EXIT_SESSION_INBOUND);
                scope.tasks().spawn(drive(
                    Arc::clone(&self.shared),
                    Some(scope.clone()),
                    arguments,
                    lease,
                    received,
                    received_at_ms,
                ))?;
                sessions
                    .live
                    .insert(arguments.session, OnionExitSessionHandle {
                        digest: arguments.digest,
                        from,
                        inbound,
                    });
            }
            OnionExitRoute::Deliver => {}
        }
        let Some(handle) = sessions.live.get_mut(&arguments.session) else {
            return Ok(());
        };
        match handle.inbound.try_send(loop_input) {
            Ok(()) => Ok(()),
            // The driver ended (closed, not yet buried): the session is unheld.
            Err(refused) if refused.is_disconnected() => {
                drop(sessions);
                self.answer_unheld(scope, refused.into_inner());
                Ok(())
            }
            // Fail closed at the queue: a dropped sequenced frame may be the
            // stream's last, which no later frame would reveal as a gap, so the session is
            // closed and the loop answered with `abort`; unsequenced credit is only dropped.
            Err(refused) => {
                let input = refused.into_inner();
                if input.frame.is_sequenced() {
                    tracing::debug!(%from, "onion exit session queue is full; abort the session");
                    sessions.bury(arguments.session, received_at_ms);
                    drop(sessions);
                    self.answer_unheld(scope, input);
                } else {
                    tracing::debug!(%from, "onion exit session queue is full; drop credit");
                }
                Ok(())
            }
        }
    }

    fn lose(&self, peer: Did) {
        match lock(&self.shared.sessions) {
            Ok(mut sessions) => sessions.lose(peer, now_ms(&self.shared)),
            Err(error) => tracing::debug!(%peer, %error, "onion exit sessions are unreadable"),
        }
    }
}

impl<W: OnionWorld> OnionExitSessions<W> {
    /// Answer a loop of a session this exit does not hold ([`reply_to_unheld`]), queued on the
    /// link without waiting, as a relay would: the evaluation never blocks on a dead session.
    fn answer_unheld(&self, scope: &Scope, input: OnionExitInput) {
        let Some(frame) = reply_to_unheld(&input.frame) else {
            return;
        };
        let Some((next, cell)) = seal(&frame, *input.surb) else {
            return;
        };
        let queued =
            self.shared
                .link_sender
                .enqueue_local(scope.clone(), OnionLink::new(next), cell);
        if let Err(error) = queued {
            tracing::debug!(
                %next,
                %error,
                "an onion exit's answer to an unheld session was dropped"
            );
        }
    }
}

/// A read of the world in flight: it owns the reader and hands it back with the result.
type WorldRead<R> =
    Pin<Box<rings_runtime::maybe_send!(dyn Future<Output = (R, Result<Option<Bytes>>)>)>>;

/// One ordered write-side operation on the session's world.
enum WorldWriteOp {
    /// Write stream bytes to the world.
    Write(Bytes),
    /// Close the world's write half (`fin`).
    Shutdown,
}

/// A write or shutdown of the world in flight: it owns the writer and hands it back with
/// whether the operation succeeded (`Shutdown` errors are ignored, matching the shell law).
type WorldWrite<W> = Pin<Box<rings_runtime::maybe_send!(dyn Future<Output = (W, bool)>)>>;

/// The halves of a session's open world.
struct OnionOpenWorld<W: OnionWorld> {
    /// The read half, while no read is in flight.
    reader: Option<W::Reader>,
    /// The write half, while no write or shutdown is in flight.
    writer: Option<W::Writer>,
    /// Ordered write and shutdown operations waiting for `writer`.
    pending_writes: VecDeque<WorldWriteOp>,
}

/// What the driver does after performing a batch of machine effects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PerformOutcome {
    /// Keep driving the session.
    Open,
    /// Both directions closed cleanly (`fin` both ways): finish any queued writes/shutdown before
    /// ending, while still polling abort, channel closure, and session expiry.
    DrainWritesThenClose,
    /// The session aborted, failed, or expired: stop immediately and drop any in-flight IO.
    CloseNow,
}

/// The driver of one session (see the module diagram). It holds the session's lease until it
/// ends, and buries the session when it does.
async fn drive<W: OnionWorld>(
    shared: Arc<OnionExitShared<W>>,
    scope: Option<Scope>,
    arguments: OnionSessionArguments,
    lease: OnionExitLease,
    mut inbound: mpsc::Receiver<OnionExitInput>,
    created_at_ms: u128,
) {
    let mut machine = OnionExitSession::new(arguments.digest, created_at_ms);
    let mut world: Option<OnionOpenWorld<W>> = None;
    let mut reading: Fuse<WorldRead<W::Reader>> = Fuse::terminated();
    let mut writing: Fuse<WorldWrite<W::Writer>> = Fuse::terminated();
    let mut closing_after_writes = false;
    let mut tick = Box::pin(session_sleep(tick_period(&shared)).fuse());
    loop {
        if writing.is_terminated() {
            let started = world.as_mut().and_then(|open| {
                let op = open.pending_writes.pop_front()?;
                let writer = open.writer.take()?;
                Some(write_world(writer, op, write_timeout(&shared)))
            });
            if let Some(write) = started {
                writing = write.fuse();
            }
        }
        if closing_after_writes
            && writing.is_terminated()
            && world
                .as_ref()
                .is_none_or(|open| open.pending_writes.is_empty())
        {
            break;
        }
        // The reader leaves the world only for a read that starts: taken with no capacity, it
        // would be dropped, and the world with it.
        if !closing_after_writes && reading.is_terminated() {
            let started = machine.reply_capacity(now_ms(&shared)).and_then(|max| {
                world
                    .as_mut()
                    .and_then(|open| open.reader.take())
                    .map(|reader| read_world(reader, max))
            });
            if let Some(read) = started {
                reading = read.fuse();
            }
        }
        let effects = futures::select! {
            input = inbound.next() => match input {
                Some(input) if closing_after_writes => {
                    if matches!(input.frame, OnionFrame::Abort { .. }) {
                        break;
                    }
                    Vec::new()
                }
                Some(input) => machine.forward(input.received_at_ms, input.frame, *input.surb),
                // Buried (a fate, or a full queue): fail closed, so `abort` leaves under a block
                // the session holds, then end whatever the machine answers.
                None => {
                    if !closing_after_writes {
                        let effects = machine.fail(now_ms(&shared));
                        let _ =
                            perform(&shared, scope.as_ref(), &mut machine, &mut world, effects)
                                .await;
                    }
                    break;
                }
            },
            (reader, read) = reading => {
                if let Some(open) = world.as_mut() {
                    open.reader = Some(reader);
                }
                let now = now_ms(&shared);
                match read {
                    Ok(Some(bytes)) => machine.world(now, OnionWorldRead::Bytes(bytes)),
                    Ok(None) => machine.world(now, OnionWorldRead::Eof),
                    Err(_) => machine.fail(now),
                }
            },
            (writer, ok) = writing => {
                if let Some(open) = world.as_mut() {
                    open.writer = Some(writer);
                }
                if ok {
                    Vec::new()
                } else if closing_after_writes {
                    break;
                } else {
                    if let Some(open) = world.as_mut() {
                        open.pending_writes.clear();
                    }
                    machine.fail(now_ms(&shared))
                }
            },
            _ = tick => {
                tick = Box::pin(session_sleep(tick_period(&shared)).fuse());
                let now = now_ms(&shared);
                if closing_after_writes {
                    if machine.is_expired_at(now) {
                        break;
                    }
                    Vec::new()
                } else {
                    machine.tick(now)
                }
            },
        };
        match perform(&shared, scope.as_ref(), &mut machine, &mut world, effects).await {
            PerformOutcome::Open => {}
            PerformOutcome::DrainWritesThenClose => {
                closing_after_writes = true;
            }
            PerformOutcome::CloseNow => break,
        }
    }
    drop(lease);
    if let Ok(mut sessions) = lock(&shared.sessions) {
        sessions.bury(arguments.session, now_ms(&shared));
    }
}

/// Start one read of at most `max` bytes, owning the reader until it resolves.
fn read_world<R: OnionWorldReader>(mut reader: R, max: usize) -> WorldRead<R> {
    Box::pin(async move {
        let read = reader.read(max).await;
        (reader, read)
    })
}

/// Start one write or shutdown on `writer`, owning the writer until it resolves or `timeout`
/// elapses.
fn write_world<W: OnionWorldWriter>(
    mut writer: W,
    op: WorldWriteOp,
    timeout: Duration,
) -> WorldWrite<W> {
    Box::pin(async move {
        let ok = {
            let op_fut = async {
                match op {
                    WorldWriteOp::Write(bytes) => writer.write(bytes).await.is_ok(),
                    WorldWriteOp::Shutdown => {
                        let _ = writer.shutdown().await;
                        true
                    }
                }
            }
            .fuse();
            let timeout_fut = session_sleep(timeout).fuse();
            futures::pin_mut!(op_fut, timeout_fut);
            futures::select! {
                ok = op_fut => ok,
                _ = timeout_fut => false,
            }
        };
        (writer, ok)
    })
}

/// Perform `effects` in order, feeding the machine's answers to `Open` back into the queue and
/// enqueuing world writes for `drive` to poll concurrently with session events.
async fn perform<W: OnionWorld>(
    shared: &OnionExitShared<W>,
    scope: Option<&Scope>,
    machine: &mut OnionExitSession,
    world: &mut Option<OnionOpenWorld<W>>,
    effects: Vec<OnionExitEffect>,
) -> PerformOutcome {
    let mut queue = VecDeque::from(effects);
    let mut failed = false;
    while let Some(effect) = queue.pop_front() {
        match effect {
            OnionExitEffect::Open { target } => {
                let opened = open(shared, &target).await;
                let accepted = opened.is_some();
                *world = opened;
                queue.extend(machine.opened(now_ms(shared), accepted));
            }
            OnionExitEffect::Write(bytes) => {
                let queued = match world.as_mut() {
                    Some(open)
                        if open.pending_writes.len() < ONION_EXIT_SESSION_INBOUND
                            && record(shared, bytes.len(), now_ms(shared)) =>
                    {
                        open.pending_writes.push_back(WorldWriteOp::Write(bytes));
                        true
                    }
                    _ => false,
                };
                if !queued {
                    failed = true;
                    if let Some(open) = world.as_mut() {
                        open.pending_writes.clear();
                    }
                    fail_at(now_ms(shared), machine, &mut queue);
                }
            }
            OnionExitEffect::ShutdownWrite => {
                if let Some(open) = world.as_mut() {
                    if open.pending_writes.len() < ONION_EXIT_SESSION_INBOUND {
                        open.pending_writes.push_back(WorldWriteOp::Shutdown);
                    } else {
                        failed = true;
                        open.pending_writes.clear();
                        fail_at(now_ms(shared), machine, &mut queue);
                    }
                }
            }
            // The driver waits for its reply to leave, so the world is read no faster than the
            // link emits: a reply is never dropped at a full link queue, which would be a gap
            // at the client (Law pause).
            OnionExitEffect::Reply { frame, surb } => {
                let Some((next, cell)) = seal(&frame, *surb) else {
                    tracing::debug!("an onion reply could not be sealed");
                    failed = true;
                    if let Some(open) = world.as_mut() {
                        open.pending_writes.clear();
                    }
                    fail_at(now_ms(shared), machine, &mut queue);
                    continue;
                };
                if let Some(scope) = scope {
                    let sent = shared
                        .link_sender
                        .send(scope.clone(), OnionLink::new(next), cell)
                        .await;
                    if let Err(error) = sent {
                        tracing::debug!(%next, %error, "an onion reply did not leave");
                        failed = true;
                        if let Some(open) = world.as_mut() {
                            open.pending_writes.clear();
                        }
                        fail_at(now_ms(shared), machine, &mut queue);
                    }
                }
            }
            OnionExitEffect::Close => {
                return if !failed && machine.completed_cleanly() {
                    PerformOutcome::DrainWritesThenClose
                } else {
                    PerformOutcome::CloseNow
                };
            }
        }
    }
    PerformOutcome::Open
}

/// The session failed while performing its effects: the failure's effects replace the ones still
/// queued, which would otherwise run against a failed world or send after its `abort`.
#[cfg(all(test, rings_native))]
fn fail(machine: &mut OnionExitSession, queue: &mut VecDeque<OnionExitEffect>) {
    fail_at(rings_core::utils::get_epoch_ms(), machine, queue);
}

/// [`fail`] at `now_ms`.
fn fail_at(now_ms: u128, machine: &mut OnionExitSession, queue: &mut VecDeque<OnionExitEffect>) {
    *queue = VecDeque::from(machine.fail(now_ms));
}

/// Current epoch milliseconds for `shared` (or its test clock override).
fn now_ms<W>(shared: &OnionExitShared<W>) -> u128 {
    #[cfg(all(test, rings_native))]
    if let Some(clock) = &shared.clock_ms {
        return u128::from(clock.load(std::sync::atomic::Ordering::SeqCst));
    }
    shared.clock.now_ms()
}

/// Tick period for `shared` (or its test override).
fn tick_period<W>(shared: &OnionExitShared<W>) -> Duration {
    #[cfg(all(test, rings_native))]
    {
        shared.tick_period
    }
    #[cfg(not(all(test, rings_native)))]
    {
        let _ = shared;
        ONION_EXIT_SESSION_TICK
    }
}

/// Per-operation write timeout for `shared` (or its test override).
fn write_timeout<W>(shared: &OnionExitShared<W>) -> Duration {
    #[cfg(all(test, rings_native))]
    {
        shared.write_timeout
    }
    #[cfg(not(all(test, rings_native)))]
    {
        let _ = shared;
        ONION_EXIT_WRITE_TIMEOUT
    }
}

/// Admit and open the world at `target`: its authority must parse and the policy must allow
/// it. Any failure is a refusal, whose reason stays here (#843 Q5).
async fn open<W: OnionWorld>(
    shared: &OnionExitShared<W>,
    target: &[u8],
) -> Option<OnionOpenWorld<W>> {
    let target = std::str::from_utf8(target)
        .ok()
        .and_then(|authority| OnionProxyTarget::parse_authority(authority).ok())?;
    if !shared.policy.allows_target(&target) {
        tracing::debug!(target = %target.authority(), "onion exit policy refuses a target");
        return None;
    }
    let open_fut = shared.world.open(&target).fuse();
    let timeout_fut = session_sleep(ONION_EXIT_OPEN_TIMEOUT).fuse();
    futures::pin_mut!(open_fut, timeout_fut);
    let result = futures::select! {
        res = open_fut => Some(res),
        _ = timeout_fut => None,
    };
    match result {
        Some(Ok((reader, writer))) => Some(OnionOpenWorld {
            reader: Some(reader),
            writer: Some(writer),
            pending_writes: VecDeque::new(),
        }),
        Some(Err(error)) => {
            tracing::debug!(target = %target.authority(), %error, "onion exit world refused");
            None
        }
        None => {
            tracing::debug!(target = %target.authority(), "onion exit world open timed out");
            None
        }
    }
}

/// Count `bytes` against the byte policy; `false` once it is spent, which closes the session.
fn record<W>(shared: &OnionExitShared<W>, bytes: usize, now_ms: u128) -> bool {
    u64::try_from(bytes).ok().is_some_and(|bytes| {
        shared
            .accounting
            .record_bytes(&shared.policy, bytes, now_ms)
            .is_ok()
    })
}

#[cfg(all(test, rings_native))]
mod tests {
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    use bytes::Bytes;
    use rings_core::dht::Did;

    use super::fail;
    use super::open;
    use super::OnionExitRoute;
    use super::OnionExitSessionHandle;
    use super::OnionExitShared;
    use super::OnionExitTable;
    use super::OnionWorld;
    use super::OnionWorldReader;
    use super::OnionWorldWriter;
    use super::ONION_EXIT_MAX_SESSIONS;
    use crate::error::Result;
    use crate::onion::circuit::OnionLinkSender;
    use crate::onion::circuit::ONION_ADMISSION_WINDOW_MS;
    use crate::onion::exit_accounting::OnionExitAccounting;
    use crate::onion::session::exit::OnionExitEffect;
    use crate::onion::session::exit::OnionExitSession;
    use crate::onion::session::OnionSessionArguments;
    use crate::onion::session::OnionSessionId;
    use crate::onion::session::OnionTargetDigest;
    use crate::onion::OnionExitPolicy;
    use crate::onion::OnionProxyTarget;

    /// A world that counts its opens and is otherwise empty.
    #[derive(Default)]
    struct CountingWorld(AtomicUsize);

    /// The empty halves of a [`CountingWorld`].
    struct Empty;

    #[async_trait::async_trait]
    impl OnionWorld for CountingWorld {
        type Reader = Empty;
        type Writer = Empty;

        async fn open(&self, _: &OnionProxyTarget) -> Result<(Empty, Empty)> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok((Empty, Empty))
        }
    }

    #[async_trait::async_trait]
    impl OnionWorldReader for Empty {
        async fn read(&mut self, _: usize) -> Result<Option<Bytes>> {
            Ok(None)
        }
    }

    #[async_trait::async_trait]
    impl OnionWorldWriter for Empty {
        async fn write(&mut self, _: Bytes) -> Result<()> {
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<()> {
            Ok(())
        }
    }

    /// The shared state of an exit over a counting world under `policy`.
    fn shared(policy: OnionExitPolicy) -> OnionExitShared<CountingWorld> {
        OnionExitShared {
            world: CountingWorld::default(),
            policy,
            accounting: OnionExitAccounting::default(),
            link_sender: OnionLinkSender::default(),
            clock: std::sync::Arc::new(rings_core::utils::SystemClock),
            sessions: Mutex::new(OnionExitTable::default()),
            clock_ms: None,
            tick_period: super::ONION_EXIT_SESSION_TICK,
            write_timeout: super::ONION_EXIT_WRITE_TIMEOUT,
        }
    }

    /// The exit's open admits a target only under its policy, and never opens the world for a
    /// denied or malformed one (#843 Q5).
    #[tokio::test]
    async fn test_open_refuses_what_the_policy_denies_before_the_world() -> Result<()> {
        let policy = OnionExitPolicy::from_target_strings(vec!["*:443".to_string()], vec![
            "blocked.example:443".to_string(),
        ])?;
        let exit = shared(policy);

        assert!(open(&exit, b"allowed.example:443").await.is_some());
        assert!(open(&exit, b"blocked.example:443").await.is_none());
        assert!(open(&exit, b"allowed.example:80").await.is_none());
        assert!(open(&exit, b"not an authority").await.is_none());
        assert!(open(&exit, &[0xff, 0xfe]).await.is_none());
        assert_eq!(exit.world.0.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// The routing of a loop: a live session's loop is delivered, one naming
    /// another target dropped; a buried session's loop is answered; a never-opened session's
    /// loop opens it only if it carries `T`, is answered when the table is full, and is dropped
    /// otherwise, since it may overtake its own open.
    #[test]
    fn test_a_loop_is_routed_by_its_session() {
        let mut table = OnionExitTable::default();
        let target = OnionTargetDigest::of(b"example.com:443");
        let other = OnionTargetDigest::of(b"other.example:443");
        let arguments = |byte: u8, digest: OnionTargetDigest| OnionSessionArguments {
            session: OnionSessionId::new([byte; 16]),
            digest,
        };
        let (inbound, _received) = futures::channel::mpsc::channel(1);
        table
            .live
            .insert(arguments(1, target).session, OnionExitSessionHandle {
                digest: target,
                from: Did::from(1_u32),
                inbound,
            });
        table.bury(arguments(2, target).session, 0);

        assert_eq!(
            table.route(&arguments(1, target), false, 0),
            OnionExitRoute::Deliver
        );
        assert_eq!(
            table.route(&arguments(1, other), true, 0),
            OnionExitRoute::Drop
        );
        assert_eq!(
            table.route(&arguments(2, target), false, 1),
            OnionExitRoute::Answer
        );
        assert_eq!(
            table.route(&arguments(2, target), true, 1),
            OnionExitRoute::Answer
        );
        assert_eq!(
            table.route(&arguments(3, target), false, 1),
            OnionExitRoute::Drop
        );
        assert_eq!(
            table.route(&arguments(3, target), true, 1),
            OnionExitRoute::Open
        );

        for index in 0..ONION_EXIT_MAX_SESSIONS {
            let (inbound, _) = futures::channel::mpsc::channel(1);
            let session = u128::try_from(index + 16).expect("small").to_be_bytes();
            table
                .live
                .insert(OnionSessionId::new(session), OnionExitSessionHandle {
                    digest: target,
                    from: Did::from(1_u32),
                    inbound,
                });
        }
        assert_eq!(
            table.route(&arguments(3, target), true, 1),
            OnionExitRoute::Answer,
            "a refused open is answered"
        );
    }

    /// A closed session is buried for `V`, so a late loop cannot respawn it, and the tombstones
    /// stay bounded (#895 H5).
    #[test]
    fn test_a_closed_session_is_buried_for_v() {
        let mut table = OnionExitTable::default();
        let session = OnionSessionId::new([3; 16]);

        table.bury(session, 1_000);
        assert!(table.is_buried(&session, 1_000 + ONION_ADMISSION_WINDOW_MS - 1));
        assert!(!table.is_buried(&session, 1_000 + ONION_ADMISSION_WINDOW_MS));

        let mut full = OnionExitTable {
            live: HashMap::new(),
            closed: HashMap::new(),
        };
        for index in 0..=ONION_EXIT_MAX_SESSIONS {
            let bytes = u128::try_from(index).expect("small").to_be_bytes();
            full.bury(
                OnionSessionId::new(bytes),
                u128::try_from(index).expect("small"),
            );
        }
        assert_eq!(full.closed.len(), ONION_EXIT_MAX_SESSIONS);
        let newest = u128::try_from(ONION_EXIT_MAX_SESSIONS)
            .expect("small")
            .to_be_bytes();
        assert!(full.is_buried(&OnionSessionId::new(newest), 0));
        assert!(!full.is_buried(&OnionSessionId::new(0_u128.to_be_bytes()), 0));
    }

    /// Fate (#963, #966): a lost link buries exactly the sessions whose loops arrive from its
    /// peer, closing their drivers' queues, so a later loop of one is answered with `abort`.
    #[test]
    fn test_a_lost_link_buries_only_the_sessions_it_carries() {
        let mut table = OnionExitTable::default();
        let target = OnionTargetDigest::of(b"example.com:443");
        let (lost, kept) = (Did::from(7_u32), Did::from(8_u32));
        let mut queues = Vec::new();
        for (byte, from) in [(1, lost), (2, kept), (3, lost)] {
            let (inbound, received) = futures::channel::mpsc::channel(1);
            queues.push(received);
            table
                .live
                .insert(OnionSessionId::new([byte; 16]), OnionExitSessionHandle {
                    digest: target,
                    from,
                    inbound,
                });
        }

        table.lose(lost, 0);

        let arguments = |byte: u8| OnionSessionArguments {
            session: OnionSessionId::new([byte; 16]),
            digest: target,
        };
        assert_eq!(table.route(&arguments(1), false, 1), OnionExitRoute::Answer);
        assert_eq!(table.route(&arguments(3), false, 1), OnionExitRoute::Answer);
        assert_eq!(
            table.route(&arguments(2), false, 1),
            OnionExitRoute::Deliver
        );
        let closed = queues
            .iter_mut()
            .map(|queue| queue.try_recv().is_err_and(|error| error.is_closed()))
            .collect::<Vec<_>>();
        assert_eq!(closed, [true, false, true]);
    }

    /// A failure while performing drops the effects still queued: no write runs against the
    /// failed world, and nothing is sent after the failure's `abort`; only the failure's own
    /// effects remain, ending the session.
    #[test]
    fn test_a_failure_replaces_the_queued_effects() {
        let mut machine = OnionExitSession::new(OnionTargetDigest::of(b"example.com:443"), 0);
        let mut queue = VecDeque::from([
            OnionExitEffect::Write(Bytes::from_static(b"late")),
            OnionExitEffect::ShutdownWrite,
        ]);

        fail(&mut machine, &mut queue);

        assert!(queue.iter().all(|effect| !matches!(
            effect,
            OnionExitEffect::Write(_) | OnionExitEffect::ShutdownWrite
        )));
        assert!(matches!(queue.back(), Some(OnionExitEffect::Close)));
    }

    /// A fixture reply block admissible at `60_000` (`2Q`), expiring at `90_000` (`3Q`).
    fn fixture_surb(seed: u64) -> crate::onion::sphinx::cell::OnionSurb {
        use rand::rngs::StdRng;
        use rand::SeedableRng;
        use rings_core::delegation::DelegateeKey;
        use rings_core::ecc::SecretKey;

        use crate::onion::circuit::OnionExpiry;
        use crate::onion::sphinx::builder::build_surb;
        use crate::onion::sphinx::class::OnionLoopClass;
        use crate::onion::OnionProcessEpoch;
        use crate::onion::OnionRouteHop;

        let path = [1_u8, 2].map(|byte| {
            let secret = SecretKey::try_from(format!("{byte:02x}").repeat(32).as_str())
                .expect("fixture scalar");
            let key = DelegateeKey::new_with_seckey(&secret).expect("fixture delegation");
            OnionRouteHop::new(
                key.delegator_did(),
                key.delegatee_public_key(),
                OnionProcessEpoch::new([1; 16]),
            )
        });
        let mut rng = StdRng::seed_from_u64(seed);
        let expiry = OnionExpiry::from_ms(90_000).expect("on the grid");
        build_surb(
            path.iter(),
            Did::from(9_u32),
            OnionLoopClass::DEFAULT,
            expiry,
            &mut rng,
        )
        .expect("a reply block")
        .0
    }

    /// A world whose writer enters `write` and stays `Poll::Pending` forever, recording when the
    /// write starts and when the writer is dropped.
    #[derive(Clone)]
    struct PendingWriteWorld {
        write_started: std::sync::Arc<tokio::sync::Notify>,
        writer_dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    struct PendingWriter {
        write_started: std::sync::Arc<tokio::sync::Notify>,
        writer_dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for PendingWriter {
        fn drop(&mut self) {
            self.writer_dropped.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl OnionWorld for PendingWriteWorld {
        type Reader = Empty;
        type Writer = PendingWriter;

        async fn open(&self, _: &OnionProxyTarget) -> Result<(Self::Reader, Self::Writer)> {
            Ok((Empty, PendingWriter {
                write_started: std::sync::Arc::clone(&self.write_started),
                writer_dropped: std::sync::Arc::clone(&self.writer_dropped),
            }))
        }
    }

    #[async_trait::async_trait]
    impl OnionWorldWriter for PendingWriter {
        async fn write(&mut self, _: Bytes) -> Result<()> {
            self.write_started.notify_one();
            std::future::pending::<Result<()>>().await
        }

        async fn shutdown(&mut self) -> Result<()> {
            self.write_started.notify_one();
            std::future::pending::<Result<()>>().await
        }
    }

    /// Regression (#1211): when an exit world write is blocked indefinitely, a client `Abort`
    /// frame cancels the in-flight write immediately, drops the writer, buries the session, and
    /// releases its `OnionExitLease`.
    #[tokio::test]
    async fn test_stalled_exit_write_is_cancelled_by_client_abort_and_releases_lease() {
        let now_ms = 60_000_u128;
        let peer = Did::from(11_u32);
        let policy = OnionExitPolicy {
            allowed_targets: vec![crate::onion::OnionExitTarget::parse("*:443").expect("pattern")],
            max_sessions: 1,
            ..OnionExitPolicy::default()
        };
        let write_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let writer_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(60_000));
        let shared = std::sync::Arc::new(OnionExitShared {
            world: PendingWriteWorld {
                write_started: std::sync::Arc::clone(&write_started),
                writer_dropped: std::sync::Arc::clone(&writer_dropped),
            },
            policy: policy.clone(),
            accounting: OnionExitAccounting::default(),
            link_sender: OnionLinkSender::default(),
            clock: std::sync::Arc::new(rings_core::utils::SystemClock),
            sessions: Mutex::new(OnionExitTable::default()),
            clock_ms: Some(clock),
            tick_period: std::time::Duration::from_millis(5),
            write_timeout: super::ONION_EXIT_WRITE_TIMEOUT,
        });
        let lease = shared
            .accounting
            .admit(&policy, peer)
            .expect("initial lease admitted");
        assert!(
            shared.accounting.admit(&policy, peer).is_err(),
            "session cap of 1 is held by the active session"
        );

        let arguments = OnionSessionArguments {
            session: OnionSessionId::new([0x11; 16]),
            digest: OnionTargetDigest::of(b"example.com:443"),
        };
        let (mut inbound_tx, inbound_rx) =
            futures::channel::mpsc::channel(super::ONION_EXIT_SESSION_INBOUND);
        let driver = crate::test_support::TestTask::spawn(super::drive(
            std::sync::Arc::clone(&shared),
            None,
            arguments,
            lease,
            inbound_rx,
            now_ms,
        ));

        inbound_tx
            .try_send(super::OnionExitInput {
                frame: crate::onion::session::frame::OnionFrame::Data {
                    sequence: crate::onion::session::frame::OnionSequence::FIRST,
                    target: Some(Bytes::from_static(b"example.com:443")),
                    payload: Bytes::from_static(b"stalled-upload"),
                },
                surb: Box::new(fixture_surb(1)),
                received_at_ms: now_ms,
            })
            .expect("enqueue opening data");

        tokio::time::timeout(std::time::Duration::from_secs(1), write_started.notified())
            .await
            .expect("world write started");
        assert!(!writer_dropped.load(Ordering::SeqCst));

        inbound_tx
            .try_send(super::OnionExitInput {
                frame: crate::onion::session::frame::OnionFrame::Abort {
                    sequence: crate::onion::session::frame::OnionSequence::new(1),
                },
                surb: Box::new(fixture_surb(2)),
                received_at_ms: now_ms + 1,
            })
            .expect("enqueue abort");

        tokio::time::timeout(std::time::Duration::from_secs(1), driver)
            .await
            .expect("driver exits promptly on abort while write is blocked")
            .expect("driver join");

        assert!(writer_dropped.load(Ordering::SeqCst));
        assert!(
            shared.accounting.admit(&policy, peer).is_ok(),
            "lease released after abort"
        );
        assert!(shared
            .sessions
            .lock()
            .expect("sessions lock")
            .is_buried(&arguments.session, now_ms + 1));
    }

    /// Regression (#1211): when an exit world write is blocked indefinitely, losing the link
    /// (`sessions.lose(peer, ...)`) closes the inbound channel, cancels the blocked write, and
    /// releases the session lease.
    #[tokio::test]
    async fn test_stalled_exit_write_is_cancelled_by_lost_link_and_releases_lease() {
        let now_ms = 60_000_u128;
        let peer = Did::from(12_u32);
        let policy = OnionExitPolicy {
            allowed_targets: vec![crate::onion::OnionExitTarget::parse("*:443").expect("pattern")],
            max_sessions: 1,
            ..OnionExitPolicy::default()
        };
        let write_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let writer_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(60_000));
        let shared = std::sync::Arc::new(OnionExitShared {
            world: PendingWriteWorld {
                write_started: std::sync::Arc::clone(&write_started),
                writer_dropped: std::sync::Arc::clone(&writer_dropped),
            },
            policy: policy.clone(),
            accounting: OnionExitAccounting::default(),
            link_sender: OnionLinkSender::default(),
            clock: std::sync::Arc::new(rings_core::utils::SystemClock),
            sessions: Mutex::new(OnionExitTable::default()),
            clock_ms: Some(clock),
            tick_period: std::time::Duration::from_millis(5),
            write_timeout: super::ONION_EXIT_WRITE_TIMEOUT,
        });
        let lease = shared
            .accounting
            .admit(&policy, peer)
            .expect("initial lease admitted");

        let arguments = OnionSessionArguments {
            session: OnionSessionId::new([0x22; 16]),
            digest: OnionTargetDigest::of(b"example.com:443"),
        };
        let (mut inbound_tx, inbound_rx) =
            futures::channel::mpsc::channel(super::ONION_EXIT_SESSION_INBOUND);
        inbound_tx
            .try_send(super::OnionExitInput {
                frame: crate::onion::session::frame::OnionFrame::Data {
                    sequence: crate::onion::session::frame::OnionSequence::FIRST,
                    target: Some(Bytes::from_static(b"example.com:443")),
                    payload: Bytes::from_static(b"stalled-upload"),
                },
                surb: Box::new(fixture_surb(3)),
                received_at_ms: now_ms,
            })
            .expect("enqueue opening data");
        shared.sessions.lock().expect("sessions lock").live.insert(
            arguments.session,
            OnionExitSessionHandle {
                digest: arguments.digest,
                from: peer,
                inbound: inbound_tx,
            },
        );

        let driver = crate::test_support::TestTask::spawn(super::drive(
            std::sync::Arc::clone(&shared),
            None,
            arguments,
            lease,
            inbound_rx,
            now_ms,
        ));

        tokio::time::timeout(std::time::Duration::from_secs(1), write_started.notified())
            .await
            .expect("world write started");

        shared
            .sessions
            .lock()
            .expect("sessions lock")
            .lose(peer, now_ms + 1);

        tokio::time::timeout(std::time::Duration::from_secs(1), driver)
            .await
            .expect("driver exits promptly on link loss while write is blocked")
            .expect("driver join");

        assert!(writer_dropped.load(Ordering::SeqCst));
        assert!(
            shared.accounting.admit(&policy, peer).is_ok(),
            "lease released after link loss"
        );
    }

    /// Regression (#1211): when an exit world write is blocked indefinitely and no forward loops
    /// arrive for `V` (`ONION_ADMISSION_WINDOW_MS`), the periodic tick still runs, expires the
    /// session, drops the stalled writer, and releases its `OnionExitLease`.
    #[tokio::test]
    async fn test_stalled_exit_write_is_cancelled_by_session_expiry_and_releases_lease() {
        let now_ms = 60_000_u128;
        let peer = Did::from(13_u32);
        let policy = OnionExitPolicy {
            allowed_targets: vec![crate::onion::OnionExitTarget::parse("*:443").expect("pattern")],
            max_sessions: 1,
            ..OnionExitPolicy::default()
        };
        let write_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let writer_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(60_000));
        let shared = std::sync::Arc::new(OnionExitShared {
            world: PendingWriteWorld {
                write_started: std::sync::Arc::clone(&write_started),
                writer_dropped: std::sync::Arc::clone(&writer_dropped),
            },
            policy: policy.clone(),
            accounting: OnionExitAccounting::default(),
            link_sender: OnionLinkSender::default(),
            clock: std::sync::Arc::new(rings_core::utils::SystemClock),
            sessions: Mutex::new(OnionExitTable::default()),
            clock_ms: Some(std::sync::Arc::clone(&clock)),
            tick_period: std::time::Duration::from_millis(5),
            write_timeout: super::ONION_EXIT_WRITE_TIMEOUT,
        });
        let lease = shared
            .accounting
            .admit(&policy, peer)
            .expect("initial lease admitted");

        let arguments = OnionSessionArguments {
            session: OnionSessionId::new([0x33; 16]),
            digest: OnionTargetDigest::of(b"example.com:443"),
        };
        let (mut inbound_tx, inbound_rx) =
            futures::channel::mpsc::channel(super::ONION_EXIT_SESSION_INBOUND);
        let driver = crate::test_support::TestTask::spawn(super::drive(
            std::sync::Arc::clone(&shared),
            None,
            arguments,
            lease,
            inbound_rx,
            now_ms,
        ));

        inbound_tx
            .try_send(super::OnionExitInput {
                frame: crate::onion::session::frame::OnionFrame::Data {
                    sequence: crate::onion::session::frame::OnionSequence::FIRST,
                    target: Some(Bytes::from_static(b"example.com:443")),
                    payload: Bytes::from_static(b"stalled-upload"),
                },
                surb: Box::new(fixture_surb(4)),
                received_at_ms: now_ms,
            })
            .expect("enqueue opening data");

        tokio::time::timeout(std::time::Duration::from_secs(1), write_started.notified())
            .await
            .expect("world write started");

        let expired_ms = u64::try_from(now_ms + ONION_ADMISSION_WINDOW_MS).expect("u64");
        clock.store(expired_ms, Ordering::SeqCst);

        tokio::time::timeout(std::time::Duration::from_secs(1), driver)
            .await
            .expect("driver expires promptly even while world write is blocked")
            .expect("driver join");

        assert!(writer_dropped.load(Ordering::SeqCst));
        assert!(
            shared.accounting.admit(&policy, peer).is_ok(),
            "lease released after session expiry"
        );
    }

    /// Regression (#1211): a real TCP exit socket (`OnionTcpWriter`) whose upstream peer never
    /// reads and whose OS send buffer is completely full does not wedge session expiry or lease
    /// release.
    #[tokio::test]
    async fn test_stalled_tcp_exit_socket_write_releases_lease_on_expiry() {
        use crate::onion::tcp::OnionTcpReader;
        use crate::onion::tcp::OnionTcpWriter;

        struct StalledSocketWorld {
            stream: Mutex<Option<tokio::net::TcpStream>>,
            policy: OnionExitPolicy,
            accounting: OnionExitAccounting,
        }

        #[async_trait::async_trait]
        impl OnionWorld for StalledSocketWorld {
            type Reader = OnionTcpReader;
            type Writer = OnionTcpWriter;

            async fn open(&self, _: &OnionProxyTarget) -> Result<(Self::Reader, Self::Writer)> {
                let stream = self
                    .stream
                    .lock()
                    .expect("stream lock")
                    .take()
                    .expect("single open");
                let (read, write) = stream.into_split();
                Ok((
                    OnionTcpReader::from_split(read, self.policy.clone(), self.accounting.clone()),
                    OnionTcpWriter::from_split(write),
                ))
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (client_res, accept_res) =
            tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
        let client_stream = client_res.expect("connect");
        let (_stalled_upstream_peer, _) = accept_res.expect("accept");

        // Pre-fill the socket's kernel send buffer until a non-blocking try_write reports
        // WouldBlock so the very first `OnionTcpWriter::write` blocks inside `write_all`.
        let fill_buf = vec![0x5a_u8; 64 * 1024];
        loop {
            match client_stream.try_write(&fill_buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("unexpected try_write error: {err}"),
            }
        }

        let now_ms = 60_000_u128;
        let peer = Did::from(14_u32);
        let policy = OnionExitPolicy {
            allowed_targets: vec![crate::onion::OnionExitTarget::parse("*:443").expect("pattern")],
            max_sessions: 1,
            ..OnionExitPolicy::default()
        };
        let accounting = OnionExitAccounting::default();
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(60_000));
        let shared = std::sync::Arc::new(OnionExitShared {
            world: StalledSocketWorld {
                stream: Mutex::new(Some(client_stream)),
                policy: policy.clone(),
                accounting: accounting.clone(),
            },
            policy: policy.clone(),
            accounting,
            link_sender: OnionLinkSender::default(),
            clock: std::sync::Arc::new(rings_core::utils::SystemClock),
            sessions: Mutex::new(OnionExitTable::default()),
            clock_ms: Some(std::sync::Arc::clone(&clock)),
            tick_period: std::time::Duration::from_millis(5),
            write_timeout: super::ONION_EXIT_WRITE_TIMEOUT,
        });
        let lease = shared
            .accounting
            .admit(&policy, peer)
            .expect("initial lease admitted");

        let arguments = OnionSessionArguments {
            session: OnionSessionId::new([0x44; 16]),
            digest: OnionTargetDigest::of(b"example.com:443"),
        };
        let (mut inbound_tx, inbound_rx) =
            futures::channel::mpsc::channel(super::ONION_EXIT_SESSION_INBOUND);
        let driver = crate::test_support::TestTask::spawn(super::drive(
            std::sync::Arc::clone(&shared),
            None,
            arguments,
            lease,
            inbound_rx,
            now_ms,
        ));

        inbound_tx
            .try_send(super::OnionExitInput {
                frame: crate::onion::session::frame::OnionFrame::Data {
                    sequence: crate::onion::session::frame::OnionSequence::FIRST,
                    target: Some(Bytes::from_static(b"example.com:443")),
                    payload: Bytes::from(vec![0x7f_u8; 256 * 1024]),
                },
                surb: Box::new(fixture_surb(5)),
                received_at_ms: now_ms,
            })
            .expect("enqueue opening data");

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            shared.accounting.admit(&policy, peer).is_err(),
            "lease still held while socket write is blocked"
        );

        let expired_ms = u64::try_from(now_ms + ONION_ADMISSION_WINDOW_MS).expect("u64");
        clock.store(expired_ms, Ordering::SeqCst);

        tokio::time::timeout(std::time::Duration::from_secs(1), driver)
            .await
            .expect("driver terminates on expiry despite blocked TCP socket write")
            .expect("driver join");

        assert!(
            shared.accounting.admit(&policy, peer).is_ok(),
            "lease released after blocked TCP socket session expires"
        );
    }

    /// Regression (#1211): even when a client sends periodic keep-alive `Credit` loops so `V`
    /// never elapses without a forward loop, a wedged exit world write times out after
    /// `write_timeout`, fails the session closed, drops the writer, and releases the lease.
    #[tokio::test]
    async fn test_stalled_exit_write_times_out_despite_keep_alive_credits_and_releases_lease() {
        let now_ms = 60_000_u128;
        let peer = Did::from(15_u32);
        let policy = OnionExitPolicy {
            allowed_targets: vec![crate::onion::OnionExitTarget::parse("*:443").expect("pattern")],
            max_sessions: 1,
            ..OnionExitPolicy::default()
        };
        let write_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let writer_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(60_000));
        let shared = std::sync::Arc::new(OnionExitShared {
            world: PendingWriteWorld {
                write_started: std::sync::Arc::clone(&write_started),
                writer_dropped: std::sync::Arc::clone(&writer_dropped),
            },
            policy: policy.clone(),
            accounting: OnionExitAccounting::default(),
            link_sender: OnionLinkSender::default(),
            clock: std::sync::Arc::new(rings_core::utils::SystemClock),
            sessions: Mutex::new(OnionExitTable::default()),
            clock_ms: Some(std::sync::Arc::clone(&clock)),
            tick_period: std::time::Duration::from_secs(10),
            write_timeout: std::time::Duration::from_millis(25),
        });
        let lease = shared
            .accounting
            .admit(&policy, peer)
            .expect("initial lease admitted");

        let arguments = OnionSessionArguments {
            session: OnionSessionId::new([0x55; 16]),
            digest: OnionTargetDigest::of(b"example.com:443"),
        };
        let (mut inbound_tx, inbound_rx) =
            futures::channel::mpsc::channel(super::ONION_EXIT_SESSION_INBOUND);
        let driver = crate::test_support::TestTask::spawn(super::drive(
            std::sync::Arc::clone(&shared),
            None,
            arguments,
            lease,
            inbound_rx,
            now_ms,
        ));

        inbound_tx
            .try_send(super::OnionExitInput {
                frame: crate::onion::session::frame::OnionFrame::Data {
                    sequence: crate::onion::session::frame::OnionSequence::FIRST,
                    target: Some(Bytes::from_static(b"example.com:443")),
                    payload: Bytes::from_static(b"stalled-upload"),
                },
                surb: Box::new(fixture_surb(6)),
                received_at_ms: now_ms,
            })
            .expect("enqueue opening data");

        tokio::time::timeout(std::time::Duration::from_secs(1), write_started.notified())
            .await
            .expect("world write started");

        // Send a keep-alive credit loop well before `V` elapses; the per-write timeout still
        // terminates the wedged write and releases the lease.
        inbound_tx
            .try_send(super::OnionExitInput {
                frame: crate::onion::session::frame::OnionFrame::Credit {
                    mark: crate::onion::session::frame::OnionSequence::new(1),
                    surbs: vec![fixture_surb(7)],
                },
                surb: Box::new(fixture_surb(8)),
                received_at_ms: now_ms + 1_000,
            })
            .expect("enqueue keep-alive credit");

        tokio::time::timeout(std::time::Duration::from_secs(1), driver)
            .await
            .expect("driver terminates on write timeout despite keep-alive credit")
            .expect("driver join");

        assert!(writer_dropped.load(Ordering::SeqCst));
        assert!(
            shared.accounting.admit(&policy, peer).is_ok(),
            "lease released after write timeout"
        );
    }
}
