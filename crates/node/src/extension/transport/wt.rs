#![warn(missing_docs)]
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
//! `JsFuture`, and the session table is a plain `Mutex` (no contention). **Compile-
//! checked only — not runtime-tested.** Requires `--cfg=web_sys_unstable_apis`.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

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
use crate::extension::transport::Frame;
use crate::extension::transport::Initiator;
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
        queue: Vec<Outbound>,
        generation: u64,
    },
    /// Handshake done: the live peer→local writer and the session's WebTransport.
    Ready {
        writer: WritableStreamDefaultWriter,
        transport: WebTransport,
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

    /// Open a WebTransport session to `url` for the session identified by `key`. The slot is
    /// registered as `Opening` *before* the handshake, so peer frames arriving during it are
    /// queued (`Write`/`Shutdown`) or abort it (`Close`). On open failure — or if a peer `Close`
    /// superseded the slot mid-handshake — the just-opened transport is discarded; a `Frame::Close`
    /// is sent only when we are still the slot's owner.
    pub async fn connect(
        self: Arc<Self>,
        scope: Scope,
        key: SessionKey,
        url: String,
        kind: TransportKind,
    ) {
        debug_assert_eq!(
            scope.namespace(),
            key.namespace.as_str(),
            "relay engine acted with a scope outside the session's namespace"
        );
        let generation = self.open_slot(key.clone());
        match open(url.as_str(), kind).await {
            Ok((transport, readable, writer)) => {
                // Promote to Ready iff still current; a peer Close during the handshake removed
                // the slot, so a stale open must discard its transport and stay silent.
                if self.promote(&key, generation, writer, transport.clone()) {
                    self.spawn_read_loop(scope, key, readable, generation);
                } else {
                    transport.close();
                }
            }
            Err(e) => {
                tracing::error!("WebTransport connect to {url} failed: {e:?}");
                // Drop the opening slot and tell the peer — but only if we are still its owner
                // (a peer Close during the handshake already tore it down and told the peer).
                if self.close_if_current(&scope, &key, generation).await {
                    let _ = send_frame(&scope, key.peer, Frame::Close {
                        session: key.session,
                        from_opener: matches!(key.initiator, Initiator::Local),
                    })
                    .await;
                }
            }
        }
    }

    /// Deliver peer bytes to a session's local stream. Queued if the session is still opening,
    /// dropped if unknown — a non-owner peer's key never resolves, so it cannot write to a
    /// session it does not own.
    pub async fn write(&self, key: &SessionKey, bytes: Bytes) {
        let Some(writer) = self.ready_writer_or_queue(key, Outbound::Data(bytes.clone())) else {
            return;
        };
        let chunk = Uint8Array::from(bytes.as_ref());
        let _ = JsFuture::from(writer.write_with_chunk(chunk.as_ref())).await;
    }

    /// Half-close a session's send side (peer sent FIN). Queued if still opening.
    pub async fn shutdown(&self, key: &SessionKey) {
        if let Some(writer) = self.ready_writer_or_queue(key, Outbound::Shutdown) {
            let _ = JsFuture::from(writer.close()).await;
        }
    }

    /// Close and drop the **current** session for `key` (peer `Close` path: the reducer
    /// already removed it). Injects `Untrack` exactly once — only on actual removal.
    pub async fn close(&self, scope: &Scope, key: &SessionKey) {
        let removed = self.map.lock().ok().and_then(|mut map| map.remove(key));
        self.finish_close(scope, key, removed).await;
    }

    /// Close a session **only if** its handle still has `generation` (ABA safety). Returns
    /// whether it removed it; a stale read loop gets `false` and must not peer-`Close` either.
    async fn close_if_current(&self, scope: &Scope, key: &SessionKey, generation: u64) -> bool {
        let removed = self.map.lock().ok().and_then(|mut map| {
            let current = map.get(key).map(|handle| handle.generation());
            (current == Some(generation))
                .then(|| map.remove(key))
                .flatten()
        });
        self.finish_close(scope, key, removed).await
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

    /// Register a fresh `Opening` slot for `key` before the handshake, returning its
    /// generation. The mirror of native's pre-dial `register`.
    fn open_slot(&self, key: SessionKey) -> u64 {
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        self.insert(key, SessionHandle::Opening {
            queue: Vec::new(),
            generation,
        });
        generation
    }

    /// Promote the `Opening` slot for `key` to `Ready` with the just-opened `writer`/
    /// `transport`, flushing peer ops queued during the handshake in arrival order. Returns
    /// `false` (caller discards `transport`) if the slot is gone or its generation was
    /// superseded — a peer `Close` or a newer open during the handshake. Performs no `await`,
    /// so the take-drain-install sequence is atomic against inbound dispatch.
    fn promote(
        &self,
        key: &SessionKey,
        generation: u64,
        writer: WritableStreamDefaultWriter,
        transport: WebTransport,
    ) -> bool {
        let Ok(mut map) = self.map.lock() else {
            return false;
        };
        let queue = match map.get(key) {
            Some(SessionHandle::Opening {
                generation: current,
                ..
            }) if *current == generation => match map.remove(key) {
                Some(SessionHandle::Opening { queue, .. }) => queue,
                _ => return false,
            },
            _ => return false,
        };
        // `write_with_chunk`/`close` enqueue onto the stream in call order (the returned
        // backpressure promise is intentionally dropped); doing it before inserting `Ready`
        // keeps the queued ops ahead of any later write.
        for op in queue {
            match op {
                Outbound::Data(bytes) => {
                    let chunk = Uint8Array::from(bytes.as_ref());
                    let _ = writer.write_with_chunk(chunk.as_ref());
                }
                Outbound::Shutdown => {
                    let _ = writer.close();
                }
            }
        }
        map.insert(key.clone(), SessionHandle::Ready {
            writer,
            transport,
            generation,
        });
        true
    }

    /// If the session is `Ready`, return its writer (the caller applies the op). If it is still
    /// `Opening`, push `op` onto its queue and return `None`. Unknown session → `None` (dropped).
    fn ready_writer_or_queue(
        &self,
        key: &SessionKey,
        op: Outbound,
    ) -> Option<WritableStreamDefaultWriter> {
        let mut map = self.map.lock().ok()?;
        match map.get_mut(key)? {
            SessionHandle::Ready { writer, .. } => Some(writer.clone()),
            SessionHandle::Opening { queue, .. } => {
                queue.push(op);
                None
            }
        }
    }

    fn insert(&self, key: SessionKey, handle: SessionHandle) {
        if let Ok(mut map) = self.map.lock() {
            // Defensive: if a session already exists for this key (a duplicate Open that
            // slipped past the pure reject, or a key reuse), close the old WebTransport
            // before replacing it, so it cannot keep running or later tear down the new one.
            // An `Opening` slot owns no transport yet — its in-flight open will fail to promote.
            if let Some(SessionHandle::Ready { transport, .. }) = map.insert(key, handle) {
                transport.close();
            }
        }
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
            if sessions.close_if_current(&scope, &key, generation).await {
                let _ = send_frame(&scope, peer, Frame::Close {
                    session,
                    from_opener,
                })
                .await;
            }
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
