//! The recipient's side: its reads of its mail topic, the keys it opens mail with, and what it
//! remembers. It builds for native and browser nodes alike: its records and its inbox are any
//! key-value store (a file store natively, IndexedDB in a browser) and any [`MailInbox`].
//!
//! ```text
//!   poll(t):  E = fetch(mail_topic(d))                      the elements live at t
//!             R = records ∖ { r | outlived(r, t) }           retention
//!             for o ∈ open(E, d, keyring(R), fresh = ¬opened_R):
//!               Incomplete             ─▶ nothing           tried again on the next poll
//!               Unreadable             ─▶ opened(id)
//!               Opened(m) ∧ filed_R(k) ─▶ opened(id)        k = H(Message-ID), or id without one
//!               Opened(m)              ─▶ inbox.file(m); filed(k); opened(id)
//! ```
//!
//! Laws:
//! - **Schedule.** The recipient polls when it starts and every `I ≤ H / 2` while it runs
//!   ([`MAX_MAIL_POLL_INTERVAL`]): a mail written just after one poll lives `H` and the next
//!   poll comes within `H / 2`, which leaves the other half for clock skew and propagation.
//! - **No removal.** It never writes its mail topic: every element retires at its horizon, and
//!   a data carrier's remove budget (`DATA_TOMBSTONE_MAX_LEN`) is never spent on mail.
//! - **At least once.** A mail is filed before it is remembered, so a crash between the two
//!   files it again after a restart: a mail is never lost to the recipient, at worst filed twice.
//! - **Deduplication.** Encryption is randomized, so a resent mail is a new ciphertext: mail
//!   with a `Message-ID` already filed is not filed again.
//! - **Retention.** An opened id is kept `H + σ` ([`OPENED_RETENTION_MS`]), after which no copy
//!   of its mail is served; a `Message-ID` is kept [`MESSAGE_ID_RETENTION_MS`]; a retired key is
//!   kept `H + σ` after its retirement, so mail sealed to it before a rotation still opens.
//! - **Rotation.** When the recipient starts under a delegatee key other than the one it stored
//!   as current, it retires the stored one at that instant.
//!
//! The records hold the delegatee secrets the keyring opens mail with, as the node's own
//! configuration holds its current one.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::consts::MAX_TTL_MS;
use rings_core::consts::TS_OFFSET_TOLERANCE_MS;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::entry::Entry;
use rings_core::lifecycle::StopToken;
use rings_core::storage::KvStorageInterface;
use rings_core::utils::get_epoch_ms;
use rings_runtime::MaybeSendSync;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use super::envelope::MailDigest;
use super::mail_topic;
use super::open::open_mail;
use super::open::OpenedMail;
use super::open::Opening;
use crate::prelude::rings_core::dht::Did;
use crate::processor::sleep_unless_stopped;
use crate::processor::Processor;

/// The default interval between two polls of the mail topic.
pub const DEFAULT_MAIL_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// The longest interval between two polls, `H / 2` (the schedule law).
pub const MAX_MAIL_POLL_INTERVAL: Duration = Duration::from_millis(MAX_TTL_MS / 2);
/// How long an opened mail id, and a retired key, is kept: `H + σ`, after which no owner on a
/// clock within the skew tolerance serves an element of a mail written before.
pub const OPENED_RETENTION_MS: u128 = MAX_TTL_MS as u128 + TS_OFFSET_TOLERANCE_MS;
/// How long a filed `Message-ID` is kept: five days, the longest a sender retries a message
/// (RFC 5321 §4.5.4.1), so a resend never files a mail twice.
pub const MESSAGE_ID_RETENTION_MS: u128 = 5 * 24 * 3600 * 1000;

/// Where the recipient files the mail it opens: the mailbox store natively, IndexedDB in a
/// browser ([`KvInbox`]).
#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
pub trait MailInbox: MaybeSendSync {
    /// Why a mail could not be filed.
    type Error: std::error::Error + MaybeSendSync;

