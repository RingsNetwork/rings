//! Browser transport-relay engine — the WebTransport endpoint.
//!
//! Browsers have no raw sockets, so the relay's local backend here is a *WebTransport*
//! server (a URL). This is the browser counterpart of the native socket engine
//! ([`engine`](crate::extension::transport::engine)); it presents the same
//! `write`/`shutdown`/`close` surface so the relay interpreter dispatches uniformly, and
//! opens sessions via the relay's own `RelayEffect::Connect`.
//!
//! Mapping: `TransportKind::Tcp` → a WebTransport **bidirectional stream**;
//! `TransportKind::Udp` → WebTransport **datagrams**. Reads from the local side become
//! `Frame::Data` to the peer (the event trace flowing outward); peer `Write` is written
//! to the stream; `Shutdown` closes the send side; `Close` closes the session.
//!
//! Single-threaded (wasm): tasks are `spawn_local`, promises are awaited via
//! `JsFuture`, and the session table is a plain `Mutex` (no contention). The target-neutral queue
//! budget, drain ownership, and non-reusing allocator are unit-tested natively; the JS binding
//! path is compile-checked but still needs browser integration coverage. Requires
//! `--cfg=web_sys_unstable_apis`.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use bytes::Bytes;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_core::dht::Did;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::ReadableStream;
use web_sys::ReadableStreamDefaultReader;
use web_sys::WebTransport;
use web_sys::WritableStream;
use web_sys::WritableStreamDefaultWriter;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Scope;
use crate::extension::protocols::relay::RelayCommand;
use crate::extension::transport::allocate_non_reusing;
use crate::extension::transport::EffectEnqueue;
use crate::extension::transport::Frame;
use crate::extension::transport::Initiator;
use crate::extension::transport::OutboundDrainState;
use crate::extension::transport::OutboundQueueBudget;
use crate::extension::transport::SessionKey;
use crate::extension::transport::TransportKind;

/// A peer→local op that arrived while the WebTransport was still opening (no writer yet), held
/// until the handshake completes — the browser mirror of the native engine's buffered channel.
enum Outbound {
    /// Bytes to write to the local stream.
    Data(Bytes),
    /// The peer half-closed (FIN): close the send side.
    Shutdown,
}

/// One total transition of the unique writer-drain task.
enum OutboundDrainStep {
    Operation(WritableStreamDefaultWriter, Outbound),
    Complete,
    Superseded,
    InvariantViolation,
}

impl Outbound {
    fn data_bytes(&self) -> usize {
        match self {
            Self::Data(bytes) => bytes.len(),
            Self::Shutdown => 0,
        }
    }
}

/// A WebTransport-backed session. It is born `Opening`: the slot exists *before* the `open()`
/// handshake resolves — the browser counterpart of native's pre-dial `register` — so peer
/// frames arriving mid-handshake are not lost. `Write`/`Shutdown` queue onto the slot and a
/// peer `Close` drops it (aborting the promote); once the handshake succeeds the slot is
/// [`promote`](WtSessions::promote)d to `Ready`. `generation` mirrors the native engine: a read
/// loop only tears down the handle whose generation matches its own, and a promote only lands
/// while its generation is still current (ABA safety).
enum SessionHandle {
    /// Connect in flight: peer→local ops queued until the writer exists.
    Opening {
        queue: VecDeque<Outbound>,
        budget: OutboundQueueBudget,
        generation: u64,
    },
    /// Handshake done: the live peer→local writer and the session's WebTransport.
    Ready {
        writer: WritableStreamDefaultWriter,
        transport: WebTransport,
        queue: VecDeque<Outbound>,
        budget: OutboundQueueBudget,
        drain: OutboundDrainState,
        generation: u64,
    },
}

impl SessionHandle {
    /// The per-insert stamp, regardless of phase (used by generation-checked teardown).
    fn generation(&self) -> u64 {
        match self {
            SessionHandle::Opening { generation, .. } => *generation,
            SessionHandle::Ready { generation, .. } => *generation,
        }
    }
}

/// Browser relay engine: WebTransport sessions keyed by [`SessionKey`].
///
/// Like the native engine, sessions are keyed by the full `(peer, namespace, session,
/// initiator)` (owner rejection + bidirectional-open safety) and handles carry a generation
/// so a stale read loop never drops — or peer-closes — a newer reuse of the same key.
#[derive(Default)]
pub(crate) struct WtSessions {
    map: Mutex<HashMap<SessionKey, SessionHandle>>,
    generations: AtomicU64,
}

