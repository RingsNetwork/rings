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
use crate::onion::circuit::OnionLoopClient;
use crate::onion::circuit::OnionReply;
use crate::onion::circuit::ONION_FORWARD_MAX_VALIDITY_MS;
use crate::onion::sphinx::builder::OnionApplication;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;

/// Longest wait for `h`'s answer to an open.
const ONION_SESSION_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

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
            Ok(frame) => self.enqueue(&frame, &sink).is_ok(),
            Err(_) => false,
        };
        if !started || self.top_up(&sink).is_err() {
            return;
        }
        let mut tick = Box::pin(sleep(ONION_SESSION_TICK).fuse());
        let mut fin_sent = false;
        loop {
            let step = futures::select! {
                command = commands.next() => match command {
                    Some(OnionStreamCommand::Data(bytes)) => self.upload(bytes, &sink).await,
                    Some(OnionStreamCommand::Fin) if !fin_sent => {
                        fin_sent = true;
                        match self.machine.fin() {
                            Ok(frame) => self.enqueue(&frame, &sink),
                            Err(_) => Err(Error::InvalidData),
                        }
                    }
                    Some(OnionStreamCommand::Fin) => Ok(()),
                    None => return,
                },
                reply = replies.next() => match reply {
                    Some(reply) => {
                        self.credit.replied(reply.received_at_ms);
                        match self.machine.reply(reply.received_at_ms, reply.frame) {
                            Ok(released) => {
                                for event in released {
                                    let forwarded = match event {
                                        OnionClientEvent::Opened | OnionClientEvent::Refused => {
                                            if let Some(opened) = opened.take() {
                                                let _ = opened.send(event == OnionClientEvent::Opened);
                                            }
                                            if event == OnionClientEvent::Refused {
                                                return;
                                            }
                                            Ok(())
                                        }
                                        OnionClientEvent::Data(bytes) => {
                                            events.send(OnionStreamEvent::Data(bytes)).await
                                        }
                                        OnionClientEvent::Fin => {
                                            events.send(OnionStreamEvent::Fin).await
                                        }
                                    };
                                    if forwarded.is_err() {
                                        return;
                                    }
                                }
                                Ok(())
                            }
                            Err(_) => Err(Error::OnionRouteError(OnionRouteError::SessionFailed)),
                        }
                    }
                    None => return,
                },
                _ = tick => {
                    tick = Box::pin(sleep(ONION_SESSION_TICK).fuse());
                    self.keep_alive(&sink)
                },
            };
            let stepped = match step {
                Ok(()) => self.top_up(&sink),
                Err(error) => Err(error),
            };
            if stepped.is_err() {
                let _ = events.send(OnionStreamEvent::Failed).await;
                return;
            }
        }
    }

    /// Send `bytes` as data frames, each no wider than the frame capacity of the class.
    async fn upload(
        &mut self,
        bytes: Bytes,
        sink: &crate::onion::circuit::OnionReplySink,
    ) -> Result<()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let capacity = self.machine.data_capacity(self.class).max(1);
            let chunk = rest.split_to(capacity.min(rest.len()));
            let frame = self.machine.data(chunk).map_err(|_| Error::InvalidData)?;
            self.send(&frame, sink).await?;
        }
        Ok(())
    }

    /// Fail on a persisting gap, and send a credit loop if no loop has left for `V/2`.
    fn keep_alive(&mut self, sink: &crate::onion::circuit::OnionReplySink) -> Result<()> {
        let now_ms = get_epoch_ms();
        self.machine
            .expire(now_ms)
            .map_err(|_| Error::OnionRouteError(OnionRouteError::SessionFailed))?;
        if now_ms.saturating_sub(self.last_forward_ms) >= ONION_SESSION_KEEPALIVE_MS {
            self.credit_loop(sink)?;
        }
        Ok(())
    }

    /// Queue the credit loops the window asks for.
    fn top_up(&mut self, sink: &crate::onion::circuit::OnionReplySink) -> Result<()> {
        let wanted = self
            .window
            .loops_wanted(self.credit.count(get_epoch_ms()), self.class);
        for _ in 0..wanted {
            self.credit_loop(sink)?;
        }
        Ok(())
    }

    /// One credit loop: `k` fresh reply blocks in a `credit` frame, `k + 1` blocks at `h`.
    fn credit_loop(&mut self, sink: &crate::onion::circuit::OnionReplySink) -> Result<()> {
        let count = OnionFrame::credit_capacity(self.class);
        let (surbs, expiry) = self.loops.surbs(&self.route, self.class, count, sink)?;
        self.credit.sent(expiry, count);
        self.enqueue(&OnionFrame::Credit(surbs), sink)
    }

    /// Send one stream-data frame in a fresh loop and wait until it has left: the upload's
    /// backpressure.
    async fn send(
        &mut self,
        frame: &OnionFrame,
        sink: &crate::onion::circuit::OnionReplySink,
    ) -> Result<()> {
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

    /// Queue one control frame (the open, `fin`, credit) in a fresh loop without waiting, so
    /// the driver keeps reading replies while the guard's lane drains.
    fn enqueue(
        &mut self,
        frame: &OnionFrame,
        sink: &crate::onion::circuit::OnionReplySink,
    ) -> Result<()> {
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

    /// `enc(frame)` in the session's class.
    fn encode(&self, frame: &OnionFrame) -> Result<Vec<u8>> {
        frame
            .encode(self.class)
            .map_err(|error| Error::OnionRouteError(OnionRouteError::LoopBuild(error.to_string())))
    }

    /// Count the block a departed loop of expiry `expiry` leaves at `h`.
    fn departed(&mut self, expiry: crate::onion::circuit::OnionExpiry) {
        self.credit.sent(expiry, 1);
        self.last_forward_ms = get_epoch_ms();
    }
}
