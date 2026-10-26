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
use rings_core::utils::get_epoch_ms;
use rings_runtime::sleep;
use rings_runtime::MaybeSend;
use rings_runtime::MaybeSendSync;

use super::exit::reply_to_unheld;
use super::exit::seal;
use super::exit::OnionExitEffect;
use super::exit::OnionExitSession;
use super::exit::OnionWorldRead;
use super::frame::OnionFrame;
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
    /// The sessions.
    sessions: Mutex<OnionExitTable>,
}

/// The interpretation of one world-facing session symbol over the world `W`; see the module
/// diagram.
pub(crate) struct OnionExitSessions<W> {
    shared: Arc<OnionExitShared<W>>,
}

impl<W: OnionWorld> OnionExitSessions<W> {
    /// The sessions of one symbol over `world`, under `policy` and the node's `accounting`,
    /// replying through `link_sender`.
    pub(crate) fn new(
        world: W,
        policy: OnionExitPolicy,
        accounting: OnionExitAccounting,
        link_sender: OnionLinkSender,
    ) -> Self {
        Self {
            shared: Arc::new(OnionExitShared {
                world,
                policy,
                accounting,
                link_sender,
                sessions: Mutex::new(OnionExitTable::default()),
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
                    scope.clone(),
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
            Ok(mut sessions) => sessions.lose(peer, get_epoch_ms()),
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

/// The halves of a session's open world.
struct OnionOpenWorld<W: OnionWorld> {
    /// The read half, while no read is in flight.
    reader: Option<W::Reader>,
    /// The write half.
    writer: W::Writer,
}

/// The driver of one session (see the module diagram). It holds the session's lease until it
/// ends, and buries the session when it does.
async fn drive<W: OnionWorld>(
    shared: Arc<OnionExitShared<W>>,
    scope: Scope,
    arguments: OnionSessionArguments,
    lease: OnionExitLease,
    mut inbound: mpsc::Receiver<OnionExitInput>,
    created_at_ms: u128,
) {
    let mut machine = OnionExitSession::new(arguments.digest, created_at_ms);
    let mut world: Option<OnionOpenWorld<W>> = None;
    let mut reading: Fuse<WorldRead<W::Reader>> = Fuse::terminated();
    let mut tick = Box::pin(sleep(ONION_EXIT_SESSION_TICK).fuse());
    loop {
        let effects = futures::select! {
            input = inbound.next() => match input {
                Some(input) => machine.forward(input.received_at_ms, input.frame, *input.surb),
                // Buried (a fate, or a full queue): fail closed, so `abort` leaves under a block
                // the session holds, then end whatever the machine answers.
                None => {
                    let effects = machine.fail(get_epoch_ms());
                    perform(&shared, &scope, &mut machine, &mut world, effects).await;
                    break;
                }
            },
            (reader, read) = reading => {
                if let Some(open) = world.as_mut() {
                    open.reader = Some(reader);
                }
                let now_ms = get_epoch_ms();
                match read {
                    Ok(Some(bytes)) => machine.world(now_ms, OnionWorldRead::Bytes(bytes)),
                    Ok(None) => machine.world(now_ms, OnionWorldRead::Eof),
                    Err(_) => machine.fail(now_ms),
                }
            },
            _ = tick => {
                tick = Box::pin(sleep(ONION_EXIT_SESSION_TICK).fuse());
                machine.tick(get_epoch_ms())
            },
        };
        let open = perform(&shared, &scope, &mut machine, &mut world, effects).await;
        if !open {
            break;
        }
        // The reader leaves the world only for a read that starts: taken with no capacity, it
        // would be dropped, and the world with it.
        if reading.is_terminated() {
            let started = machine.reply_capacity(get_epoch_ms()).and_then(|max| {
                world
                    .as_mut()
                    .and_then(|open| open.reader.take())
                    .map(|reader| read_world(reader, max))
            });
            if let Some(read) = started {
                reading = read.fuse();
            }
        }
    }
    drop(lease);
    if let Ok(mut sessions) = lock(&shared.sessions) {
        sessions.bury(arguments.session, get_epoch_ms());
    }
}

/// Start one read of at most `max` bytes, owning the reader until it resolves.
fn read_world<R: OnionWorldReader>(mut reader: R, max: usize) -> WorldRead<R> {
    Box::pin(async move {
        let read = reader.read(max).await;
        (reader, read)
    })
}

/// Perform `effects` in order, feeding the machine's answers to `Open` back into the queue;
/// return whether the session is still open.
async fn perform<W: OnionWorld>(
    shared: &OnionExitShared<W>,
    scope: &Scope,
    machine: &mut OnionExitSession,
    world: &mut Option<OnionOpenWorld<W>>,
    effects: Vec<OnionExitEffect>,
) -> bool {
    let mut queue = VecDeque::from(effects);
    while let Some(effect) = queue.pop_front() {
        match effect {
            OnionExitEffect::Open { target } => {
                let opened = open(shared, &target).await;
                let accepted = opened.is_some();
                *world = opened;
                queue.extend(machine.opened(get_epoch_ms(), accepted));
            }
            OnionExitEffect::Write(bytes) => {
                let written = match world.as_mut() {
                    Some(open) if record(shared, bytes.len(), get_epoch_ms()) => {
                        open.writer.write(bytes).await.is_ok()
                    }
                    _ => false,
                };
                if !written {
                    fail(machine, &mut queue);
                }
            }
            OnionExitEffect::ShutdownWrite => {
                if let Some(open) = world.as_mut() {
                    let _ = open.writer.shutdown().await;
                }
            }
            // The driver waits for its reply to leave, so the world is read no faster than the
            // link emits: a reply is never dropped at a full link queue, which would be a gap
            // at the client (Law pause).
            OnionExitEffect::Reply { frame, surb } => {
                let Some((next, cell)) = seal(&frame, *surb) else {
                    tracing::debug!("an onion reply could not be sealed");
                    fail(machine, &mut queue);
                    continue;
                };
                let sent = shared
                    .link_sender
                    .send(scope.clone(), OnionLink::new(next), cell)
                    .await;
                if let Err(error) = sent {
                    tracing::debug!(%next, %error, "an onion reply did not leave");
                    fail(machine, &mut queue);
                }
            }
            OnionExitEffect::Close => return false,
        }
    }
    true
}

/// The session failed while performing its effects: the failure's effects replace the ones still
/// queued, which would otherwise run against a failed world or send after its `abort`.
fn fail(machine: &mut OnionExitSession, queue: &mut VecDeque<OnionExitEffect>) {
    *queue = VecDeque::from(machine.fail(get_epoch_ms()));
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
    match shared.world.open(&target).await {
        Ok((reader, writer)) => Some(OnionOpenWorld {
            reader: Some(reader),
            writer,
        }),
        Err(error) => {
            tracing::debug!(target = %target.authority(), %error, "onion exit world refused");
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
            sessions: Mutex::new(OnionExitTable::default()),
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
}
