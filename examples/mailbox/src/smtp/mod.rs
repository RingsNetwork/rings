//! The SMTP shell: the receiver for the mailbox's own address, submission for its user, and the
//! mail gateway's receiver.
//!
//! All run the same [`Session`] over a socket and differ in their [`Role`] and in where a
//! committed transaction goes:
//!
//! - the receiver accepts mail for `me@<did>.<zone>` and files it in the INBOX, with a
//!   `Return-Path` and a `Received` trace header naming the client (RFC 5321 §4.4); behind a
//!   relay port served with `proxy_protocol: v2`, it reads the client from the PROXY header
//!   ([`proxy`]) first, as the TCP peer is then its own node;
//! - submission accepts mail for anyone and hands it to the outbound queue, which delivers it
//!   to the recipients' mail exchangers;
//! - the gateway ([`crate::gateway`]) looks every recipient up in the DHT and writes the mail,
//!   traced as the receiver traces it, to each recipient's mail topic.
//!
//! A reply to a transaction is sent only after the transaction is committed, so a `250`
//! means the message is on disk, or, from the gateway, that its write has left the node. A
//! client silent for [`SESSION_TIMEOUT`] is told `421` and disconnected (RFC 5321 §4.5.3.2.7).
//! A listener with a certificate offers `STARTTLS` (RFC 3207); every byte the client sent after
//! the command, before the handshake, is discarded (§5), and the session starts afresh under TLS.

pub mod proxy;
mod session;
pub mod tls;

use std::net::SocketAddr;
use std::slice::Iter;
use std::sync::Arc;
use std::time::Duration;

pub use session::Lookup;
pub use session::Next;
pub use session::Reply;
pub use session::Role;
pub use session::Session;
pub use session::Transaction;
use smtp_proto::request::receiver::DataReceiver;
use smtp_proto::request::receiver::RequestReceiver;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use crate::gateway::Gateway;
use crate::gateway::GatewayError;
use crate::outbound::Outbound;
use crate::outbound::OutboundError;
use crate::serve::serve_each;
use crate::store::Store;
use crate::store::StoreError;
use crate::store::INBOX;

/// Most recipients one transaction may name.
pub const MAX_RECIPIENTS: usize = 100;
/// How long the server waits for the client's next bytes, command or message.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Size of one socket read.
const READ_CHUNK: usize = 16 * 1024;

