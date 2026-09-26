//! The client's session shell: one session over a selected loop route, driven by the pure
//! machines of `session::client` (#834 D2′, D6′, D8).
//!
//! ```text
//! open(route, f, t, b, W):  ς ← uniform;  ā = ς ‖ SHA-256(t);  spawn the driver
//!                           await the first event: Opened ⇒ the stream, Refused | timeout ⇒ error
//!
//! driver:  send data(0, T, ε);  top up                                            (awaited)
//!          loop select
//!            user Data(w)  ─▶ data(n, T?, w′) per chunk w′ ≤ capacity, one loop each (awaited)
//!            user Fin      ─▶ fin(n)                                                (awaited)
//!            user gone     ─▶ abort(n), unless both directions ended                (queued)
//!            reply abort   ─▶ Failed to the user; the session is over
//!            reply         ─▶ credit.replied;  machine.reply ⇒ events to the user
//!            tick          ─▶ gap check;  one credit loop if keep_alive_due          (queued)
//!          top up:  ⌊(min(W, Q_max) − outstanding) / (k + 1)⌋ full credit loops    (queued)
//! ```
//!
//! Departure: stream frames wait for the guard's lane, which is the upload's backpressure;
//! control loops are queued, at most `⌊W / (k + 1)⌋` at a time, so they never hold up the
//! replies the driver reads.
//!
//! Laws: the pure machines' (Target, Sequence, Open result, Credit). Every loop leaves exactly
//! one block at `h` and every credit loop `k + 1`, so upload is `1/(k + 1)` of download when the
//! client only receives (Prop. SURB batching). The driver awaits the user's consumption of each
//! event before it tops up credit, so a slow user slows `h` down instead of losing bytes.

use std::time::Duration;

use bytes::Bytes;
use futures::channel::mpsc;
use futures::future::Either;
use futures::FutureExt;
use futures::SinkExt;
use futures::StreamExt;
use rings_core::utils::get_epoch_ms;
use rings_runtime::sleep;
use rings_runtime::Spawner;
use zeroize::Zeroizing;

use super::client::keep_alive_due;
use super::client::OnionClientCredit;
use super::client::OnionClientEvent;
use super::client::OnionClientSession;
use super::client::OnionCreditWindow;
use super::frame::OnionFrame;
use super::pool::ONION_SURB_POOL_CAPACITY;
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
use crate::onion::sphinx::builder::OnionApplication;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;

/// Longest wait for `h`'s answer to an open: longer than the longest world open at `h` (30 s,
/// rounded to its 250 ms quantum) plus a loop round trip, so a refusal arrives as a refusal and
/// not as a timeout.
const ONION_SESSION_OPEN_TIMEOUT: Duration = Duration::from_secs(45);

/// The period of the driver's tick.
const ONION_SESSION_TICK: Duration = Duration::from_secs(10);

/// Commands and events queued between a stream and its driver.
const ONION_SESSION_QUEUE: usize = 32;

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
}

impl OnionClientStream {
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

