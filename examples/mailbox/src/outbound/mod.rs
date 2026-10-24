//! The outbound queue: mail the user submits, delivered to each recipient's mail exchanger.
//!
//! ```text
//!   <dir>/outbound/<id>.eml    the message, as submitted
//!   <dir>/outbound/<id>.json   its delivery state (schedule::Pending)
//!
//!   enqueue ─▶ files written ─▶ wake ─▶ worker: for each due message, for each recipient:
//!     exchangers(domain) ─▶ SMTP to each in turn (client: STARTTLS when offered)
//!     ─▶ Outcome ─▶ settle: delivered dropped, deferred rescheduled, failed bounced to INBOX
//! ```
//!
//! A recipient `me@<did>.<zone>` resolves, through the relay's DNS, to the owner's own IPv6
//! address, so one node's mailbox writes to another's through the relay like any mail server.
//! Where a recipient's exchangers are is a capability ([`Exchangers`]): the system resolver in
//! the program, a fixed map in tests. A domain that does not exist or takes no mail bounces at
//! once; one DNS cannot tell about now stays queued.

mod client;
mod exchangers;
mod schedule;

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use client::Delivery;
pub use client::SendError;
pub use exchangers::exchange_hosts;
pub use exchangers::DnsExchangers;
pub use exchangers::Exchangers;
pub use exchangers::ExchangersError;
pub use schedule::Outcome;
pub use schedule::Pending;
pub use schedule::Settled;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::Notify;

use crate::store::Store;
use crate::store::INBOX;

/// How long the worker sleeps when nothing is queued: a wake comes with every enqueue.
const IDLE_SLEEP: Duration = Duration::from_secs(3_600);

/// Why the queue could not take or keep a message.
#[derive(Debug, Error)]
pub enum OutboundError {
    /// Reading or writing the queue failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The failure.
        source: std::io::Error,
    },
    /// A queue file does not decode, or a state does not encode.
    #[error("{path}: {source}")]
    Json {
        /// The file.
        path: PathBuf,
        /// The failure.
        source: serde_json::Error,
    },
}

/// The outbound queue.
pub struct Outbound {
    dir: PathBuf,
    host: String,
    store: Arc<Store>,
    exchangers: Arc<dyn Exchangers>,
    queue: Mutex<(u64, BTreeMap<u64, Pending>)>,
    wake: Notify,
}

