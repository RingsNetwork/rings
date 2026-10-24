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
//!   no client event for AUTOLOGOUT ─▶ BYE
//! ```
//!
//! The connection closes once the last response of a session that said BYE has been sent:
//! after LOGOUT, after its selected mailbox was deleted, or after the autologout timer
//! (RFC 3501 §5.4) ran out.

pub mod mime;
pub mod search;
pub mod session;
pub mod view;

use std::sync::Arc;
use std::time::Duration;

use imap_next::imap_types::auth::AuthMechanism;
use imap_next::imap_types::auth::AuthenticateData;
use imap_next::imap_types::core::Tag;
use imap_next::imap_types::response::Code;
use imap_next::imap_types::response::Greeting;
use imap_next::imap_types::response::GreetingKind;
use imap_next::imap_types::response::Response;
use imap_next::imap_types::response::StatusKind;
use imap_next::server::Event;
use imap_next::server::Options;
use imap_next::server::ResponseHandle;
use imap_next::server::Server;
use imap_next::stream::Stream;
use session::bye;
use session::continuation;
use session::text;
pub use session::Credentials;
use session::Session;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::serve::serve_each;
use crate::store::Store;

/// How long a client may send nothing before the server logs it out: the least RFC 3501 §5.4
/// allows, which an IDLE client renews by reissuing IDLE.
pub const AUTOLOGOUT: Duration = Duration::from_secs(30 * 60);

/// Why a connection ended abnormally.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct ConnectionError(#[from] imap_next::stream::Error<imap_next::server::Error>);

/// The IMAP service: the store it serves and the credentials it admits.
#[derive(Clone)]
pub struct ImapService {
    /// The store.
    pub store: Arc<Store>,
    /// The credentials.
    pub credentials: Credentials,
}

impl ImapService {
    /// Serve on `listener` forever, one task per connection.
    pub async fn serve(self, listener: TcpListener) {
        let service = Arc::new(self);
        serve_each(listener, "imap", |stream, peer| {
            let service = Arc::clone(&service);
            async move {
                if let Err(error) = service.connection(stream).await {
                    tracing::debug!("imap {peer}: {error}");
                }
            }
        })
        .await;
    }

    /// Run one session on `stream` until it says BYE or the connection ends.
    async fn connection(&self, stream: TcpStream) -> Result<(), ConnectionError> {
        let mut stream = Stream::insecure(stream);
        let mut connection = Connection::new(self);
        let mut changes = self.store.watch();
        let autologout = tokio::time::sleep(AUTOLOGOUT);
        tokio::pin!(autologout);
        loop {
            tokio::select! {
                event = stream.next(&mut connection.server) => {
                    let event = match event {
                        Ok(event) => event,
                        Err(imap_next::stream::Error::State(error)) => {
                            // A malformed command: say so, and read the next one.
                            let bad = session::status(None, StatusKind::Bad, None, &error.to_string());
                            connection.server.enqueue_status(bad);
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    };
                    if !matches!(event, Event::GreetingSent { .. } | Event::ResponseSent { .. }) {
                        autologout.as_mut().reset(Instant::now() + AUTOLOGOUT);
                    }
                    if connection.on_event(event).await {
                        return Ok(());
                    }
                }
                change = changes.recv(), if connection.idling.is_some() => match change {
                    Ok(_) | Err(RecvError::Lagged(_)) => connection.sync().await,
                    Err(RecvError::Closed) => return Ok(()),
                },
                () = &mut autologout, if connection.closing.is_none() => {
                    let handle = connection.server.enqueue_status(bye("autologout"));
                    connection.closing = Some(handle);
                }
            }
        }
    }
}

/// One connection's state beside its session: the protocol flow it is in, and the response
/// after which it closes.
struct Connection {
    server: Server,
    session: Session,
    idling: Option<Tag<'static>>,
    authenticating: Option<Tag<'static>>,
    closing: Option<ResponseHandle>,
}

impl Connection {
    /// A fresh connection of `service`, its greeting queued.
    fn new(service: &ImapService) -> Self {
        let greeting = Greeting {
            kind: GreetingKind::Ok,
            code: Some(Code::Capability(session::capabilities())),
            text: text("rings-mailbox ready"),
        };
        let session = Session::new(Arc::clone(&service.store), service.credentials.clone());
        Self {
            server: Server::new(Options::default(), greeting),
            session,
            idling: None,
            authenticating: None,
            closing: None,
        }
    }