    /// Wait for the user's next command: whether it is `fin`, `None` once the user is gone.
    pub(crate) async fn next_is_fin(&mut self) -> Option<bool> {
        self.commands
            .next()
            .await
            .map(|command| matches!(command, OnionStreamCommand::Fin))
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
/// [`OnionRouteError::ExitRefused`] if `h` refused the target or the session failed before it
/// opened, and [`Error::OnionProxyRequestTimedOut`] if `h` did not answer in time.
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
        machine: OnionClientSession::new(target),
        credit: OnionClientCredit::default(),
        last_forward_ms: 0,
        fin_sent: false,
        given_up: false,
        world_ended: false,
    };
    Spawner::current()?.spawn(driver.run(received, events, opened));
    let timeout = sleep(ONION_SESSION_OPEN_TIMEOUT).fuse();
    futures::pin_mut!(timeout);
    match futures::future::select(&mut open_result, timeout).await {
        Either::Left((Ok(true), _)) => Ok(OnionClientStream {
            commands,
            events: delivered,
        }),
        Either::Left((Ok(false) | Err(_), _)) => {
            Err(Error::OnionRouteError(OnionRouteError::ExitRefused))
        }
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
}

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
    /// The driver's loop (see the module diagram).
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<OnionStreamCommand>,
        mut events: mpsc::Sender<OnionStreamEvent>,
        opened: futures::channel::oneshot::Sender<bool>,
    ) {
        let (sink, mut replies) = mpsc::channel::<OnionReply>(ONION_SURB_POOL_CAPACITY);
        let mut opened = Some(opened);
        let started = match self.machine.data(Bytes::new()) {
            Ok(frame) => self.send(&frame, &sink).await.is_ok(),
            Err(_) => false,
        };
        if !started || self.top_up(&sink).is_err() {
            return;
        }
        let mut tick = Box::pin(sleep(ONION_SESSION_TICK).fuse());
        loop {
            let step = futures::select! {
                command = commands.next() => match command {
                    Some(OnionStreamCommand::Data(bytes)) => self.upload(bytes, &sink).await,
                    Some(OnionStreamCommand::Fin) => self.finish(&sink).await,
                    None => {
                        // The user is gone: give the session up, unless both directions ended.
                        if !(self.fin_sent && self.world_ended) {
                            self.abort(&sink);
                        }
                        return;
                    }
                },
                reply = replies.next() => match reply {
                    Some(reply) => match self.on_reply(reply, &mut events, &mut opened).await {
                        OnionReplyFlow::Continue => Ok(()),
                        OnionReplyFlow::Stop => return,
                        OnionReplyFlow::Fail => {
                            Err(Error::OnionRouteError(OnionRouteError::SessionFailed))
                        }
                    },
                    None => return,
                },
                _ = tick => {
                    tick = Box::pin(sleep(ONION_SESSION_TICK).fuse());
                    self.keep_alive(&sink)
                },
            };
            if step.and_then(|()| self.top_up(&sink)).is_err() {
                // Fail closed: `h` learns of it by our `abort`, the user by `Failed`.
                self.abort(&sink);
                let _ = events.send(OnionStreamEvent::Failed).await;
                return;
            }
        }
    }

    /// Apply one reply: count its block, release its frames in order, and hand the events to
    /// the user (the open result to the opener).
    async fn on_reply(
        &mut self,
        reply: OnionReply,
        events: &mut mpsc::Sender<OnionStreamEvent>,
        opened: &mut Option<futures::channel::oneshot::Sender<bool>>,
    ) -> OnionReplyFlow {
        self.credit.replied(reply.received_at_ms);
        let Ok(released) = self.machine.reply(reply.received_at_ms, reply.frame) else {
            return OnionReplyFlow::Fail;
        };
        for event in released {
            let forwarded = match event {
                OnionClientEvent::Opened => {
                    if let Some(opened) = opened.take() {
                        let _ = opened.send(true);
                    }
                    Ok(())
                }
                OnionClientEvent::Refused => {
                    if let Some(opened) = opened.take() {
                        let _ = opened.send(false);
                    }
                    return OnionReplyFlow::Stop;
                }
                OnionClientEvent::Data(bytes) => events.send(OnionStreamEvent::Data(bytes)).await,
                OnionClientEvent::Fin => {
                    self.world_ended = true;
                    events.send(OnionStreamEvent::Fin).await
                }
                // `h` gave the session up and dropped it: a failure, never an end of stream,
                // and nothing to tell `h` back.
                OnionClientEvent::Aborted => {
                    self.given_up = true;
                    let _ = events.send(OnionStreamEvent::Failed).await;
                    return OnionReplyFlow::Stop;
                }
            };
            if forwarded.is_err() {
                return OnionReplyFlow::Stop;
            }
        }
        OnionReplyFlow::Continue
    }

    /// Send `bytes` as data frames, each no wider than the frame capacity of the class.
    async fn upload(&mut self, bytes: Bytes, sink: &OnionReplySink) -> Result<()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let capacity = self.machine.data_capacity(self.class).max(1);
            let chunk = rest.split_to(capacity.min(rest.len()));
            let frame = self.machine.data(chunk).map_err(|_| Error::InvalidData)?;
            self.send(&frame, sink).await?;
        }
        Ok(())
    }

    /// Close this direction with `fin(n)`, once.
    async fn finish(&mut self, sink: &OnionReplySink) -> Result<()> {
        if self.fin_sent {
            return Ok(());
        }
        self.fin_sent = true;
        let frame = self.machine.fin().map_err(|_| Error::InvalidData)?;
        self.send(&frame, sink).await
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

    /// The tick: fail on a persisting gap, and send one credit loop when the keep-alive rule
    /// is due ([`keep_alive_due`]).
    fn keep_alive(&mut self, sink: &OnionReplySink) -> Result<()> {
        let now_ms = get_epoch_ms();
        self.machine
            .expire(now_ms)
            .map_err(|_| Error::OnionRouteError(OnionRouteError::SessionFailed))?;
        if !self.world_ended && keep_alive_due(now_ms, self.last_forward_ms) {
            self.credit_loop(sink)?;
        }
        Ok(())
    }

    /// Queue the credit loops the window asks for, until the guard's lane is full: the rest is
    /// asked for again at the next step.
    fn top_up(&mut self, sink: &OnionReplySink) -> Result<()> {
        if self.world_ended {
            return Ok(());
        }
        let loops = self
            .window
            .credit_loops(self.credit.count(get_epoch_ms()), self.class);
        for _ in 0..loops {
            match self.credit_loop(sink) {
                Ok(()) => {}
                Err(error) if is_lane_full(&error) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// One full credit loop: `k` fresh reply blocks, `k + 1` at `h`.
    fn credit_loop(&mut self, sink: &OnionReplySink) -> Result<()> {
        let blocks = OnionFrame::credit_capacity(self.class);
        let (surbs, expiry) = self.loops.surbs(&self.route, self.class, blocks, sink)?;
        self.enqueue(&OnionFrame::Credit(surbs), sink)?;
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
