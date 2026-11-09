//! The client's session shell: one session over a selected loop route, driven by the pure
//! machines of `session::client` (#834 D2′, D6′, D8).
//!
//! ```text
//! open(route, f, t, b, W):  ς ← uniform;  ā = ς ‖ SHA-256(t);  spawn the driver
//!                           await the open's answer: Opened ⇒ the stream, Refused ⇒ ExitRefused,
//!                           the driver's own end first ⇒ its error, timeout ⇒ error
//!
//! driver:  send data(0, T, ε);  control(Refill)                                   (awaited)
//!          loop select
//!            user Data(w)  ─▶ data(n, T?, w′) per chunk w′ ≤ capacity, one loop each (awaited)
//!            user Fin      ─▶ fin(n, T?)                                            (awaited)
//!            user gone     ─▶ abort(n), unless both directions ended                (queued)
//!            reply abort   ─▶ Failed to the user; the session is over
//!            replies closed ─▶ abort(n); Failed to the user (a reply was or may be lost here:
//!                             its guard's link retired or dropped, or the queue was full)
//!            reply         ─▶ credit.replied;  machine.reply ⇒ events;  control(Refill)
//!            tick          ─▶ gap and deadline check;  control(Tick)
//!          control(step):  control_loops(step) full credit loops, each credit(m, υ…) with m
//!                          the next forward sequence                               (queued)
//!            Refill ⇒ ⌊(min(W, Q_max) − outstanding) / (k + 1)⌋;  Tick ⇒ keep-alive
//!          tick at:        min(now + T, last_forward + V/2, heard + Δ), each only once it
//!                          is later than now: the keep-alive and the deadline are punctual
//!          any failure ─▶ abort(n);  Failed to the user                             (queued)
//!          witness:        after each reply, Stalled ⇔ a reply is held behind a missing one;
//!                          Failed before Failed reaches the user (session::witness)
//! ```
//!
//! Departure: stream frames wait for the guard's lane, which is the upload's backpressure;
//! control loops are queued, at most `⌊W / (k + 1)⌋` at a time, so they never hold up the
//! replies the driver reads.
//!
//! Laws: the pure machines' (Target, Sequence, Open result, Credit, Keep-alive, Deadline): the
//! driver wakes at the keep-alive's and the deadline's own instants, so the keep-alive leaves
//! `V/2` after the last loop and a reply direction silent for `Δ = 2V` fails the session, sends
//! `abort(n)` toward `h` and hands the user `Failed` (D2′ Liveness). Every loop leaves exactly
//! one block at `h` and every credit loop `k + 1`, so upload is `1/(k + 1)` of download when the
//! client only receives (Prop. SURB batching). The driver awaits the user's consumption of each
//! event before it tops up credit, so a slow user slows `h` down instead of losing bytes.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use futures::channel::mpsc;
use futures::future::Either;
use futures::future::Fuse;
use futures::future::FusedFuture;
use futures::FutureExt;
use futures::SinkExt;
use futures::StreamExt;
use rings_core::utils::get_epoch_ms;
use rings_runtime::sleep;
use zeroize::Zeroizing;

use super::client::keep_alive_at;
use super::client::OnionClientCredit;
use super::client::OnionClientEvent;
use super::client::OnionClientSession;
use super::client::OnionControlStep;
use super::client::OnionCreditWindow;
use super::frame::OnionFrame;
use super::pool::ONION_SURB_POOL_CAPACITY;
use super::witness::OnionSessionWitness;
use super::OnionSessionArguments;
use super::OnionSessionId;
use super::OnionTargetDigest;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::onion::circuit::OnionExpiry;
use crate::onion::circuit::OnionLoopClient;
use crate::onion::circuit::OnionReply;
use crate::onion::circuit::OnionReplySink;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::layer::OnionApplication;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;
#[cfg(rings_native)]
use crate::tasks::NodeTasks;

