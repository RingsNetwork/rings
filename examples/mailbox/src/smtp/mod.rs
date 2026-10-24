//! The SMTP shell: the receiver for the mailbox's own address, and submission for its user.
//!
//! Both run the same [`Session`] over a socket and differ in their [`Role`] and in where a
//! committed transaction goes:
//!
//! - the receiver accepts mail for `me@<did>.<zone>` and files it in the INBOX, with a
//!   `Return-Path` and a `Received` trace header (RFC 5321 §4.4);
//! - submission accepts mail for anyone and hands it to the outbound queue, which delivers it
//!   to the recipients' mail exchangers.
//!
//! A reply to a transaction is sent only after the transaction is committed, so a `250`
//! means the message is on disk.

mod session;

use std::sync::Arc;

pub use session::Next;
pub use session::Reply;
pub use session::Role;
pub use session::Session;
pub use session::Transaction;
use smtp_proto::request::receiver::DataReceiver;
use smtp_proto::request::receiver::RequestReceiver;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use crate::outbound::Outbound;
use crate::store::Store;
use crate::store::INBOX;

/// Most recipients one transaction may name.
pub const MAX_RECIPIENTS: usize = 100;
/// Size of one socket read.
const READ_CHUNK: usize = 16 * 1024;

/// Where committed transactions go.
#[derive(Clone)]
pub enum Sink {
    /// Filed in the store's INBOX.
    Inbox(Arc<Store>),
    /// Queued for delivery onward.
    Outbound(Arc<Outbound>),
}

impl Sink {
    /// Commit `transaction`, received by `host`; return the reply to send.
    async fn commit(&self, host: &str, transaction: Transaction, ok: Reply) -> Reply {
        let committed = match self {
            Self::Inbox(store) => {
                let message = with_trace(host, &transaction);
                let now = chrono::Utc::now().timestamp();
                store
                    .append(INBOX, &message, Default::default(), now)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
            Self::Outbound(outbound) => outbound
                .enqueue(&transaction.from, &transaction.to, &transaction.message)
                .await
                .map_err(|error| error.to_string()),
        };
        match committed {
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
}

/// `transaction`'s message with its trace headers prepended: the reverse path, and the hop
/// from the client to `host`.
fn with_trace(host: &str, transaction: &Transaction) -> Vec<u8> {
    let date = chrono::Utc::now().to_rfc2822();
    let mut message = format!(
        "Return-Path: <{}>\r\nReceived: from {} by {host} with ESMTP; {date}\r\n",
        transaction.from, transaction.client
    )
    .into_bytes();
    message.extend_from_slice(&transaction.message);
    message
}

/// One SMTP listener: its role, its name, its size bound, and where it commits.
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
}

impl SmtpService {
    /// Serve on `listener` forever, one task per connection.
    pub async fn serve(self, listener: TcpListener) {
        let service = Arc::new(self);
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let service = Arc::clone(&service);
                    tokio::spawn(async move {
                        if let Err(error) = service.connection(stream).await {
                            tracing::debug!("smtp {peer}: {error}");
                        }
                    });
                }
                Err(error) => tracing::warn!("smtp: accept failed: {error}"),
            }
        }
    }

    /// Run one session on `stream` until the client quits or the connection ends.
    async fn connection(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let mut session =
            Session::new(self.role.clone(), &self.host, self.max_size, MAX_RECIPIENTS);
        stream.write_all(&session.greeting().encode()).await?;
        let mut requests = RequestReceiver::default();
        let mut body: Option<(DataReceiver, Vec<u8>, bool)> = None;
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok(());
            }
            let mut bytes = chunk.get(..read).unwrap_or_default().iter();
            while !bytes.as_slice().is_empty() {
                if let Some((receiver, buffer, oversize)) = body.as_mut() {
                    let complete = receiver.ingest(&mut bytes, buffer);
                    if buffer.len() > session.max_size() {
                        // Keep reading to the end of the body, but keep none of it.
                        *oversize = true;
                        buffer.clear();
                    }
                    if !complete {
                        continue;
                    }
                    let Some((_, buffer, oversize)) = body.take() else {
                        continue;
                    };
                    let reply = if oversize {
                        session.data_too_large()
                    } else {
                        match session.data(buffer) {
                            (Some(transaction), ok) => {
                                self.sink.commit(&self.host, transaction, ok).await
                            }
                            (None, refused) => refused,
                        }
                    };
                    stream.write_all(&reply.encode()).await?;
                    continue;
                }
                let next = match requests.ingest(&mut bytes) {
                    Ok(request) => session.step(request.into_owned()),
                    Err(smtp_proto::Error::NeedsMoreData { .. }) => break,
                    Err(_) => session.unparsable(),
                };
                match next {
                    Next::Reply(reply) => stream.write_all(&reply.encode()).await?,
                    Next::ReadData(reply) => {
                        stream.write_all(&reply.encode()).await?;
                        body = Some((DataReceiver::new(), Vec::new(), false));
                    }
                    Next::Close(reply) => {
                        stream.write_all(&reply.encode()).await?;
                        return stream.shutdown().await;
                    }
                }
            }
        }
    }
}