impl WtSessions {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start opening a WebTransport session to `url` for the session identified by `key`.
    /// The slot is registered as `Opening` before spawning the handshake, so the extension
    /// transition never awaits network I/O and peer frames can queue or abort it. On failure —
    /// or if a peer `Close` superseded the slot — teardown affects only the current generation.
    pub fn connect(
        self: Arc<Self>,
        scope: Scope,
        key: SessionKey,
        url: String,
        kind: TransportKind,
    ) -> EffectEnqueue {
        debug_assert_eq!(
            scope.namespace(),
            key.namespace.as_str(),
            "relay engine acted with a scope outside the session's namespace"
        );
        let Some(generation) = self.open_slot(key.clone()) else {
            return EffectEnqueue::Failed;
        };
        spawn_local(async move {
            self.finish_connect(scope, key, url, kind, generation).await;
        });
        EffectEnqueue::Enqueued
    }

    async fn finish_connect(
        self: Arc<Self>,
        scope: Scope,
        key: SessionKey,
        url: String,
        kind: TransportKind,
        generation: u64,
    ) {
        match open(url.as_str(), kind).await {
            Ok((transport, readable, writer)) => {
                // Promote to Ready iff still current; a peer Close during the handshake removed
                // the slot, so a stale open must discard its transport and stay silent.
                if let Some(start_drain) = self.promote(&key, generation, writer, transport.clone())
                {
                    if start_drain {
                        self.spawn_writer_loop(scope.clone(), key.clone(), generation);
                    }
                    self.spawn_read_loop(scope, key, readable, generation);
                } else {
                    transport.close();
                }
            }
            Err(e) => {
                tracing::error!("WebTransport connect to {url} failed: {e:?}");
                // Drop the opening slot and tell the peer — but only if we are still its owner
                // (a peer Close during the handshake already tore it down and told the peer).
                self.close_current_and_notify(&scope, &key, generation)
                    .await;
            }
        }
    }

    /// Deliver peer bytes to a session's local stream. Queued if the session is still opening,
    /// dropped if unknown — a non-owner peer's key never resolves, so it cannot write to a
    /// session it does not own.
    pub fn write(self: &Arc<Self>, scope: Scope, key: SessionKey, bytes: Bytes) -> EffectEnqueue {
        self.enqueue(scope, key, Outbound::Data(bytes))
    }

    /// Half-close a session's send side (peer sent FIN). Queued if still opening.
    pub fn shutdown(self: &Arc<Self>, scope: Scope, key: SessionKey) -> EffectEnqueue {
        self.enqueue(scope, key, Outbound::Shutdown)
    }

    /// Release a session while applying a `Close` effect.
    ///
    /// The reducer already forgot this key, so this path deliberately does not self-inject an
    /// `Untrack` event into the active effect turn.
    pub fn close_for_effect(&self, key: &SessionKey) {
        let removed = self.lock_sessions().remove(key);
        self.finish_close_without_feedback(removed);
    }

    /// Close a session **only if** its handle still has `generation` (ABA safety). Returns
    /// whether it removed it; a stale read loop gets `false` and must not peer-`Close` either.
    async fn close_if_current(&self, scope: &Scope, key: &SessionKey, generation: u64) -> bool {
        let removed = {
            let mut map = self.lock_sessions();
            let current = map.get(key).map(|handle| handle.generation());
            (current == Some(generation))
                .then(|| map.remove(key))
                .flatten()
        };
        self.finish_close(scope, key, removed).await
    }

    /// Close the exact generation and emit its terminal peer event once.
    async fn close_current_and_notify(&self, scope: &Scope, key: &SessionKey, generation: u64) {
        if self.close_if_current(scope, key, generation).await {
            let _ = send_frame(scope, key.peer, Frame::Close {
                session: key.session,
                from_opener: matches!(key.initiator, Initiator::Local),
            })
            .await;
        }
    }

    /// Shared teardown tail: close the WebTransport (only a `Ready` slot owns one) and
    /// `Untrack` — only if a handle was removed. Returns whether it removed one.
    async fn finish_close(
        &self,
        scope: &Scope,
        key: &SessionKey,
        removed: Option<SessionHandle>,
    ) -> bool {
        let Some(handle) = removed else {
            return false;
        };
        if let SessionHandle::Ready { transport, .. } = handle {
            transport.close();
        }
        inject_untrack(scope, key).await;
        true
    }

    fn finish_close_without_feedback(&self, removed: Option<SessionHandle>) -> bool {
        let Some(handle) = removed else {
            return false;
        };
        if let SessionHandle::Ready { transport, .. } = handle {
            transport.close();
        }
        true
    }

    /// Register a fresh `Opening` slot for `key` before the handshake, returning its
    /// generation. The mirror of native's pre-dial `register`.
    fn open_slot(&self, key: SessionKey) -> Option<u64> {
        let generation = allocate_non_reusing(&self.generations)?;
        self.insert(key, SessionHandle::Opening {
            queue: VecDeque::new(),
            budget: OutboundQueueBudget::default(),
            generation,
        });
        Some(generation)
    }