/// Longest wait for `h`'s answer to an open: longer than the longest world open at `h` (30 s,
/// rounded to its 250 ms quantum) plus a loop round trip, so a refusal arrives as a refusal and
/// not as a timeout.
pub(crate) const ONION_SESSION_OPEN_TIMEOUT: Duration = Duration::from_secs(45);

/// The longest period of the driver's tick, in milliseconds: the gap check's resolution; the
/// keep-alive and the deadline wake the driver at their own instants.
const ONION_SESSION_TICK_MS: u128 = 10_000;

/// Commands and events queued between a stream and its driver.
pub(crate) const ONION_SESSION_QUEUE: usize = 32;

/// The session a client asks for.
pub(crate) struct OnionSessionRequest {
    /// The selected loop route; its symbol hop registers `symbol`.
    pub(crate) route: OnionRoute,
    /// The world-facing symbol.
    pub(crate) symbol: OnionServiceName,
    /// The target `t`.
    pub(crate) target: OnionProxyTarget,
    /// The loop class `b`, the client's choice between throughput and leakage (L3).
    pub(crate) class: OnionLoopClass,
    /// The credit window `W`.
    pub(crate) window: OnionCreditWindow,
}

/// What an open stream hands its user.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum OnionStreamEvent {
    /// World bytes, in order.
    Data(Bytes),
    /// The world closed its stream.
    Fin,
    /// The session failed closed (a gap, or its loops could not leave); nothing more arrives.
    Failed,
}

/// What a user asks of its stream.
enum OnionStreamCommand {
    /// Send stream bytes.
    Data(Bytes),
    /// Close the stream's client-to-world direction.
    Fin,
}

/// An open session, as its user holds it: split into its sending and receiving halves. Dropping
/// both ends the session's driver.
pub(crate) struct OnionClientStream {
    /// The driver's command queue.
    commands: mpsc::Sender<OnionStreamCommand>,
    /// The driver's event queue.
    events: mpsc::Receiver<OnionStreamEvent>,
    /// The session's witness, which the driver writes; only native streams have a reader.
    #[cfg(rings_native)]
    witness: OnionSessionWitness,
    /// The tasks of the node the session was opened on, which its native pump joins (#1085).
    #[cfg(rings_native)]
    tasks: NodeTasks,
}

impl OnionClientStream {
    /// The session's witness: its condition at any instant, without waiting on its events.
    #[cfg(rings_native)]
    pub(crate) fn witness(&self) -> OnionSessionWitness {
        self.witness.clone()
    }

    /// The tasks of the node the session was opened on.
    #[cfg(rings_native)]
    pub(crate) fn tasks(&self) -> &NodeTasks {
        &self.tasks
    }

    /// Split into the sending and the receiving halves.
    pub(crate) fn split(self) -> (OnionStreamSender, OnionStreamReceiver) {
        (
            OnionStreamSender {
                commands: self.commands,
            },
            OnionStreamReceiver {
                events: self.events,
            },
        )
    }
}

/// The driver's side of a stream whose driver is a test: it emits the stream's events and
/// observes its commands; the native `tcp` pump's tests drive their stream with it.
#[cfg(all(test, rings_native))]
pub(crate) struct OnionTestDriver {
    /// The stream's event queue.
    events: mpsc::Sender<OnionStreamEvent>,
    /// The stream's command queue.
    commands: mpsc::Receiver<OnionStreamCommand>,
}

#[cfg(all(test, rings_native))]
impl OnionClientStream {
    /// A stream driven by the returned [`OnionTestDriver`] instead of a session driver.
    pub(crate) fn driven_by_test() -> (Self, OnionTestDriver) {
        let (commands, received) = mpsc::channel(ONION_SESSION_QUEUE);
        let (events, delivered) = mpsc::channel(ONION_SESSION_QUEUE);
        (
            Self {
                commands,
                events: delivered,
                witness: OnionSessionWitness::default(),
                tasks: NodeTasks::new(),
            },
            OnionTestDriver {
                events,
                commands: received,
            },
        )
    }
}