    /// File `mail`; `Ok` once it is kept.
    async fn file(&self, mail: &OpenedMail) -> Result<(), Self::Error>;
}

/// A mail as a [`KvInbox`] keeps it.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct StoredMail {
    /// The gateway that wrote it.
    pub gateway: Did,
    /// The message.
    pub message: Vec<u8>,
    /// When it was filed, in milliseconds since Unix epoch.
    pub filed_at_ms: u128,
}

/// An inbox over a key-value store: each mail under `mail-<mail id>`.
pub struct KvInbox<K> {
    store: K,
}

impl<K> KvInbox<K> {
    /// The inbox keeping its mail in `store`.
    pub fn new(store: K) -> Self {
        Self { store }
    }

    /// The store the mail is kept in.
    pub fn store(&self) -> &K {
        &self.store
    }
}

#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
impl<K> MailInbox for KvInbox<K>
where K: KvStorageInterface<StoredMail> + MaybeSendSync
{
    type Error = rings_core::error::Error;

    async fn file(&self, mail: &OpenedMail) -> Result<(), Self::Error> {
        let stored = StoredMail {
            gateway: mail.gateway,
            message: mail.message.clone(),
            filed_at_ms: get_epoch_ms(),
        };
        self.store
            .put(&format!("mail-{}", mail.mail_id), &stored)
            .await
    }
}

/// A record the recipient keeps.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum RecipientRecord {
    /// A mail id opened, or found unreadable, at `at_ms`: it is not opened again.
    Opened {
        /// When.
        at_ms: u128,
    },
    /// A mail with this deduplication key filed at `at_ms`.
    Filed {
        /// When.
        at_ms: u128,
    },
    /// A delegatee key: the current one (`retired_at_ms = None`) or one retired then.
    Key {
        /// The key, secret included.
        key: DelegateeKey,
        /// When it was retired, if it was.
        retired_at_ms: Option<u128>,
    },
}

impl RecipientRecord {
    /// Whether this record has outlived its retention at `now_ms` (the retention law); the
    /// current key never does.
    pub fn outlived_at(&self, now_ms: u128) -> bool {
        let (since, retention) = match self {
            Self::Opened { at_ms } => (*at_ms, OPENED_RETENTION_MS),
            Self::Filed { at_ms } => (*at_ms, MESSAGE_ID_RETENTION_MS),
            Self::Key {
                retired_at_ms: Some(at_ms),
                ..
            } => (*at_ms, OPENED_RETENTION_MS),
            Self::Key {
                retired_at_ms: None,
                ..
            } => return false,
        };
        now_ms >= since.saturating_add(retention)
    }
}

/// The name a record is kept under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordName {
    /// A mail id's [`RecipientRecord::Opened`].
    Opened(MailDigest),
    /// A deduplication key's [`RecipientRecord::Filed`].
    Filed(MailDigest),
    /// A delegatee's [`RecipientRecord::Key`], by the delegatee's DID.
    Key(Did),
}

impl std::fmt::Display for RecordName {
    /// `opened-<hex>`, `filed-<hex>` or `key-<did>`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Opened(mail_id) => write!(f, "opened-{mail_id}"),
            Self::Filed(key) => write!(f, "filed-{key}"),
            Self::Key(delegatee) => write!(f, "key-{delegatee}"),
        }
    }
}