impl Outbound {
    /// Open the queue in `dir`, delivering as `host`, bouncing into `store`, and finding
    /// exchangers with `exchangers`.
    pub async fn open(
        dir: &Path,
        host: String,
        store: Arc<Store>,
        exchangers: Arc<dyn Exchangers>,
    ) -> Result<Self, OutboundError> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|source| io_error(dir, source))?;
        let mut queue = BTreeMap::new();
        let mut entries = tokio::fs::read_dir(dir)
            .await
            .map_err(|source| io_error(dir, source))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| io_error(dir, source))?
        {
            let path = entry.path();
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u64>().ok());
            if let (Some(id), Some("json")) = (id, path.extension().and_then(|ext| ext.to_str())) {
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|source| io_error(&path, source))?;
                let pending = serde_json::from_slice::<Pending>(&bytes)
                    .map_err(|source| OutboundError::Json { path, source })?;
                queue.insert(id, pending);
            }
        }
        let next_id = queue
            .keys()
            .next_back()
            .map_or(0, |id| id.saturating_add(1));
        Ok(Self {
            dir: dir.to_path_buf(),
            host,
            store,
            exchangers,
            queue: Mutex::new((next_id, queue)),
            wake: Notify::new(),
        })
    }

    /// Queue `message` from `from` to `to` for delivery.
    pub async fn enqueue(
        &self,
        from: &str,
        to: &[String],
        message: &[u8],
    ) -> Result<(), OutboundError> {
        let now = chrono::Utc::now().timestamp();
        let pending = Pending::new(from.to_owned(), to.to_vec(), now);
        let mut queue = self.queue.lock().await;
        let id = queue.0;
        write_atomic(&self.message_path(id), message).await?;
        self.write_pending(id, &pending).await?;
        queue.0 = id.saturating_add(1);
        queue.1.insert(id, pending);
        self.wake.notify_one();
        Ok(())
    }

    /// Deliver queued mail forever.
    pub async fn run(self: Arc<Self>) {
        loop {
            let now = chrono::Utc::now().timestamp();
            let due = {
                let queue = self.queue.lock().await;
                queue
                    .1
                    .iter()
                    .filter(|(_id, pending)| pending.is_due(now))
                    .map(|(id, pending)| (*id, pending.clone()))
                    .collect::<Vec<_>>()
            };
            for (id, pending) in due {
                if let Err(error) = self.attempt(id, pending).await {
                    tracing::warn!("outbound: message {id}: {error}");
                }
            }
            let sleep = {
                let queue = self.queue.lock().await;
                let now = chrono::Utc::now().timestamp();
                queue
                    .1
                    .values()
                    .map(|pending| pending.next_attempt_at.saturating_sub(now).max(1))
                    .min()
                    .and_then(|secs| u64::try_from(secs).ok())
                    .map_or(IDLE_SLEEP, Duration::from_secs)
            };
            tokio::select! {
                () = tokio::time::sleep(sleep) => {}
                () = self.wake.notified() => {}
            }
        }
    }

    /// Try every pending recipient of message `id` once, then settle it.
    async fn attempt(&self, id: u64, pending: Pending) -> Result<(), OutboundError> {
        let message = tokio::fs::read(self.message_path(id))
            .await
            .map_err(|source| io_error(&self.message_path(id), source))?;
        let mut outcomes = Vec::new();
        for recipient in &pending.recipients {
            let outcome = self.deliver(&pending.from, recipient, &message).await;
            outcomes.push((recipient.clone(), outcome));
        }
        let from = pending.from.clone();
        let settled = pending.settle(outcomes, chrono::Utc::now().timestamp());
        if !settled.bounced.is_empty() {
            self.bounce(&from, &settled.bounced, &message).await;
        }
        let mut queue = self.queue.lock().await;
        match settled.pending {
            Some(pending) => {
                self.write_pending(id, &pending).await?;
                queue.1.insert(id, pending);
            }
            None => {
                queue.1.remove(&id);
                for path in [self.message_path(id), self.pending_path(id)] {
                    if let Err(error) = tokio::fs::remove_file(&path).await {
                        tracing::warn!("outbound: {}: {error}", path.display());
                    }
                }
            }
        }
        Ok(())
    }

    /// Deliver `message` from `from` to `recipient`, trying its exchangers in turn.
    async fn deliver(&self, from: &str, recipient: &str, message: &[u8]) -> Outcome {
        let Some((_local, domain)) = recipient.rsplit_once('@') else {
            return Outcome::Failed(format!("{recipient} is not an address"));
        };
        let exchangers = match self.exchangers.exchangers(domain).await {
            Ok(exchangers) => exchangers,
            Err(error) if error.is_permanent() => return Outcome::Failed(error.to_string()),
            Err(error) => return Outcome::Deferred(error.to_string()),
        };
        let delivery = Delivery {
            helo: &self.host,
            from,
            recipient,
            message,
        };
        let mut last = format!("no exchanger for {domain} answered");
        for exchanger in exchangers {
            match client::deliver(exchanger, delivery).await {
                Ok(()) => return Outcome::Delivered,
                Err(error) if error.is_permanent() => {
                    return Outcome::Failed(format!("{exchanger}: {error}"));
                }
                Err(error) => last = format!("{exchanger}: {error}"),
            }
        }
        Outcome::Deferred(last)
    }

    /// File a bounce for `from`'s message `message` to the recipients `bounced` in the INBOX.
    async fn bounce(&self, from: &str, bounced: &[(String, String)], message: &[u8]) {
        let reasons = bounced
            .iter()
            .map(|(recipient, reason)| format!("  {recipient}: {reason}\r\n"))
            .collect::<String>();
        let headers_end = message
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map_or(message.len(), |end| end.saturating_add(2));
        let original = String::from_utf8_lossy(message.get(..headers_end).unwrap_or_default());
        let now = chrono::Utc::now();
        let report = format!(
            "From: Mail Delivery System <mailer-daemon@{host}>\r\nTo: <{from}>\r\nSubject: \
             Undelivered Mail Returned to Sender\r\nDate: {date}\r\nMessage-ID: \
             <{stamp}.bounce@{host}>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; \
             charset=utf-8\r\n\r\nYour message could not be delivered to:\r\n\r\n{reasons}\r\n\
             --- The headers of your message ---\r\n\r\n{original}",
            host = self.host,
            date = now.to_rfc2822(),
            stamp = now.timestamp_nanos_opt().unwrap_or_default(),
        );
        if let Err(error) = self
            .store
            .append(
                INBOX,
                report.as_bytes(),
                Default::default(),
                now.timestamp(),
            )
            .await
        {
            tracing::warn!("outbound: bounce not filed: {error}");
        }
    }

    /// The path of message `id`.
    fn message_path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id}.eml"))
    }

    /// The path of message `id`'s delivery state.
    fn pending_path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Write message `id`'s delivery state.
    async fn write_pending(&self, id: u64, pending: &Pending) -> Result<(), OutboundError> {
        let path = self.pending_path(id);
        let bytes = serde_json::to_vec(pending).map_err(|source| OutboundError::Json {
            path: path.clone(),
            source,
        })?;
        write_atomic(&path, &bytes).await
    }
}

/// The I/O failure `source` on `path`.
fn io_error(path: &Path, source: std::io::Error) -> OutboundError {
    OutboundError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Replace the file `path` with `bytes`, atomically.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), OutboundError> {
    crate::files::write_atomic(path, bytes)
        .await
        .map_err(|(path, source)| OutboundError::Io { path, source })
}