#[cfg(all(test, rings_native))]
impl OnionTestDriver {
    /// Emit `event` to the stream's user.
    pub(crate) async fn emit(&mut self, event: OnionStreamEvent) {
        self.events
            .send(event)
            .await
            .expect("the user holds the stream");
    }

    /// Try to enqueue `event` to the stream's user without waiting; `false` when the event queue
    /// is full.
    pub(crate) fn try_emit(&mut self, event: OnionStreamEvent) -> bool {
        self.events.try_send(event).is_ok()
    }

    /// Wait for the user's next command: whether it is `fin`, `None` once the user is gone.
    pub(crate) async fn next_is_fin(&mut self) -> Option<bool> {
        self.commands
            .next()
            .await
            .map(|command| matches!(command, OnionStreamCommand::Fin))
    }

    /// Wait for the user's next data command: `Some(bytes)` for `Data(bytes)`, `None` on `Fin`
    /// or once the user is gone.
    pub(crate) async fn next_data(&mut self) -> Option<Bytes> {
        match self.commands.next().await {
            Some(OnionStreamCommand::Data(bytes)) => Some(bytes),
            Some(OnionStreamCommand::Fin) | None => None,
        }
    }
}

/// The sending half of an [`OnionClientStream`].
pub(crate) struct OnionStreamSender {
    /// The driver's command queue.
    commands: mpsc::Sender<OnionStreamCommand>,
}

impl OnionStreamSender {
    /// Send stream bytes.
    ///
    /// # Errors
    ///
    /// [`OnionRouteError::TcpStreamClosed`] once the driver has ended.
    pub(crate) async fn send(&mut self, bytes: Bytes) -> Result<()> {
        self.commands
            .send(OnionStreamCommand::Data(bytes))
            .await
            .map_err(|_| Error::OnionRouteError(OnionRouteError::TcpStreamClosed))
    }

    /// Close the client-to-world direction.
    ///
    /// # Errors
    ///
    /// [`OnionRouteError::TcpStreamClosed`] once the driver has ended.
    pub(crate) async fn fin(&mut self) -> Result<()> {
        self.commands
            .send(OnionStreamCommand::Fin)
            .await
            .map_err(|_| Error::OnionRouteError(OnionRouteError::TcpStreamClosed))
    }

    /// Wait until the command queue can accept one command without blocking.
    ///
    /// Cancel-safe: no command is enqueued until [`Self::start_send`] or [`Self::start_fin`]
    /// consumes the ready slot.
    #[cfg(rings_native)]
    pub(crate) async fn ready(&mut self) -> Result<()> {
        std::future::poll_fn(|cx| self.commands.poll_ready(cx))
            .await
            .map_err(|_| Error::OnionRouteError(OnionRouteError::TcpStreamClosed))
    }

    /// Enqueue stream bytes into the slot reserved by [`Self::ready`].
    #[cfg(rings_native)]
    pub(crate) fn start_send(&mut self, bytes: Bytes) -> Result<()> {
        self.commands
            .start_send(OnionStreamCommand::Data(bytes))
            .map_err(|_| Error::OnionRouteError(OnionRouteError::TcpStreamClosed))
    }

    /// Enqueue `fin` into the slot reserved by [`Self::ready`].
    #[cfg(rings_native)]
    pub(crate) fn start_fin(&mut self) -> Result<()> {
        self.commands
            .start_send(OnionStreamCommand::Fin)
            .map_err(|_| Error::OnionRouteError(OnionRouteError::TcpStreamClosed))
    }
}

/// The receiving half of an [`OnionClientStream`].
pub(crate) struct OnionStreamReceiver {
    /// The driver's event queue.
    events: mpsc::Receiver<OnionStreamEvent>,
}

impl OnionStreamReceiver {
    /// The next event, or `None` once the session has ended.
    pub(crate) async fn next(&mut self) -> Option<OnionStreamEvent> {
        self.events.next().await
    }
}

