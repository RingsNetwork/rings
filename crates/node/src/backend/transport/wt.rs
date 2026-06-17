#![warn(missing_docs)]
//! Browser transport-relay engine — the WebTransport endpoint.
//!
//! Browsers have no raw sockets, so the relay's local backend here is a *WebTransport*
//! server (a URL). This is the browser counterpart of the native socket engine
//! ([`engine`](crate::backend::transport::engine)); it presents the same
//! `write`/`shutdown`/`close` surface so the interpreter dispatches uniformly, and
//! opens sessions via [`Effect::WtConnect`](crate::backend::ext::Effect::WtConnect).
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

use crate::backend::ext::Envelope;
use crate::backend::transport::Frame;
use crate::backend::transport::SessionId;
use crate::backend::transport::TransportKind;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

/// A live WebTransport-backed session.
struct SessionHandle {
    /// Peer→local writer.
    writer: WritableStreamDefaultWriter,
    /// The session's WebTransport (closed on teardown).
    transport: WebTransport,
}

/// Browser relay engine: WebTransport sessions keyed by [`SessionId`].
#[derive(Default)]
pub struct WtSessions {
    map: Mutex<HashMap<SessionId, SessionHandle>>,
}

impl WtSessions {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a WebTransport session to `url` and relay it to `peer` under `namespace`.
    /// On any failure a `Frame::Close` is sent.
    pub async fn connect(
        self: Arc<Self>,
        processor: Arc<Processor>,
        session: SessionId,
        peer: Did,
        namespace: String,
        url: String,
        kind: TransportKind,
    ) {
        match open(url.as_str(), kind).await {
            Ok((transport, readable, writer)) => {
                self.insert(session, SessionHandle { writer, transport });
                self.spawn_read_loop(processor, session, peer, namespace, readable);
            }
            Err(e) => {
                tracing::error!("WebTransport connect to {url} failed: {e:?}");
                let _ = send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Close {
                    session,
                })
                .await;
            }
        }
    }

    /// Deliver peer bytes to a session's local stream. Unknown sessions are dropped.
    pub async fn write(&self, session: SessionId, bytes: Bytes) {
        let Some(writer) = self.writer(session) else {
            return;
        };
        let chunk = Uint8Array::from(bytes.as_ref());
        let _ = JsFuture::from(writer.write_with_chunk(chunk.as_ref())).await;
    }

    /// Half-close a session's send side (peer sent FIN).
    pub async fn shutdown(&self, session: SessionId) {
        if let Some(writer) = self.writer(session) {
            let _ = JsFuture::from(writer.close()).await;
        }
    }

    /// Close and drop a session (closes the WebTransport).
    pub fn close(&self, session: SessionId) {
        if let Ok(mut map) = self.map.lock() {
            if let Some(handle) = map.remove(&session) {
                handle.transport.close();
            }
        }
    }

    fn writer(&self, session: SessionId) -> Option<WritableStreamDefaultWriter> {
        self.map
            .lock()
            .ok()
            .and_then(|map| map.get(&session).map(|handle| handle.writer.clone()))
    }

    fn insert(&self, session: SessionId, handle: SessionHandle) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(session, handle);
        }
    }

    /// Spawn the local→peer read loop for `readable`.
    fn spawn_read_loop(
        self: &Arc<Self>,
        processor: Arc<Processor>,
        session: SessionId,
        peer: Did,
        namespace: String,
        readable: ReadableStream,
    ) {
        let sessions = self.clone();
        spawn_local(async move {
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
                if send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Data {
                    session,
                    bytes,
                })
                .await
                .is_err()
                {
                    break;
                }
            }
            sessions.close(session);
            let _ = send_frame(processor.as_ref(), peer, namespace.as_str(), Frame::Close {
                session,
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
async fn send_frame(processor: &Processor, peer: Did, namespace: &str, frame: Frame) -> Result<()> {
    let payload = bincode::serialize(&frame).map_err(|_| Error::EncodeError)?;
    let envelope = Envelope::new(namespace.to_string(), Bytes::from(payload));
    processor.send_envelope(peer, &envelope).await?;
    Ok(())
}
