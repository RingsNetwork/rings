//! The client's session shell: one session over a selected loop route, driven by the pure
//! machines of `session::client` (#834 D2′, D6′, D8).
//!
//! ```text
//! open(route, f, t, b, W):  ς ← uniform;  ā = ς ‖ SHA-256(t);  spawn the driver
//!                           await the first event: Opened ⇒ the stream, Refused | timeout ⇒ error
//!
//! driver:  queue data(0, T, ε);  top up
//!          loop select
//!            user Data(w)  ─▶ data(n, T?, w′) per chunk w′ ≤ capacity, one loop each (awaited)
//!            user Fin      ─▶ fin(n)                                                (queued)
//!            reply         ─▶ credit.replied;  machine.reply ⇒ events to the user
//!            tick          ─▶ gap check;  a credit loop if no loop left for V/2     (queued)
//!          top up:  ⌈(W − outstanding) / (k + 1)⌉ credit loops, each k fresh blocks (queued)
//! ```
//!
//! Departure: stream data waits for the guard's lane, which is the upload's backpressure;
//! control loops are queued, at most `⌈W / (k + 1)⌉` at a time, so they never hold up the
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
use crate::onion::circuit::ONION_FORWARD_MAX_VALIDITY_MS;
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

/// The keepalive interval `V/2`: a session that sent no loop for this long sends a credit loop,
/// so `h` sees a forward loop well within `V` and keeps unexpired credit.
const ONION_SESSION_KEEPALIVE_MS: u128 = ONION_FORWARD_MAX_VALIDITY_MS / 2;

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
        last_reply_ms: 0,
        fin_sent: false,
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
    /// When the last reply arrived (or the session started).
    last_reply_ms: u128,
    /// Whether this direction's `fin` has been sent.
    fin_sent: bool,
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
        self.last_reply_ms = get_epoch_ms();
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
                        // The user is gone without a `fin`: tell `h`, best effort.
                        self.abort(&sink);
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
                // Fail closed: `h` learns of it by our `fin`, the user by `Failed`.
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
        self.last_reply_ms = reply.received_at_ms;
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

    /// Fail closed toward `h`: queue `fin(n)` if this direction is still open, best effort.
    fn abort(&mut self, sink: &OnionReplySink) {
        if self.fin_sent {
            return;
        }
        self.fin_sent = true;
        if let Ok(frame) = self.machine.fin() {
            if let Err(error) = self.enqueue(&frame, sink) {
                tracing::debug!(%error, "an onion session's closing fin did not leave");
            }
        }
    }

    /// The tick: fail on a persisting gap, and send a credit loop if no loop has left for `V/2`,
    /// or if no reply has come for `V/2` while the ledger reads the window full (blocks lost on
    /// the way, which the ledger cannot see, are then replaced).
    fn keep_alive(&mut self, sink: &OnionReplySink) -> Result<()> {
        let now_ms = get_epoch_ms();
        self.machine
            .expire(now_ms)
            .map_err(|_| Error::OnionRouteError(OnionRouteError::SessionFailed))?;
        if self.world_ended {
            return Ok(());
        }
        let quiet = now_ms.saturating_sub(self.last_forward_ms) >= ONION_SESSION_KEEPALIVE_MS;
        let starved = now_ms.saturating_sub(self.last_reply_ms) >= ONION_SESSION_KEEPALIVE_MS
            && self
                .window
                .credit_loops(self.credit.count(now_ms), self.class)
                .is_empty();
        if quiet || starved {
            self.credit_loop(OnionFrame::credit_capacity(self.class), sink)?;
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
        for blocks in loops {
            match self.credit_loop(blocks, sink) {
                Ok(()) => {}
                Err(error) if is_lane_full(&error) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// One credit loop of `blocks` fresh reply blocks, `blocks + 1` at `h`.
    fn credit_loop(&mut self, blocks: usize, sink: &OnionReplySink) -> Result<()> {
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