/// Open one session (see the module diagram) and wait for `h` to answer its open.
///
/// # Errors
///
/// [`OnionRouteError::ExitRefused`] only if `h` answered the open with a refusal; the error the
/// driver ended with if the session ended locally before `h` answered (a loop that could not be
/// built or leave, the liveness deadline, a lost reply, `h`'s abort), or
/// [`OnionRouteError::SessionFailed`] if the driver was dropped without an end (its node
/// stopped); and [`Error::OnionProxyRequestTimedOut`] if `h` did not answer in time.
pub(crate) async fn open(
    loops: OnionLoopClient,
    scope: Scope,
    request: OnionSessionRequest,
) -> Result<OnionClientStream> {
    let target = Bytes::from(request.target.authority().into_bytes());
    let arguments = OnionSessionArguments {
        session: OnionSessionId::random(),
        digest: OnionTargetDigest::of(&target),
    };
    let (commands, received) = mpsc::channel(ONION_SESSION_QUEUE);
    let (opened, mut open_result) = futures::channel::oneshot::channel();
    let (events, delivered) = mpsc::channel(ONION_SESSION_QUEUE);
    let driver = OnionSessionDriver {
        loops,
        scope,
        application: OnionApplication {
            symbol: request.symbol,
            arguments: arguments.encode(),
        },
        route: request.route,
        class: request.class,
        window: request.window,
        machine: OnionClientSession::new(target, get_epoch_ms()),
        credit: OnionClientCredit::default(),
        last_forward_ms: 0,
        fin_sent: false,
        given_up: false,
        world_ended: false,
        witness: OnionSessionWitness::default(),
    };
    #[cfg(rings_native)]
    let witness = driver.witness.clone();
    let tasks = driver.scope.tasks().clone();
    tasks.spawn(driver.run(received, events, opened))?;
    let timeout = sleep(ONION_SESSION_OPEN_TIMEOUT).fuse();
    futures::pin_mut!(timeout);
    match futures::future::select(&mut open_result, timeout).await {
        Either::Left((Ok(Ok(true)), _)) => Ok(OnionClientStream {
            commands,
            events: delivered,
            #[cfg(rings_native)]
            witness,
            #[cfg(rings_native)]
            tasks,
        }),
        Either::Left((Ok(Ok(false)), _)) => {
            Err(Error::OnionRouteError(OnionRouteError::ExitRefused))
        }
        Either::Left((Ok(Err(ended)), _)) => Err(ended),
        Either::Left((Err(_), _)) => Err(Error::OnionRouteError(OnionRouteError::SessionFailed)),
        Either::Right(_) => Err(Error::OnionProxyRequestTimedOut),
    }
}

/// The driver of one client session.
struct OnionSessionDriver {
    /// The loop client the session's loops leave through.
    loops: OnionLoopClient,
    /// The scope the loops are sent under.
    scope: Scope,
    /// `(f, ā)`, the same on every loop.
    application: OnionApplication,
    /// The route every loop takes.
    route: OnionRoute,
    /// The class of every loop.
    class: OnionLoopClass,
    /// `W`.
    window: OnionCreditWindow,
    /// The pure session machine.
    machine: OnionClientSession,
    /// The blocks outstanding at `h`.
    credit: OnionClientCredit,
    /// When the last loop left.
    last_forward_ms: u128,
    /// Whether this direction's `fin` has been sent.
    fin_sent: bool,
    /// Whether the session is given up, by `h` or by the client: nothing more is sent.
    given_up: bool,
    /// Whether the world's `fin` has arrived: `h` replies nothing more, so no credit is sent.
    world_ended: bool,
    /// The session's witness, read by the stream's user.
    witness: OnionSessionWitness,
}

/// The opener's half of the open's answer: `Ok(true)` once `h` opened the session, `Ok(false)`
/// once `h` refused it, or the error the session ended with before `h` answered.
type OnionOpenAnswer = futures::channel::oneshot::Sender<Result<bool>>;