/// Why a connection ended abnormally.
#[derive(Debug, Error)]
pub enum ConnectionError {
    /// The socket, or the TLS handshake, failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The PROXY header the port expects did not come.
    #[error(transparent)]
    Proxy(#[from] proxy::ProxyError),
}

/// Why a transaction could not be committed.
#[derive(Debug, Error)]
pub enum CommitError {
    /// The store did not file it.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The outbound queue did not take it.
    #[error(transparent)]
    Outbound(#[from] OutboundError),
    /// The gateway did not write it.
    #[error(transparent)]
    Gateway(#[from] GatewayError),
}

impl CommitError {
    /// The reply to a transaction that failed so: `552` for a message the gateway cannot carry,
    /// `451` for every other failure, which a later attempt may not meet.
    fn reply(&self) -> Reply {
        match self {
            Self::Gateway(error) if error.is_too_large() => Reply {
                code: 552,
                status: "5.3.4",
                lines: vec!["Message too big for a mail gateway".to_owned()],
            },
            Self::Store(_) | Self::Outbound(_) | Self::Gateway(_) => Reply {
                code: 451,
                status: "4.3.0",
                lines: vec!["Local error, try again later".to_owned()],
            },
        }
    }
}

/// Where committed transactions go.
#[derive(Clone)]
pub enum Sink {
    /// Filed in the store's INBOX.
    Inbox(Arc<Store>),
    /// Queued for delivery onward.
    Outbound(Arc<Outbound>),
    /// Written to each recipient's mail topic in the DHT.
    Gateway(Arc<Gateway>),
}

impl Sink {
    /// What this sink's directory says of `address`, which a [`Role::Directory`] session asked
    /// about. Only the gateway serves that role; the inbox and the outbound queue serve roles
    /// that decide in the session, and refuse what they would be asked.
    async fn lookup(&self, address: &str) -> Lookup {
        match self {
            Self::Gateway(gateway) => gateway.lookup(address).await,
            Self::Inbox(_) | Self::Outbound(_) => Lookup::Refused,
        }
    }

    /// Commit `transaction` from `client`, received by `host`; return the reply to send.
    async fn commit(
        &self,
        host: &str,
        client: SocketAddr,
        transaction: Transaction,
        ok: Reply,
    ) -> Reply {
        match self.file(host, client, transaction).await {
            Ok(()) => ok,
            Err(error) => {
                tracing::warn!("smtp: transaction not committed: {error}");
                error.reply()
            }
        }
    }

    /// File `transaction` where this sink keeps it.
    async fn file(
        &self,
        host: &str,
        client: SocketAddr,
        transaction: Transaction,
    ) -> Result<(), CommitError> {
        match self {
            Self::Inbox(store) => {
                let message = with_trace(host, client, &transaction);
                let now = chrono::Utc::now().timestamp();
                store
                    .append(INBOX, &message, Default::default(), now)
                    .await?;
            }
            Self::Outbound(outbound) => {
                let Transaction {
                    from, to, message, ..
                } = &transaction;
                outbound.enqueue(from, to, message).await?;
            }
            Self::Gateway(gateway) => {
                let message = with_trace(host, client, &transaction);
                gateway.deliver(&transaction.to, &message).await?;
            }
        }
        Ok(())
    }
}

/// `client` as a `Received` header's TCP-info names it (RFC 5321 §4.1.3 address literal).
pub fn address_literal(client: SocketAddr) -> String {
    match client {
        SocketAddr::V4(client) => format!("[{}]", client.ip()),
        SocketAddr::V6(client) => format!("[IPv6:{}]", client.ip()),
    }
}

/// `transaction`'s message with its trace headers prepended: the reverse path, and the hop
/// from `client` to `host`.
pub fn with_trace(host: &str, client: SocketAddr, transaction: &Transaction) -> Vec<u8> {
    let date = chrono::Utc::now().to_rfc2822();
    let mut message = format!(
        "Return-Path: <{}>\r\nReceived: from {} ({}) by {host} with ESMTP; {date}\r\n",
        transaction.from,
        transaction.client,
        address_literal(client)
    )
    .into_bytes();
    message.extend_from_slice(&transaction.message);
    message
}

/// One SMTP listener: its role, its name, its size bound, where it commits, whether its
/// clients' connections start with a PROXY header, and the certificate it offers `STARTTLS`
/// with, if any.
#[derive(Clone)]
pub struct SmtpService {
    /// The role every session plays.
    pub role: Role,
    /// The name the server greets with.
    pub host: String,
    /// The largest message accepted.
    pub max_size: usize,
    /// Where committed transactions go.
    pub sink: Sink,
    /// Whether every connection starts with a PROXY protocol v2 header ([`proxy`]).
    pub proxy_protocol: bool,
    /// The TLS server `STARTTLS` hands a connection to; without one it is not offered.
    pub tls: Option<TlsAcceptor>,
}

/// How a conversation over one stream ended.
enum Ending {
    /// The client quit, fell silent, or closed the connection.
    Closed,
    /// The session answered `STARTTLS`: the stream is to be handed to TLS.
    StartTls,
}

impl SmtpService {
    /// Serve on `listener` forever, one task per connection.
    pub async fn serve(self, listener: TcpListener) {
        let service = Arc::new(self);
        serve_each(listener, "smtp", |stream, peer| {
            let service = Arc::clone(&service);
            async move {
                if let Err(error) = service.connection(stream, peer).await {
                    tracing::debug!("smtp {peer}: {error}");
                }
            }
        })
        .await;
    }

    /// Run one session on `stream`, from `peer`, until the client quits, falls silent, or the
    /// connection ends; under TLS from the point the client asks for it.
    async fn connection(
        &self,
        mut stream: TcpStream,
        peer: SocketAddr,
    ) -> Result<(), ConnectionError> {
        let client = if self.proxy_protocol {
            proxy::read_client(&mut stream).await?.unwrap_or(peer)
        } else {
            peer
        };
        let session = Session::new(self.role.clone(), &self.host, self.max_size, MAX_RECIPIENTS);
        let session = match self.tls {
            Some(_) => session.offering_starttls(),
            None => session,
        };
        stream.write_all(&session.greeting().encode()).await?;
        let mut conversation = Conversation::new(session, client);
        match (
            self.converse(&mut stream, &mut conversation).await?,
            &self.tls,
        ) {
            (Ending::StartTls, Some(acceptor)) => {
                let mut stream = acceptor.accept(stream).await?;
                conversation.secured();
                self.converse(&mut stream, &mut conversation).await?;
                Ok(())
            }
            (Ending::StartTls | Ending::Closed, _) => Ok(()),
        }
    }

    /// Converse over `stream` until the conversation ends there.
    async fn converse<S>(
        &self,
        stream: &mut S,
        conversation: &mut Conversation,
    ) -> Result<Ending, ConnectionError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            let Ok(read) = timeout(SESSION_TIMEOUT, stream.read(&mut chunk)).await else {
                let closing = conversation.session.timed_out().encode();
                stream.write_all(&closing).await?;
                stream.shutdown().await?;
                return Ok(Ending::Closed);
            };
            let read = read?;
            if read == 0 {
                return Ok(Ending::Closed);
            }
            let mut bytes = chunk.get(..read).unwrap_or_default().iter();
            match conversation.ingest(self, &mut bytes, stream).await? {
                Turn::Continue => {}
                Turn::Close => {
                    stream.shutdown().await?;
                    return Ok(Ending::Closed);
                }
                Turn::StartTls => return Ok(Ending::StartTls),
            }
        }
    }
}

/// A message body being read: its receiver, what it holds so far, and whether it grew past the
/// session's bound, in which case it holds nothing.
struct Body {
    receiver: DataReceiver,
    bytes: Vec<u8>,
    oversize: bool,
}

/// What a conversation does after taking the bytes of one read.
enum Turn {
    /// Read on.
    Continue,
    /// The session is over.
    Close,
    /// Hand the stream to TLS; the rest of the read is discarded.
    StartTls,
}

/// One connection's session and the parsers it reads with.
struct Conversation {
    session: Session,
    client: SocketAddr,
    requests: RequestReceiver,
    body: Option<Body>,
}

impl Conversation {
    /// The conversation of `session` with `client`, before any byte is read.
    fn new(session: Session, client: SocketAddr) -> Self {
        Self {
            session,
            client,
            requests: RequestReceiver::default(),
            body: None,
        }
    }

