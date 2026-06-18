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
use crate::extension::ext::Core;
use crate::extension::protocols::relay::RelayCommand;
use crate::extension::transport::Frame;
use crate::extension::transport::Initiator;
use crate::extension::transport::SessionKey;
use crate::extension::transport::TransportKind;

/// A live WebTransport-backed session.
struct SessionHandle {
    /// Peer→local writer.
    writer: WritableStreamDefaultWriter,
    /// The session's WebTransport (closed on teardown).
    transport: WebTransport,
}

/// Browser relay engine: WebTransport sessions keyed by [`SessionKey`].
///
/// Like the native engine, sessions are keyed by the full `(peer, namespace, session)`
/// rather than the bare opener-assigned id, so a frame from a peer can only ever address
/// its own sessions (owner rejection by keyed-lookup miss).
#[derive(Default)]
pub struct WtSessions {
    map: Mutex<HashMap<SessionKey, SessionHandle>>,
}

impl WtSessions {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a WebTransport session to `url` for the session identified by `key`.
    /// On any failure a `Frame::Close` is sent.
    pub async fn connect(
        self: Arc<Self>,
        core: Core,
        key: SessionKey,
        url: String,
        kind: TransportKind,
    ) {
        match open(url.as_str(), kind).await {
            Ok((transport, readable, writer)) => {
                self.insert(key.clone(), SessionHandle { writer, transport });
                self.spawn_read_loop(core, key, readable);
            }
            Err(e) => {
                tracing::error!("WebTransport connect to {url} failed: {e:?}");
                // Pre-registered nothing here, but tell the pure relay to forget it and the
                // peer that it's closed.
                inject_untrack(&core, &key).await;
                let _ = send_frame(&core, key.peer, key.namespace.as_str(), Frame::Close {
                    session: key.session,
                    from_opener: matches!(key.initiator, Initiator::Local),
                })
                .await;
            }
        }
    }

    /// Deliver peer bytes to a session's local stream. Unknown sessions are dropped — a
    /// non-owner peer's key never resolves, so it cannot write to a session it does not own.
    pub async fn write(&self, key: &SessionKey, bytes: Bytes) {
        let Some(writer) = self.writer(key) else {
            return;
        };
        let chunk = Uint8Array::from(bytes.as_ref());
        let _ = JsFuture::from(writer.write_with_chunk(chunk.as_ref())).await;
    }

    /// Half-close a session's send side (peer sent FIN).
    pub async fn shutdown(&self, key: &SessionKey) {
        if let Some(writer) = self.writer(key) {
            let _ = JsFuture::from(writer.close()).await;
        }
    }

    /// Close and drop a session (closes the WebTransport), then feed the teardown back to
    /// the pure relay as an `Untrack`. Injects exactly once — only on actual removal.
    pub async fn close(&self, core: &Core, key: &SessionKey) {
        let removed = self
            .map
            .lock()
            .ok()
            .and_then(|mut map| map.remove(key))
            .map(|handle| handle.transport.close())
            .is_some();
        if removed {
            inject_untrack(core, key).await;
        }
    }

    fn writer(&self, key: &SessionKey) -> Option<WritableStreamDefaultWriter> {
        self.map
            .lock()
            .ok()
            .and_then(|map| map.get(key).map(|handle| handle.writer.clone()))
    }

    fn insert(&self, key: SessionKey, handle: SessionHandle) {
        if let Ok(mut map) = self.map.lock() {
            // Defensive: if a session already exists for this key (a duplicate Open that
            // slipped past the pure reject, or a key reuse), close the old WebTransport
            // before replacing it, so it cannot keep running or later tear down the new one.
            if let Some(old) = map.insert(key, handle) {
                old.transport.close();
            }
        }
    }

    /// Spawn the local→peer read loop for `readable`.
    fn spawn_read_loop(self: &Arc<Self>, core: Core, key: SessionKey, readable: ReadableStream) {
        let sessions = self.clone();
        spawn_local(async move {
            let peer = key.peer;
            let namespace = key.namespace.clone();
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
                if send_frame(&core, peer, namespace.as_str(), Frame::Data {
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
            sessions.close(&core, &key).await;
            let _ = send_frame(&core, peer, namespace.as_str(), Frame::Close {
                session,
                from_opener,
            })
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

/// Send a [`Frame`] to `peer` under `namespace` over the overlay.
async fn send_frame(core: &Core, peer: Did, namespace: &str, frame: Frame) -> Result<()> {
    let payload = bincode::serialize(&frame).map_err(|_| Error::EncodeError)?;
    core.send(peer, namespace, Bytes::from(payload)).await
}

/// Feed a teardown back to the pure relay so it removes the session from `State.sessions`.
async fn inject_untrack(core: &Core, key: &SessionKey) {
    let command = RelayCommand::<String>::Untrack {
        peer: key.peer,
        session: key.session,
        initiator: key.initiator,
    };
    if let Ok(bytes) = bincode::serialize(&command) {
        let _ = core
            .inject(key.namespace.as_str(), Bytes::from(bytes))
            .await;
    }
}
