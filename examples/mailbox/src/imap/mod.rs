//! The IMAP shell: IMAP4rev1 over a socket, one [`Session`] per connection.
//!
//! `imap-next` owns the protocol's framing (literals, continuations, IDLE and AUTHENTICATE
//! flows); this module feeds its events to the session and the session's responses back:
//!
//! ```text
//!   CommandReceived            ─▶ session.handle ─▶ responses
//!   AUTHENTICATE PLAIN         ─▶ (initial response | "+" ─▶ data) ─▶ session.authenticate_plain
//!   IDLE                       ─▶ "+ idling"; every store change ─▶ session.sync ─▶ untagged data
//!   DONE                       ─▶ tagged OK
//! ```
//!
//! The connection closes after the tagged response to LOGOUT has been sent.

pub mod mime;
pub mod search;
pub mod session;
pub mod view;

use std::sync::Arc;

use imap_next::imap_types::auth::AuthMechanism;
use imap_next::imap_types::auth::AuthenticateData;
use imap_next::imap_types::response::Code;
use imap_next::imap_types::response::CommandContinuationRequest;
use imap_next::imap_types::response::Greeting;
use imap_next::imap_types::response::Response;
use imap_next::imap_types::response::Status;
use imap_next::server::Event;
use imap_next::server::Options;
use imap_next::server::ResponseHandle;
use imap_next::server::Server;
use imap_next::stream::Stream;
use session::text;
pub use session::Credentials;
use session::Session;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;

use crate::store::Store;

/// The IMAP service: the store it serves and the credentials it admits.
#[derive(Clone)]
pub struct ImapService {
    /// The store.
    pub store: Arc<Store>,
    /// The credentials.
    pub credentials: Credentials,
}

/// Queue `response` on `server`; return its handle.
fn enqueue(server: &mut Server, response: Response<'static>) -> ResponseHandle {
    match response {
        Response::Data(data) => server.enqueue_data(data),
        Response::Status(status) => server.enqueue_status(status),
        Response::CommandContinuationRequest(request) => {
            server.enqueue_continuation_request(request)
        }
    }
}

impl ImapService {
    /// Serve on `listener` forever, one task per connection.
    pub async fn serve(self, listener: TcpListener) {
        let service = Arc::new(self);
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let service = Arc::clone(&service);
                    tokio::spawn(async move {
                        if let Err(error) = service.connection(stream).await {
                            tracing::debug!("imap {peer}: {error}");
                        }
                    });
                }
                Err(error) => tracing::warn!("imap: accept failed: {error}"),
            }
        }
    }

    /// Run one session on `stream` until the client logs out or the connection ends.
    async fn connection(&self, stream: TcpStream) -> Result<(), String> {
        let mut stream = Stream::insecure(stream);
        let greeting = Greeting {
            kind: imap_next::imap_types::response::GreetingKind::Ok,
            code: Some(Code::Capability(session::capabilities())),
            text: text("rings-mailbox ready"),
        };
        let mut server = Server::new(Options::default(), greeting);
        let mut session = Session::new(Arc::clone(&self.store), self.credentials.clone());
        let mut changes = self.store.watch();
        let mut idling = None;
        let mut authenticating = None;
        let mut closing = None;
        loop {
            tokio::select! {
                event = stream.next(&mut server) => {
                    let event = match event {
                        Ok(event) => event,
                        Err(imap_next::stream::Error::State(error)) => {
                            // A malformed command: say so, and read the next one.
                            let bad = Status::bad(None, None, text(&error.to_string()))
                                .map_err(|error| error.to_string())?;
                            server.enqueue_status(bad);
                            continue;
                        }
                        Err(error) => return Err(error.to_string()),
                    };
                    match event {
                        Event::GreetingSent { .. } => {}
                        Event::ResponseSent { handle, .. } => {
                            if closing == Some(handle) {
                                return Ok(());
                            }
                        }
                        Event::CommandReceived { command } => {
                            let mut last = None;
                            for response in session.handle(command).await {
                                last = Some(enqueue(&mut server, response));
                            }
                            if session.is_logged_out() {
                                closing = last;
                            }
                        }
                        Event::CommandAuthenticateReceived { command_authenticate } => {
                            let tag = command_authenticate.tag;
                            if command_authenticate.mechanism != AuthMechanism::Plain {
                                let refused = Status::no(Some(tag), None, text("only PLAIN"))
                                    .map_err(|error| error.to_string())?;
                                let _ = server.authenticate_finish(refused);
                            } else if let Some(initial) = command_authenticate.initial_response {
                                let status = self.plain(&mut session, tag, initial.declassify())?;
                                let _ = server.authenticate_finish(status);
                            } else {
                                let request = CommandContinuationRequest::basic(None, text(""))
                                    .map_err(|error| error.to_string())?;
                                let _ = server.authenticate_continue(request);
                                authenticating = Some(tag);
                            }
                        }
                        Event::AuthenticateDataReceived { authenticate_data } => {
                            let Some(tag) = authenticating.take() else {
                                continue;
                            };
                            let status = match authenticate_data {
                                AuthenticateData::Continue(data) => {
                                    self.plain(&mut session, tag, data.declassify())?
                                }
                                AuthenticateData::Cancel => Status::bad(Some(tag), None, text("cancelled"))
                                    .map_err(|error| error.to_string())?,
                            };
                            let _ = server.authenticate_finish(status);
                        }
                        Event::IdleCommandReceived { tag } => {
                            if session.is_selected() {
                                let request = CommandContinuationRequest::basic(None, text("idling"))
                                    .map_err(|error| error.to_string())?;
                                let _ = server.idle_accept(request);
                                for response in session.sync(true).await {
                                    enqueue(&mut server, response);
                                }
                                idling = Some(tag);
                            } else {
                                let refused = Status::bad(Some(tag), None, text("select a mailbox first"))
                                    .map_err(|error| error.to_string())?;
                                let _ = server.idle_reject(refused);
                            }
                        }
                        Event::IdleDoneReceived => {
                            if let Some(tag) = idling.take() {
                                let done = Status::ok(Some(tag), None, text("IDLE terminated"))
                                    .map_err(|error| error.to_string())?;
                                server.enqueue_status(done);
                            }
                        }
                    }
                }
                change = changes.recv(), if idling.is_some() => {
                    match change {
                        Ok(_) | Err(RecvError::Lagged(_)) => {
                            for response in session.sync(true).await {
                                enqueue(&mut server, response);
                            }
                        }
                        Err(RecvError::Closed) => return Ok(()),
                    }
                }
            }
        }
    }

    /// The tagged status of a PLAIN authentication with `message` under `tag`.
    fn plain(
        &self,
        session: &mut Session,
        tag: imap_next::imap_types::core::Tag<'static>,
        message: &[u8],
    ) -> Result<Status<'static>, String> {
        if session.authenticate_plain(message) {
            Status::ok(
                Some(tag),
                Some(Code::Capability(session::capabilities())),
                text("authenticated"),
            )
        } else {
            Status::no(Some(tag), None, text("invalid credentials"))
        }
        .map_err(|error| error.to_string())
    }
}