    /// The TLS handshake completed: the session starts afresh, and so do the parsers, which
    /// hold nothing read before it.
    fn secured(&mut self) {
        self.session.secured();
        self.requests = RequestReceiver::default();
        self.body = None;
    }

    /// Take `bytes`, answering on `stream` every command and body they complete; return what
    /// the conversation does next.
    async fn ingest<S>(
        &mut self,
        service: &SmtpService,
        bytes: &mut Iter<'_, u8>,
        stream: &mut S,
    ) -> std::io::Result<Turn>
    where
        S: AsyncWrite + Unpin,
    {
        while !bytes.as_slice().is_empty() {
            if let Some(body) = self.body.as_mut() {
                let complete = body.receiver.ingest(bytes, &mut body.bytes);
                if body.bytes.len() > self.session.max_size() {
                    // Keep reading to the end of the body, but keep none of it.
                    body.oversize = true;
                    body.bytes.clear();
                }
                if let Some(body) = complete.then(|| self.body.take()).flatten() {
                    let reply = self.end_of_body(service, body).await;
                    stream.write_all(&reply.encode()).await?;
                }
                continue;
            }
            let next = match self.requests.ingest(bytes) {
                Ok(request) => self.session.step(request.into_owned()),
                Err(smtp_proto::Error::NeedsMoreData { .. }) => break,
                Err(_) => self.session.unparsable(),
            };
            match next {
                Next::Reply(reply) => stream.write_all(&reply.encode()).await?,
                Next::ReadData(reply) => {
                    stream.write_all(&reply.encode()).await?;
                    self.body = Some(Body {
                        receiver: DataReceiver::new(),
                        bytes: Vec::new(),
                        oversize: false,
                    });
                }
                Next::Close(reply) => {
                    stream.write_all(&reply.encode()).await?;
                    return Ok(Turn::Close);
                }
                Next::Lookup(address) => {
                    let lookup = service.sink.lookup(&address).await;
                    let reply = self.session.admit(address, lookup);
                    stream.write_all(&reply.encode()).await?;
                }
                Next::StartTls(reply) => {
                    stream.write_all(&reply.encode()).await?;
                    return Ok(Turn::StartTls);
                }
            }
        }
        Ok(Turn::Continue)
    }

    /// The reply to a body read to its end: the commit's, or a refusal.
    async fn end_of_body(&mut self, service: &SmtpService, body: Body) -> Reply {
        if body.oversize {
            return self.session.data_too_large();
        }
        match self.session.data(body.bytes) {
            (Some(transaction), ok) => {
                let sink = &service.sink;
                sink.commit(&service.host, self.client, transaction, ok)
                    .await
            }
            (None, refused) => refused,
        }
    }
}

#[cfg(test)]
mod test_smtp;
