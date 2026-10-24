//! The SMTP shell: the receiver for the mailbox's own address, and submission for its user.
//!
//! Both run the same [`Session`] over a socket and differ in their [`Role`] and in where a
//! committed transaction goes:
//!
//! - the receiver accepts mail for `me@<did>.<zone>` and files it in the INBOX, with a
//!   `Return-Path` and a `Received` trace header naming the client (RFC 5321 §4.4); behind a
//!   relay port served with `proxy_protocol: v2`, it reads the client from the PROXY header
//!   ([`proxy`]) first, as the TCP peer is then its own node;
//! - submission accepts mail for anyone and hands it to the outbound queue, which delivers it
//!   to the recipients' mail exchangers.
//!
//! A reply to a transaction is sent only after the transaction is committed, so a `250`
//! means the message is on disk. A client silent for [`SESSION_TIMEOUT`] is told `421` and
//! disconnected (RFC 5321 §4.5.3.2.7).

pub mod proxy;
mod session;

use std::net::SocketAddr;
use std::slice::Iter;
use std::sync::Arc;
use std::time::Duration;

pub use session::Next;
pub use session::Reply;
pub use session::Role;
pub use session::Session;
pub use session::Transaction;
use smtp_proto::request::receiver::DataReceiver;
use smtp_proto::request::receiver::RequestReceiver;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::timeout;

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
    /// The socket failed.
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
}

/// Where committed transactions go.
#[derive(Clone)]
pub enum Sink {
    /// Filed in the store's INBOX.
    Inbox(Arc<Store>),
    /// Queued for delivery onward.
    Outbound(Arc<Outbound>),
}

impl Sink {
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
                Reply {
                    code: 451,
                    status: "4.3.0",
                    lines: vec!["Local error, try again later".to_owned()],
                }
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

/// One SMTP listener: its role, its name, its size bound, where it commits, and whether its
/// clients' connections start with a PROXY header.
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
    /// connection ends.
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
        stream.write_all(&session.greeting().encode()).await?;
        let mut conversation = Conversation {
            session,
            client,
            requests: RequestReceiver::default(),
            body: None,
        };
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            let Ok(read) = timeout(SESSION_TIMEOUT, stream.read(&mut chunk)).await else {
                let closing = conversation.session.timed_out().encode();
                stream.write_all(&closing).await?;
                stream.shutdown().await?;
                return Ok(());
            };
            let read = read?;
            if read == 0 {
                return Ok(());
            }
            let mut bytes = chunk.get(..read).unwrap_or_default().iter();
            if conversation.ingest(self, &mut bytes, &mut stream).await? {
                stream.shutdown().await?;
                return Ok(());
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

/// One connection's session and the parsers it reads with.
struct Conversation {
    session: Session,
    client: SocketAddr,
    requests: RequestReceiver,
    body: Option<Body>,
}

impl Conversation {
    /// Take `bytes`, answering on `stream` every command and body they complete; return whether
    /// the session is over.
    async fn ingest(
        &mut self,
        service: &SmtpService,
        bytes: &mut Iter<'_, u8>,
        stream: &mut TcpStream,
    ) -> std::io::Result<bool> {
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
                    return Ok(true);
                }
            }
        }
        Ok(false)
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