    /// Answer `event`; return whether the connection is done.
    async fn on_event(&mut self, event: Event) -> bool {
        match event {
            Event::GreetingSent { .. } => {}
            Event::ResponseSent { handle, .. } => return self.closing == Some(handle),
            Event::CommandReceived { command } => {
                let responses = self.session.handle(command).await;
                self.send(responses);
            }
            Event::CommandAuthenticateReceived {
                command_authenticate,
            } => {
                let tag = command_authenticate.tag;
                if command_authenticate.mechanism != AuthMechanism::Plain {
                    let refused = session::status(Some(&tag), StatusKind::No, None, "only PLAIN");
                    let _ = self.server.authenticate_finish(refused);
                } else if let Some(initial) = command_authenticate.initial_response {
                    let status = self.plain(&tag, initial.declassify());
                    let _ = self.server.authenticate_finish(status);
                } else {
                    let _ = self.server.authenticate_continue(continuation(""));
                    self.authenticating = Some(tag);
                }
            }
            Event::AuthenticateDataReceived { authenticate_data } => {
                let Some(tag) = self.authenticating.take() else {
                    return false;
                };
                let status = match authenticate_data {
                    AuthenticateData::Continue(data) => self.plain(&tag, data.declassify()),
                    AuthenticateData::Cancel => {
                        session::status(Some(&tag), StatusKind::Bad, None, "cancelled")
                    }
                };
                let _ = self.server.authenticate_finish(status);
            }
            Event::IdleCommandReceived { tag } => self.idle(tag).await,
            Event::IdleDoneReceived => {
                if let Some(tag) = self.idling.take() {
                    let done = session::status(Some(&tag), StatusKind::Ok, None, "IDLE terminated");
                    self.server.enqueue_status(done);
                }
            }
        }
        false
    }

    /// Accept IDLE under `tag` when a mailbox is selected, and bring the client up to date.
    async fn idle(&mut self, tag: Tag<'static>) {
        if !self.session.is_selected() {
            let refused =
                session::status(Some(&tag), StatusKind::Bad, None, "select a mailbox first");
            let _ = self.server.idle_reject(refused);
            return;
        }
        let _ = self.server.idle_accept(continuation("idling"));
        self.idling = Some(tag);
        self.sync().await;
    }

    /// Tell an idling client what changed in the store.
    async fn sync(&mut self) {
        let responses = self.session.sync(true).await;
        self.send(responses);
    }

    /// Queue `responses`; when the session said BYE, close after the last of them.
    fn send(&mut self, responses: Vec<Response<'static>>) {
        let mut last = None;
        for response in responses {
            last = Some(match response {
                Response::Data(data) => self.server.enqueue_data(data),
                Response::Status(status) => self.server.enqueue_status(status),
                Response::CommandContinuationRequest(request) => {
                    self.server.enqueue_continuation_request(request)
                }
            });
        }
        if self.session.is_logged_out() && self.closing.is_none() {
            self.closing = last;
        }
    }

    /// The tagged status of a PLAIN authentication with `message` under `tag`.
    fn plain(
        &mut self,
        tag: &Tag<'static>,
        message: &[u8],
    ) -> imap_next::imap_types::response::Status<'static> {
        if self.session.authenticate_plain(message) {
            let code = Some(Code::Capability(session::capabilities()));
            session::status(Some(tag), StatusKind::Ok, code, "authenticated")
        } else {
            session::status(Some(tag), StatusKind::No, None, "invalid credentials")
        }
    }
}