/// An in-flight stream frame send on the loop client.
type LoopSend = Pin<Box<rings_runtime::maybe_send!(dyn Future<Output = Result<OnionExpiry>>)>>;

/// What the driver does after one reply.
enum OnionReplyFlow {
    /// Keep driving.
    Continue,
    /// The session is over (refused, or the user is gone).
    Stop,
    /// The session failed closed.
    Fail,
}

impl OnionSessionDriver {
    /// Drive the session, then answer the opener if `h` never did: a session that ends before
    /// `h` answers its open reports why it ended, never a refusal, which only `h` gives (#1118).
    /// An end without an error before the answer is `h`'s abort: a failed session.
    async fn run(
        mut self,
        commands: mpsc::Receiver<OnionStreamCommand>,
        events: mpsc::Sender<OnionStreamEvent>,
        opened: OnionOpenAnswer,
    ) {
        let mut opened = Some(opened);
        let ended = self.drive(commands, events, &mut opened).await;
        if let Some(opened) = opened {
            let why = ended
                .err()
                .unwrap_or(Error::OnionRouteError(OnionRouteError::SessionFailed));
            let _ = opened.send(Err(why));
        }
    }

    /// The driver's loop (see the module diagram): `Ok` once the session is over, or the error
    /// it failed with.
    async fn drive(
        &mut self,
        mut commands: mpsc::Receiver<OnionStreamCommand>,
        mut events: mpsc::Sender<OnionStreamEvent>,
        opened: &mut Option<OnionOpenAnswer>,
    ) -> Result<()> {
        let (sink, mut replies) = mpsc::channel::<OnionReply>(ONION_SURB_POOL_CAPACITY);
        let frame = self
            .machine
            .data(Bytes::new())
            .map_err(|_| Error::InvalidData)?;
        self.send(&frame, &sink).await?;
        self.control(OnionControlStep::Refill, &sink)?;
        let mut tick = Box::pin(sleep(self.tick_after(get_epoch_ms())).fuse());
        let mut sending: Fuse<LoopSend> = Fuse::terminated();
        let mut upload_rest = Bytes::new();
        let mut fin_requested = false;
        let mut pending_events = VecDeque::new();
        let mut refill_pending = false;
        loop {
            if sending.is_terminated() {
                if let Err(error) =
                    self.start_next_send(&mut sending, &mut upload_rest, &mut fin_requested, &sink)
                {
                    self.fail_closed(&sink, &mut events);
                    return Err(error);
                }
            }
            let can_send_command = sending.is_terminated();
            let (step, control) = futures::select! {
                sent = sending => match sent {
                    Ok(expiry) => {
                        self.departed(expiry);
                        (Ok(()), None)
                    }
                    Err(error) => (Err(error), None),
                },
                command = next_command(&mut commands, can_send_command).fuse() => match command {
                    Some(OnionStreamCommand::Data(bytes)) => {
                        upload_rest = bytes;
                        (Ok(()), None)
                    }
                    Some(OnionStreamCommand::Fin) => {
                        fin_requested = true;
                        (Ok(()), None)
                    }
                    None => {
                        // The user is gone: give the session up, unless both directions ended.
                        if !(self.fin_sent && self.world_ended) {
                            self.abort(&sink);
                        }
                        return Ok(());
                    }
                },
                reply_step = step_reply_or_flush(&mut replies, &mut events, &mut pending_events).fuse() => match reply_step {
                    ReplyOrFlush::Flushed(Ok(())) => {
                        let control = refill_pending.then_some(OnionControlStep::Refill);
                        refill_pending = false;
                        (Ok(()), control)
                    }
                    ReplyOrFlush::Flushed(Err(())) => return Ok(()),
                    ReplyOrFlush::Reply(Some(reply)) => match self.on_reply(reply, &mut events, &mut pending_events, opened) {
                        OnionReplyFlow::Continue if pending_events.is_empty() => {
                            (Ok(()), Some(OnionControlStep::Refill))
                        }
                        OnionReplyFlow::Continue => {
                            refill_pending = true;
                            (Ok(()), None)
                        }
                        OnionReplyFlow::Stop => return Ok(()),
                        OnionReplyFlow::Fail => (
                            Err(Error::OnionRouteError(OnionRouteError::SessionFailed)),
                            None,
                        ),
                    },
                    // The tag table closed the queue: a reply of this session was lost here, or
                    // may have been (`circuit::tags`, Fate), so the session fails closed.
                    ReplyOrFlush::Reply(None) => (
                        Err(Error::OnionRouteError(OnionRouteError::SessionFailed)),
                        None,
                    ),
                },
                _ = tick => {
                    tick = Box::pin(sleep(self.tick_after(get_epoch_ms())).fuse());
                    (self.expire(), Some(OnionControlStep::Tick))
                },
            };
            let controlled = step.and_then(|()| match control {
                Some(control) => self.control(control, &sink),
                None => Ok(()),
            });
            if let Err(error) = controlled {
                // Fail closed: `h` learns of it by our `abort`, the user by `Failed`.
                self.fail_closed(&sink, &mut events);
                return Err(error);
            }
        }
    }