/// The records a recipient starting at `now_ms` under `current` writes (the rotation law): every
/// stored current key that is not `current` retired now, and `current` as current.
pub(super) fn rotation(
    stored: &[(String, RecipientRecord)],
    current: &DelegateeKey,
    now_ms: u128,
) -> Vec<(String, RecipientRecord)> {
    let current_delegatee = current.delegation().delegatee_did();
    let retired = stored.iter().filter_map(|(_, record)| match record {
        RecipientRecord::Key {
            key,
            retired_at_ms: None,
        } if key.delegation().delegatee_did() != current_delegatee => Some((
            RecordName::Key(key.delegation().delegatee_did()).to_string(),
            RecipientRecord::Key {
                key: key.clone(),
                retired_at_ms: Some(now_ms),
            },
        )),
        _ => None,
    });
    let current_record = (
        RecordName::Key(current_delegatee).to_string(),
        RecipientRecord::Key {
            key: current.clone(),
            retired_at_ms: None,
        },
    );
    let stored_as_current = stored.contains(&current_record);
    retired
        .chain((!stored_as_current).then_some(current_record))
        .collect()
}

/// The keys a recipient under `current` opens mail with: `current`, then every retired key of
/// `records`.
fn keyring(
    records: &BTreeMap<String, RecipientRecord>,
    current: &DelegateeKey,
) -> Vec<DelegateeKey> {
    let retired = records.values().filter_map(|record| match record {
        RecipientRecord::Key {
            key,
            retired_at_ms: Some(_),
        } => Some(key.clone()),
        _ => None,
    });
    std::iter::once(current.clone()).chain(retired).collect()
}

/// The key a mail is deduplicated by: `H(Message-ID)`, or its own id without one.
fn dedup_key(mail: &OpenedMail) -> MailDigest {
    mail.message_id().map_or(mail.mail_id, |message_id| {
        MailDigest::of(message_id.as_bytes())
    })
}

/// What one poll did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PollReport {
    /// Mail filed in the inbox.
    pub filed: usize,
    /// Mail opened whose `Message-ID` was filed before.
    pub duplicates: usize,
    /// Mail no key opens.
    pub unreadable: usize,
    /// Mail a verified manifest lists a missing chunk of.
    pub incomplete: usize,
}