    /// Promote the `Opening` slot for `key` to `Ready` with the just-opened `writer`/
    /// `transport`, preserving peer ops queued during the handshake in arrival order. Returns
    /// whether a writer drain must start, or `None` (caller discards `transport`) if the slot is
    /// gone or its generation was superseded. Performs no `await`, so installing `Ready` and
    /// transferring its deferred trace are atomic against inbound dispatch.
    fn promote(
        &self,
        key: &SessionKey,
        generation: u64,
        writer: WritableStreamDefaultWriter,
        transport: WebTransport,
    ) -> Option<bool> {
        let mut map = self.lock_sessions();
        let (queue, budget) = match map.get(key) {
            Some(SessionHandle::Opening {
                generation: current,
                ..
            }) if *current == generation => match map.remove(key) {
                Some(SessionHandle::Opening { queue, budget, .. }) => (queue, budget),
                _ => return None,
            },
            _ => return None,
        };
        let mut drain = OutboundDrainState::Idle;
        let start_drain = !queue.is_empty() && drain.claim();
        map.insert(key.clone(), SessionHandle::Ready {
            writer,
            transport,
            queue,
            budget,
            drain,
            generation,
        });
        Some(start_drain)
    }

    /// Enqueue one peer-to-local effect under the fixed operation/byte budget. Starting the drain
    /// is an idempotent algebraic transition (`Idle -> Active`); the Promise itself is awaited only
    /// by the spawned shell task, never by the ordered extension turn.
    fn enqueue(self: &Arc<Self>, scope: Scope, key: SessionKey, op: Outbound) -> EffectEnqueue {
        let mut map = self.lock_sessions();
        let Some(handle) = map.get_mut(&key) else {
            return EffectEnqueue::Missing;
        };
        let data_bytes = op.data_bytes();
        let (generation, start_drain, admitted) = match handle {
            SessionHandle::Opening {
                queue,
                budget,
                generation,
            } => {
                let admitted = budget.try_reserve(data_bytes);
                if admitted {
                    queue.push_back(op);
                }
                (*generation, false, admitted)
            }
            SessionHandle::Ready {
                queue,
                budget,
                drain,
                generation,
                ..
            } => {
                let admitted = budget.try_reserve(data_bytes);
                if admitted {
                    queue.push_back(op);
                }
                let start_drain = admitted && drain.claim();
                (*generation, start_drain, admitted)
            }
        };
        if admitted {
            drop(map);
            if start_drain {
                self.spawn_writer_loop(scope, key, generation);
            }
            return EffectEnqueue::Enqueued;
        }
        let removed = (map.get(&key).map(SessionHandle::generation) == Some(generation))
            .then(|| map.remove(&key))
            .flatten();
        drop(map);
        self.finish_close_without_feedback(removed);
        EffectEnqueue::Failed
    }

    /// Drain the current generation serially. At most one Promise and the bounded local queue own
    /// payload bytes at a time; transient browser backpressure delays this task rather than
    /// blocking an extension transition or being misclassified as a permanent failure.
    fn spawn_writer_loop(self: &Arc<Self>, scope: Scope, key: SessionKey, generation: u64) {
        let sessions = self.clone();
        spawn_local(async move {
            loop {
                let (writer, op) = match sessions.take_ready_outbound(&key, generation) {
                    OutboundDrainStep::Operation(writer, op) => (writer, op),
                    OutboundDrainStep::Complete | OutboundDrainStep::Superseded => break,
                    OutboundDrainStep::InvariantViolation => {
                        tracing::error!("WebTransport outbound queue budget diverged for {key:?}");
                        sessions
                            .close_current_and_notify(&scope, &key, generation)
                            .await;
                        break;
                    }
                };
                let promise = match op {
                    Outbound::Data(bytes) => {
                        let chunk = Uint8Array::from(bytes.as_ref());
                        writer.write_with_chunk(chunk.as_ref())
                    }
                    Outbound::Shutdown => writer.close(),
                };
                if JsFuture::from(promise).await.is_err() {
                    sessions
                        .close_current_and_notify(&scope, &key, generation)
                        .await;
                    break;
                }
            }
        });
    }