    /// Apply one reply: count its block, release its frames in order, and queue the events for
    /// the user (the open result to the opener).
    fn on_reply(
        &mut self,
        reply: OnionReply,
        events: &mut mpsc::Sender<OnionStreamEvent>,
        pending_events: &mut VecDeque<OnionStreamEvent>,
        opened: &mut Option<OnionOpenAnswer>,
    ) -> OnionReplyFlow {
        self.credit.replied(reply.received_at_ms);
        let Ok(released) = self.machine.reply(reply.received_at_ms, reply.frame) else {
            return OnionReplyFlow::Fail;
        };
        self.witness.record_stalled(self.machine.stalled());
        for event in released {
            match event {
                OnionClientEvent::Opened => {
                    if let Some(opened) = opened.take() {
                        let _ = opened.send(Ok(true));
                    }
                }
                OnionClientEvent::Refused => {
                    if let Some(opened) = opened.take() {
                        let _ = opened.send(Ok(false));
                    }
                    return OnionReplyFlow::Stop;
                }
                OnionClientEvent::Data(bytes) => {
                    pending_events.push_back(OnionStreamEvent::Data(bytes));
                }
                OnionClientEvent::Fin => {
                    self.world_ended = true;
                    pending_events.push_back(OnionStreamEvent::Fin);
                }
                // `h` gave the session up and dropped it: a failure, never an end of stream,
                // and nothing to tell `h` back.
                OnionClientEvent::Aborted => {
                    self.given_up = true;
                    self.witness_failure();
                    pending_events.clear();
                    let _ = events.try_send(OnionStreamEvent::Failed);
                    return OnionReplyFlow::Stop;
                }
            }
        }
        OnionReplyFlow::Continue
    }

    /// Fail closed: queue `abort(n)` toward `h`, mark the witness failed, and enqueue `Failed`
    /// if the user's event queue has room (otherwise dropping `events` closes the stream as
    /// failed without blocking on a full queue).
    fn fail_closed(&mut self, sink: &OnionReplySink, events: &mut mpsc::Sender<OnionStreamEvent>) {
        self.abort(sink);
        self.witness_failure();
        let _ = events.try_send(OnionStreamEvent::Failed);
    }

    /// Record the session's failure in its witness before the user hears of it, unless the
    /// world's `fin` already made the stream whole (`session::witness`, Laws Report before end,
    /// Whole stream).
    fn witness_failure(&self) {
        if !self.world_ended {
            self.witness.record_failed();
        }
    }