/// Why the recipient could not start or poll.
#[derive(Debug, Error)]
pub enum RecipientError<E> {
    /// The poll interval exceeds [`MAX_MAIL_POLL_INTERVAL`].
    #[error(
        "mail poll interval {interval:?} exceeds {MAX_MAIL_POLL_INTERVAL:?}, half the horizon"
    )]
    PollInterval {
        /// The interval asked for.
        interval: Duration,
    },
    /// The DHT key of the mail topic cannot be derived.
    #[error("no DHT key for the mail topic: {0}")]
    Key(#[source] rings_core::error::Error),
    /// The fetch of the mail topic failed.
    #[error("cannot fetch the mail topic: {0}")]
    Fetch(#[source] crate::error::Error),
    /// The records could not be read or written.
    #[error("mail records: {0}")]
    Records(#[source] rings_core::error::Error),
    /// The inbox did not file a mail.
    #[error("cannot file a mail: {0}")]
    Inbox(#[source] E),
}

/// A recipient: a node reading its own mail topic into `inbox`, keeping its records in
/// `records`.
pub struct Recipient<I, K> {
    processor: Arc<Processor>,
    inbox: I,
    records: K,
    interval: Duration,
}

impl<I, K> Recipient<I, K>
where
    I: MailInbox,
    K: KvStorageInterface<RecipientRecord> + MaybeSendSync,
{
    /// Start the recipient of `processor`'s DID, polling every `interval`: apply the rotation
    /// law to `records`.
    pub async fn start(
        processor: Arc<Processor>,
        inbox: I,
        records: K,
        interval: Duration,
    ) -> Result<Self, RecipientError<I::Error>> {
        if interval > MAX_MAIL_POLL_INTERVAL {
            return Err(RecipientError::PollInterval { interval });
        }
        let stored = records.get_all().await.map_err(RecipientError::Records)?;
        for (name, record) in rotation(&stored, processor.delegatee_key(), get_epoch_ms()) {
            records
                .put(&name, &record)
                .await
                .map_err(RecipientError::Records)?;
        }
        Ok(Self {
            processor,
            inbox,
            records,
            interval,
        })
    }

    /// The inbox mail is filed in.
    pub fn inbox(&self) -> &I {
        &self.inbox
    }

    /// Read the mail topic once, file what opens, and remember it (the poll of this module).
    pub async fn poll(&self) -> Result<PollReport, RecipientError<I::Error>> {
        let did = self.processor.did();
        let key = Entry::gen_did(&mail_topic(did)).map_err(RecipientError::Key)?;
        let entry = self
            .processor
            .fetch_storage_entry(key)
            .await
            .map_err(RecipientError::Fetch)?;
        let now_ms = get_epoch_ms();
        let mut records = self.retained_records(now_ms).await?;
        let Some(entry) = entry else {
            return Ok(PollReport::default());
        };
        let keys = keyring(&records, self.processor.delegatee_key());
        let fresh =
            |mail_id: &MailDigest| !records.contains_key(&RecordName::Opened(*mail_id).to_string());
        let network_id = self.processor.swarm.network_id();
        let openings = open_mail(&entry.data, did, &keys, network_id, now_ms, fresh);
        let mut report = PollReport::default();
        for opening in openings {
            let mail_id = opening.mail_id();
            match opening {
                Opening::Incomplete { .. } => {
                    report.incomplete = report.incomplete.saturating_add(1);
                    continue;
                }
                Opening::Unreadable { .. } => {
                    report.unreadable = report.unreadable.saturating_add(1);
                }
                Opening::Opened(mail) => {
                    let filed = RecordName::Filed(dedup_key(&mail)).to_string();
                    if records.contains_key(&filed) {
                        report.duplicates = report.duplicates.saturating_add(1);
                    } else {
                        self.inbox
                            .file(&mail)
                            .await
                            .map_err(RecipientError::Inbox)?;
                        let record = RecipientRecord::Filed { at_ms: now_ms };
                        self.remember(&mut records, filed, record).await?;
                        report.filed = report.filed.saturating_add(1);
                    }
                }
            }
            let opened = RecordName::Opened(mail_id).to_string();
            let record = RecipientRecord::Opened { at_ms: now_ms };
            self.remember(&mut records, opened, record).await?;
        }
        Ok(report)
    }

    /// Poll now and then every interval until `stop`, logging a failed poll and polling again.
    pub async fn run(&self, stop: StopToken) {
        loop {
            match self.poll().await {
                Ok(report) if report.filed > 0 => {
                    tracing::info!(filed = report.filed, "mail: filed new mail");
                }
                Ok(_) => {}
                Err(error) => tracing::warn!("mail: poll failed: {error}"),
            }
            match sleep_unless_stopped(self.interval, &stop, &StopToken::never()).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    tracing::warn!("mail: stopping after a timer error: {error:?}");
                    return;
                }
            }
        }
    }

    /// The records retained at `now_ms`, after removing those that outlived their retention.
    async fn retained_records(
        &self,
        now_ms: u128,
    ) -> Result<BTreeMap<String, RecipientRecord>, RecipientError<I::Error>> {
        let mut records = BTreeMap::new();
        for (name, record) in self
            .records
            .get_all()
            .await
            .map_err(RecipientError::Records)?
        {
            if record.outlived_at(now_ms) {
                self.records
                    .remove(&name)
                    .await
                    .map_err(RecipientError::Records)?;
            } else {
                records.insert(name, record);
            }
        }
        Ok(records)
    }

    /// Keep `record` under `name`, in the store and in `records`, this poll's view of it: two
    /// copies of one mail opened by one poll are filed once.
    async fn remember(
        &self,
        records: &mut BTreeMap<String, RecipientRecord>,
        name: String,
        record: RecipientRecord,
    ) -> Result<(), RecipientError<I::Error>> {
        self.records
            .put(&name, &record)
            .await
            .map_err(RecipientError::Records)?;
        records.insert(name, record);
        Ok(())
    }
}
