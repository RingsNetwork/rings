//! The exit's session shell: a world-facing symbol's sessions at `h`, each driven by the pure
//! machine of `session::exit` over one world (#834 D2′, D8; paper Algorithms Hop and Reply).
//!
//! ```text
//! ⟦f⟧(from, ā, v, υ):  (ς, d) ← ā;  frame ← dec(v)
//!                      ς known with d ?  ⇒ its driver ← (frame, υ)
//!                      ς known, other d  ⇒ drop (D2′: a loop naming another target)
//!                      ς closed within V ⇒ drop (a tombstone: a late loop never respawns ς)
//!                      ς new, frame T    ⇒ lease(from) ⇒ spawn a driver, its driver ← (frame, υ)
//!                      ς new, other      ⇒ drop (credit, fin or data of no live session)
//!
//! driver(ς):  loop select
//!               (frame, υ)  ─▶ machine.forward
//!               world read  ─▶ machine.world            (only while reply_capacity = Some)
//!               tick        ─▶ machine.tick
//!             perform each effect in order:
//!               Open(t)     policy ∧ world.open(t) ─▶ machine.opened(ok)
//!               Write(w)    world ← w (counted against the byte policy)
//!               Reply(n, c) link sender ← (n, c), awaited: the world is read at the link's rate
//!               Close       end the driver, release the lease and the world
//! ```
//!
//! Laws: the pure machine's (Credit, Totality, Binding, Ack, Order, Fail closed), and:
//!
//! - **Isolation.** A session's reply blocks live in its own pool, so no two sessions share a
//!   block, and a session's inputs reach only its own driver.
//! - **Pause.** The world is read only when the machine reports a capacity, so a `tcp` session
//!   with no credit leaves its socket unread, and resumes on credit.
//! - **Bound.** Only a loop that can open a session (`data` with `T`) creates one, and it is
//!   leased against its previous hop's share before its driver runs, so no previous hop holds
//!   more than its share of the table, bound or not; each driver's inbound queue holds at most
//!   `ONION_EXIT_SESSION_INBOUND` loops, since every loop goes through the one stored sender.

use std::collections::HashMap;
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
use rings_core::utils::get_epoch_ms;
use rings_runtime::sleep;
use rings_runtime::MaybeSend;
use rings_runtime::MaybeSendSync;
use rings_runtime::Spawner;

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
use crate::onion::circuit::ONION_FORWARD_MAX_VALIDITY_MS;
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

    /// Whether the world records the bytes it reads against the exit's byte policy itself, as
    /// they stream, so the shell must not record them again: one place per byte.
    const RECORDS_OWN_READS: bool = false;

    /// Open the world at `target`, already admitted by the exit policy.
    async fn open(&self, target: &OnionProxyTarget) -> Result<(Self::Reader, Self::Writer)>;
}

/// The read half of one session's world.
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
    /// Its driver's inbound queue, the one sender every loop goes through: a full queue then
    /// refuses.
    inbound: mpsc::Sender<OnionExitInput>,
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
            .insert(session, now_ms + ONION_FORWARD_MAX_VALIDITY_MS);
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
        if !sessions.live.contains_key(&arguments.session) {
            let opens = matches!(loop_input.frame, OnionFrame::Data {
                target: Some(_),
                ..
            });
            if !opens
                || sessions.is_buried(&arguments.session, received_at_ms)
                || sessions.live.len() >= ONION_EXIT_MAX_SESSIONS
            {
                return Ok(());
            }
            let Ok(lease) = self.shared.accounting.admit(&self.shared.policy, from) else {
                tracing::debug!(%from, "onion exit session share is full; drop an open");
                return Ok(());
            };
            let (inbound, received) = mpsc::channel(ONION_EXIT_SESSION_INBOUND);
            Spawner::current()?.spawn(drive(
                Arc::clone(&self.shared),
                scope.clone(),
                arguments,
                lease,
                received,
                received_at_ms,
            ));
            sessions
                .live
                .insert(arguments.session, OnionExitSessionHandle {
                    digest: arguments.digest,
                    inbound,
                });
        }
        let Some(handle) = sessions.live.get_mut(&arguments.session) else {
            return Ok(());
        };
        // D2′: a later loop of `ς` that names another target is rejected.
        if handle.digest != arguments.digest {
            return Ok(());
        }
        // A full queue drops the loop; the session's reorder window then fails it closed.
        if handle.inbound.try_send(loop_input).is_err() {
            tracing::debug!(%from, "onion exit session queue is full; drop a loop");
        }
        Ok(())
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
                None => break,
            },
            (reader, read) = reading => {
                if let Some(open) = world.as_mut() {
                    open.reader = Some(reader);
                }
                let now_ms = get_epoch_ms();
                let recorded = |bytes: &Bytes| {
                    W::RECORDS_OWN_READS || record(&shared, bytes.len(), now_ms)
                };
                match read {
                    Ok(Some(bytes)) if recorded(&bytes) => {
                        machine.world(now_ms, OnionWorldRead::Bytes(bytes))
                    }
                    Ok(None) => machine.world(now_ms, OnionWorldRead::Eof),
                    Ok(Some(_)) | Err(_) => machine.fail(now_ms),
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
    let mut queue = std::collections::VecDeque::from(effects);
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
                    queue.extend(machine.fail(get_epoch_ms()));
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
            OnionExitEffect::Reply { next, cell } => {
                let sent = shared
                    .link_sender
                    .send(
                        scope.clone(),
                        OnionLink::new(next),
                        Bytes::from(cell.into_bytes()),
                    )
                    .await;
                if let Err(error) = sent {
                    tracing::debug!(%next, %error, "an onion reply did not leave");
                    queue.extend(machine.fail(get_epoch_ms()));
                }
            }
            OnionExitEffect::Close => return false,
        }
    }
    true
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
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    use bytes::Bytes;

    use super::open;
    use super::OnionExitShared;
    use super::OnionExitTable;
    use super::OnionWorld;
    use super::OnionWorldReader;
    use super::OnionWorldWriter;
    use super::ONION_EXIT_MAX_SESSIONS;
    use crate::error::Result;
    use crate::onion::circuit::OnionLinkSender;
    use crate::onion::circuit::ONION_FORWARD_MAX_VALIDITY_MS;
    use crate::onion::exit_accounting::OnionExitAccounting;
    use crate::onion::session::OnionSessionId;
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
    /// denied or malformed one (#895 D-L5, #843 Q5).
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

    /// A closed session is buried for `V`, so a late loop cannot respawn it, and the tombstones
    /// stay bounded (#895 H5).
    #[test]
    fn test_a_closed_session_is_buried_for_v() {
        let mut table = OnionExitTable::default();
        let session = OnionSessionId::new([3; 16]);

        table.bury(session, 1_000);
        assert!(table.is_buried(&session, 1_000 + ONION_FORWARD_MAX_VALIDITY_MS - 1));
        assert!(!table.is_buried(&session, 1_000 + ONION_FORWARD_MAX_VALIDITY_MS));

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
}