    /// Start the next in-flight loop send for `upload_rest` or `fin_requested`, one frame at a
    /// time.
    fn start_next_send(
        &mut self,
        sending: &mut Fuse<LoopSend>,
        upload_rest: &mut Bytes,
        fin_requested: &mut bool,
        sink: &OnionReplySink,
    ) -> Result<()> {
        if !upload_rest.is_empty() {
            let capacity = self.machine.data_capacity(self.class).max(1);
            let chunk = upload_rest.split_to(capacity.min(upload_rest.len()));
            let frame = self.machine.data(chunk).map_err(|_| Error::InvalidData)?;
            *sending = self.send_loop(&frame, sink)?.fuse();
        } else if *fin_requested {
            *fin_requested = false;
            if !self.fin_sent {
                self.fin_sent = true;
                let frame = self.machine.fin().map_err(|_| Error::InvalidData)?;
                *sending = self.send_loop(&frame, sink)?.fuse();
            }
        }
        Ok(())
    }

    /// Build a future that sends `frame` in a fresh loop and resolves once it has left.
    fn send_loop(&self, frame: &OnionFrame, sink: &OnionReplySink) -> Result<LoopSend> {
        let value = self.encode(frame)?;
        let loops = self.loops.clone();
        let scope = self.scope.clone();
        let route = self.route.clone();
        let application = self.application.clone();
        let class = self.class;
        let sink = sink.clone();
        Ok(Box::pin(async move {
            loops
                .send(&scope, &route, &application, class, &value, &sink)
                .await
        }))
    }

    /// Give the session up toward `h`: queue `abort(n)` once, best effort, so `h` drops the
    /// session and stops reading the world at once.
    fn abort(&mut self, sink: &OnionReplySink) {
        if self.given_up {
            return;
        }
        self.given_up = true;
        if let Ok(frame) = self.machine.abort() {
            if let Err(error) = self.enqueue(&frame, sink) {
                tracing::debug!(%error, "an onion session's abort did not leave");
            }
        }
    }

    /// The wait from `now` to the driver's next tick: the earliest of `now + T`, the keep-alive
    /// (while credit is still sent) and the deadline, each counted only once it is later than
    /// `now`, so a keep-alive the guard's lane refused is retried a period later, not in a spin.
    fn tick_after(&self, now_ms: u128) -> Duration {
        let keep_alive_ms = (!self.world_ended).then(|| keep_alive_at(self.last_forward_ms));
        let wake_ms = [keep_alive_ms, self.machine.deadline_ms()]
            .into_iter()
            .flatten()
            .filter(|at_ms| *at_ms > now_ms)
            .fold(now_ms.saturating_add(ONION_SESSION_TICK_MS), u128::min);
        Duration::from_millis(u64::try_from(wake_ms.saturating_sub(now_ms)).unwrap_or(u64::MAX))
    }

    /// The tick's checks: a reply missing for longer than the reorder window, or a reply
    /// direction silent for `Δ` (Law Deadline), fails the session.
    fn expire(&mut self) -> Result<()> {
        self.machine
            .expire(get_epoch_ms())
            .map_err(|_| Error::OnionRouteError(OnionRouteError::SessionFailed))
    }