    /// Pop one queued operation for the exact ready generation. Emptying the queue clears the
    /// drain owner under the same lock, so a concurrent enqueue either belongs to this task or
    /// observes `Idle` and starts its successor.
    fn take_ready_outbound(&self, key: &SessionKey, generation: u64) -> OutboundDrainStep {
        let mut map = self.lock_sessions();
        let Some(SessionHandle::Ready {
            writer,
            queue,
            budget,
            drain,
            generation: current,
            ..
        }) = map.get_mut(key)
        else {
            return OutboundDrainStep::Superseded;
        };
        if *current != generation {
            return OutboundDrainStep::Superseded;
        }
        let Some(op) = queue.pop_front() else {
            drain.release();
            return OutboundDrainStep::Complete;
        };
        if !budget.release(op.data_bytes()) {
            return OutboundDrainStep::InvariantViolation;
        }
        OutboundDrainStep::Operation(writer.clone(), op)
    }

    fn insert(&self, key: SessionKey, handle: SessionHandle) {
        let mut map = self.lock_sessions();
        // Defensive: if a session already exists for this key (a duplicate Open that
        // slipped past the pure reject, or a key reuse), close the old WebTransport
        // before replacing it, so it cannot keep running or later tear down the new one.
        // An `Opening` slot owns no transport yet — its in-flight open will fail to promote.
        if let Some(SessionHandle::Ready { transport, .. }) = map.insert(key, handle) {
            transport.close();
        }
    }

    /// Recover the single-threaded browser table after an unwinding test panic instead of
    /// translating poison into an unrelated missing-session transition.
    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<SessionKey, SessionHandle>> {
        self.map.lock().unwrap_or_else(|poisoned| {
            tracing::error!("recovering poisoned WebTransport session table");
            poisoned.into_inner()
        })
    }

    /// Spawn the local→peer read loop for `readable`.
    fn spawn_read_loop(
        self: &Arc<Self>,
        scope: Scope,
        key: SessionKey,
        readable: ReadableStream,
        generation: u64,
    ) {
        let sessions = self.clone();
        spawn_local(async move {
            let peer = key.peer;
            let session = key.session;
            let from_opener = matches!(key.initiator, Initiator::Local);
            let reader: ReadableStreamDefaultReader = match readable.get_reader().dyn_into() {
                Ok(reader) => reader,
                Err(_) => return,
            };
            loop {
                let result = match JsFuture::from(reader.read()).await {
                    Ok(result) => result,
                    Err(_) => break,
                };
                let done = Reflect::get(&result, &JsValue::from_str("done"))
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                if done {
                    break;
                }
                let value = match Reflect::get(&result, &JsValue::from_str("value")) {
                    Ok(value) => value,
                    Err(_) => break,
                };
                let bytes = Bytes::from(Uint8Array::new(&value).to_vec());
                if send_frame(&scope, peer, Frame::Data {
                    session,
                    from_opener,
                    bytes,
                })
                .await
                .is_err()
                {
                    break;
                }
            }
            // Generation-checked teardown: only Close the peer if we were still the current
            // owner, so a stale read loop never tears down a reopened session.
            sessions
                .close_current_and_notify(&scope, &key, generation)
                .await;
        });
    }
}

/// Open a WebTransport and return its (transport, readable, writer) for the kind.
async fn open(
    url: &str,
    kind: TransportKind,
) -> std::result::Result<(WebTransport, ReadableStream, WritableStreamDefaultWriter), JsValue> {
    let transport = WebTransport::new(url)?;
    JsFuture::from(transport.ready()).await?;

    let (readable, writable): (ReadableStream, WritableStream) = match kind {
        TransportKind::Tcp => {
            let bidi = JsFuture::from(transport.create_bidirectional_stream()).await?;
            let bidi: web_sys::WebTransportBidirectionalStream = bidi.unchecked_into();
            (
                bidi.readable().unchecked_into(),
                bidi.writable().unchecked_into(),
            )
        }
        TransportKind::Udp => {
            let datagrams = transport.datagrams();
            (datagrams.readable(), datagrams.writable())
        }
    };
    let writer = writable.get_writer()?;
    Ok((transport, readable, writer))
}

/// Send a [`Frame`] to `peer` over the overlay, under the scope's own namespace.
async fn send_frame(scope: &Scope, peer: Did, frame: Frame) -> Result<()> {
    let payload = bincode::serialize(&frame).map_err(|_| Error::EncodeError)?;
    scope.send(peer, Bytes::from(payload)).await
}

/// Feed a teardown back to the pure relay so it removes the session from `State.sessions`.
async fn inject_untrack(scope: &Scope, key: &SessionKey) {
    let command = RelayCommand::<String>::Untrack {
        peer: key.peer,
        session: key.session,
        initiator: key.initiator,
    };
    if let Ok(bytes) = bincode::serialize(&command) {
        if let Err(e) = scope.inject(Bytes::from(bytes)).await {
            tracing::warn!(
                "relay Untrack inject failed for {key:?}: {e:?}; pure state may still list \
                 this (now dropped) session"
            );
        }
    }
}