    /// Queue the credit loops [`OnionCreditWindow::control_loops`] asks for after `step`, until
    /// the guard's lane is full: the rest is asked for again after the next reply, or by the
    /// keep-alive. Nothing once the world's `fin` has arrived, since `h` replies nothing more.
    fn control(&mut self, step: OnionControlStep, sink: &OnionReplySink) -> Result<()> {
        if self.world_ended {
            return Ok(());
        }
        let now_ms = get_epoch_ms();
        let loops = self.window.control_loops(
            step,
            self.credit.count(now_ms),
            self.class,
            now_ms,
            self.last_forward_ms,
        );
        for _ in 0..loops {
            match self.credit_loop(sink) {
                Ok(()) => {}
                Err(error) if is_lane_full(&error) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// One full credit loop: `k` fresh reply blocks, `k + 1` at `h`, and the forward high-water
    /// mark `m` (Law Mark).
    fn credit_loop(&mut self, sink: &OnionReplySink) -> Result<()> {
        let blocks = OnionFrame::credit_capacity(self.class);
        let (surbs, expiry) = self.loops.surbs(&self.route, self.class, blocks, sink)?;
        let frame = self.machine.credit(surbs).map_err(|_| Error::InvalidData)?;
        self.enqueue(&frame, sink)?;
        self.credit.sent(get_epoch_ms(), expiry, blocks);
        Ok(())
    }

    /// Send one stream frame in a fresh loop and wait until it has left: the upload's
    /// backpressure, and the open's and `fin`'s wait for room on the guard's lane.
    async fn send(&mut self, frame: &OnionFrame, sink: &OnionReplySink) -> Result<()> {
        let value = self.encode(frame)?;
        let expiry = self
            .loops
            .send(
                &self.scope,
                &self.route,
                &self.application,
                self.class,
                &value,
                sink,
            )
            .await?;
        self.departed(expiry);
        Ok(())
    }

    /// Queue one control frame (credit, or a closing `fin`) in a fresh loop without waiting, so
    /// the driver keeps reading replies while the guard's lane drains.
    fn enqueue(&mut self, frame: &OnionFrame, sink: &OnionReplySink) -> Result<()> {
        let value = self.encode(frame)?;
        let expiry = self.loops.enqueue(
            &self.scope,
            &self.route,
            &self.application,
            self.class,
            &value,
            sink,
        )?;
        self.departed(expiry);
        Ok(())
    }

    /// `enc(frame)` in the session's class; the encoding holds reply-block seeds in a credit
    /// frame, so it is zeroized on drop.
    fn encode(&self, frame: &OnionFrame) -> Result<Zeroizing<Vec<u8>>> {
        frame
            .encode(self.class)
            .map_err(|error| Error::OnionRouteError(OnionRouteError::LoopBuild(error.to_string())))
    }

    /// Count the block a departed loop of expiry `expiry` leaves at `h`.
    fn departed(&mut self, expiry: OnionExpiry) {
        let now_ms = get_epoch_ms();
        self.credit.sent(now_ms, expiry, 1);
        self.last_forward_ms = now_ms;
    }
}

/// Whether `error` is the guard's lane refusing a cell for a full bound, which a later step
/// retries.
fn is_lane_full(error: &Error) -> bool {
    matches!(error, Error::OnionQueueAdmission { reason, .. } if reason.is_full())
}

/// Receive the next user command only while no upload loop send is in flight.
async fn next_command(
    commands: &mut mpsc::Receiver<OnionStreamCommand>,
    idle: bool,
) -> Option<OnionStreamCommand> {
    if !idle {
        return futures::future::pending().await;
    }
    commands.next().await
}

/// Outcome of [`step_reply_or_flush`].
enum ReplyOrFlush {
    /// All queued stream events were flushed to the user's event channel.
    Flushed(std::result::Result<(), ()>),
    /// The next reply arrived (or the reply channel closed) while no events were pending.
    Reply(Option<OnionReply>),
}

/// Drain `pending` stream events into `events` when non-empty; otherwise wait for the next
/// reply from `replies`, preserving backpressure before refilling credit.
///
/// Cancel-safe: each event is popped from `pending` only in the poll that observes readiness on
/// `events`.
async fn step_reply_or_flush(
    replies: &mut mpsc::Receiver<OnionReply>,
    events: &mut mpsc::Sender<OnionStreamEvent>,
    pending: &mut VecDeque<OnionStreamEvent>,
) -> ReplyOrFlush {
    if pending.is_empty() {
        return ReplyOrFlush::Reply(replies.next().await);
    }
    let flushed = std::future::poll_fn(|cx| {
        while pending.front().is_some() {
            match events.poll_ready(cx) {
                Poll::Ready(Ok(())) => {
                    let event = pending.pop_front().expect("front");
                    if events.start_send(event).is_err() {
                        return Poll::Ready(Err(()));
                    }
                }
                Poll::Ready(Err(_)) => return Poll::Ready(Err(())),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    })
    .await;
    ReplyOrFlush::Flushed(flushed)
}
