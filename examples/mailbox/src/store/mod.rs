//! The mailbox store: mailboxes kept on disk, shared by the SMTP receiver and every IMAP
//! session.
//!
//! ```text
//!   <dir>/catalog.json          the mailboxes: name, id, special use; the next ids
//!   <dir>/m<id>/mailbox.json    one mailbox's model: validity, next UID, messages' metadata
//!   <dir>/m<id>/<uid>.eml       one message's bytes, written once, never changed
//! ```
//!
//! Every change is a transaction over the pure model: the shell applies the change to a copy,
//! persists the copy (message bytes first, then the metadata, each by an atomic rename), and
//! only then makes it current, under one lock. A crash leaves the store at a state it held;
//! a message file whose metadata never landed is an orphan the store never lists.
//!
//! Each committed change to a mailbox is announced on a broadcast channel by mailbox id, which
//! IMAP sessions watch to tell their clients (`IDLE`, and untagged responses after commands).

mod model;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;

pub use model::Flag;
pub use model::FlagChange;
pub use model::Mailbox;
pub use model::Message;
pub use model::SpecialUse;
pub use model::UidsExhausted;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::sync::Mutex;

/// File name of the catalog.
const CATALOG_FILE: &str = "catalog.json";
/// File name of a mailbox's model, in its directory.
const MAILBOX_FILE: &str = "mailbox.json";
/// The name of the inbox, the one mailbox name that is case-insensitive (RFC 3501 §5.1).
pub const INBOX: &str = "INBOX";
/// The mailboxes a new store holds, with their special uses.
const DEFAULT_MAILBOXES: [(&str, Option<SpecialUse>); 6] = [
    (INBOX, None),
    ("Sent", Some(SpecialUse::Sent)),
    ("Drafts", Some(SpecialUse::Drafts)),
    ("Trash", Some(SpecialUse::Trash)),
    ("Junk", Some(SpecialUse::Junk)),
    ("Archive", Some(SpecialUse::Archive)),
];
/// How many changes a slow watcher may lag behind before it misses some; a watcher that lags
/// resynchronises from the store as a whole.
const CHANGE_BACKLOG: usize = 256;

/// Why a store operation failed.
#[derive(Debug, Error)]
pub enum StoreError {
    /// No mailbox has that name.
    #[error("no mailbox {0:?}")]
    NoMailbox(String),
    /// A mailbox with that name exists already.
    #[error("mailbox {0:?} exists")]
    MailboxExists(String),
    /// The mailbox cannot be deleted or renamed: it is the inbox.
    #[error("the inbox cannot be removed")]
    Inbox,
    /// No message has that UID.
    #[error("no message {0}")]
    NoMessage(NonZeroU32),
    /// The mailbox has assigned every UID.
    #[error(transparent)]
    UidsExhausted(#[from] UidsExhausted),
    /// The message is larger than the store keeps.
    #[error("a message of {0} bytes is too large")]
    TooLarge(usize),
    /// Reading or writing the store failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The failure.
        source: std::io::Error,
    },
    /// A store file does not decode.
    #[error("{path}: {source}")]
    Json {
        /// The file.
        path: PathBuf,
        /// The failure.
        source: serde_json::Error,
    },
}

impl StoreError {
    /// The I/O failure `source` on `path`.
    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// A mailbox's stable identity: it survives a rename, so a session that selected a mailbox
/// keeps it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MailboxId(u64);

/// A mailbox as the catalog lists it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Listed {
    /// Its name.
    pub name: String,
    /// Its id.
    pub id: MailboxId,
    /// Its special use, if any.
    pub special_use: Option<SpecialUse>,
}

/// The catalog file: every mailbox, and the next id and least UID validity to assign.
#[derive(Debug, Deserialize, Serialize)]
struct Catalog {
    /// The next mailbox id.
    next_id: u64,
    /// The least UID validity the next mailbox may have; a catalog written without it reads
    /// as 0, which the time then exceeds.
    #[serde(default)]
    next_validity: u32,
    /// The mailboxes.
    mailboxes: Vec<Listed>,
}

/// The ids the catalog assigns next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Next {
    /// The next mailbox id.
    id: u64,
    /// The least UID validity the next mailbox may have.
    validity: u32,
}

/// The result of a copy: the target's validity, and each source UID with its copy's UID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Copied {
    /// The target mailbox's UID validity.
    pub uid_validity: NonZeroU32,
    /// Each copied message: its UID in the source and in the target, ascending.
    pub uids: Vec<(NonZeroU32, NonZeroU32)>,
}

/// The store's state: the catalog and every mailbox's model.
struct State {
    next: Next,
    listed: Vec<Listed>,
    mailboxes: BTreeMap<MailboxId, Mailbox>,
}

impl State {
    /// The listing of the mailbox named `name`.
    fn find(&self, name: &str) -> Option<&Listed> {
        self.listed
            .iter()
            .find(|listed| same_mailbox_name(&listed.name, name))
    }

    /// The model of mailbox `id`.
    fn mailbox(&self, id: MailboxId) -> Result<&Mailbox, StoreError> {
        self.mailboxes
            .get(&id)
            .ok_or_else(|| StoreError::NoMailbox(format!("#{}", id.0)))
    }
}

/// Whether `a` and `b` name the same mailbox: equal, or both the inbox in any case.
pub fn same_mailbox_name(a: &str, b: &str) -> bool {
    a == b || (a.eq_ignore_ascii_case(INBOX) && b.eq_ignore_ascii_case(INBOX))
}

/// The mailbox store.
pub struct Store {
    dir: PathBuf,
    state: Mutex<State>,
    changes: broadcast::Sender<MailboxId>,
    max_message_size: usize,
}

impl Store {
    /// Open the store in `dir`, creating it with the default mailboxes when it is new. A
    /// message larger than `max_message_size` bytes is refused.
    pub async fn open(dir: &Path, max_message_size: usize) -> Result<Self, StoreError> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|source| StoreError::io(dir, source))?;
        let catalog_path = dir.join(CATALOG_FILE);
        let state = match read_json::<Catalog>(&catalog_path).await? {
            Some(catalog) => {
                let mut mailboxes = BTreeMap::new();
                for listed in &catalog.mailboxes {
                    let path = mailbox_dir(dir, listed.id).join(MAILBOX_FILE);
                    let mailbox = read_json::<Mailbox>(&path)
                        .await?
                        .ok_or_else(|| StoreError::NoMailbox(listed.name.clone()))?;
                    mailboxes.insert(listed.id, mailbox);
                }
                State {
                    next: Next {
                        id: catalog.next_id,
                        validity: catalog.next_validity,
                    },
                    listed: catalog.mailboxes,
                    mailboxes,
                }
            }
            None => State {
                next: Next { id: 0, validity: 1 },
                listed: Vec::new(),
                mailboxes: BTreeMap::new(),
            },
        };
        let (changes, _) = broadcast::channel(CHANGE_BACKLOG);
        let store = Self {
            dir: dir.to_path_buf(),
            state: Mutex::new(state),
            changes,
            max_message_size,
        };
        for (name, special_use) in DEFAULT_MAILBOXES {
            match store.create_with(name, special_use).await {
                Ok(_) | Err(StoreError::MailboxExists(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(store)
    }

    /// Watch the store's changes: each committed change sends the changed mailbox's id.
    pub fn watch(&self) -> broadcast::Receiver<MailboxId> {
        self.changes.subscribe()
    }

    /// The mailboxes, in catalog order.
    pub async fn list(&self) -> Vec<Listed> {
        self.state.lock().await.listed.clone()
    }

    /// The listing and model of the mailbox named `name`.
    pub async fn mailbox(&self, name: &str) -> Result<(Listed, Mailbox), StoreError> {
        let state = self.state.lock().await;
        let listed = state
            .find(name)
            .cloned()
            .ok_or_else(|| StoreError::NoMailbox(name.to_owned()))?;
        let mailbox = state.mailbox(listed.id)?.clone();
        Ok((listed, mailbox))
    }

    /// The model of mailbox `id`.
    pub async fn mailbox_by_id(&self, id: MailboxId) -> Result<Mailbox, StoreError> {
        self.state.lock().await.mailbox(id).cloned()
    }

    /// Create a mailbox named `name`.
    pub async fn create(&self, name: &str) -> Result<MailboxId, StoreError> {
        self.create_with(name, None).await
    }

    /// Create a mailbox named `name` with special use `special_use`.
    async fn create_with(
        &self,
        name: &str,
        special_use: Option<SpecialUse>,
    ) -> Result<MailboxId, StoreError> {
        let mut state = self.state.lock().await;
        if state.find(name).is_some() {
            return Err(StoreError::MailboxExists(name.to_owned()));
        }
        let id = MailboxId(state.next.id);
        let validity = uid_validity(state.next.validity, chrono::Utc::now().timestamp());
        let mailbox = Mailbox::new(validity);
        let directory = mailbox_dir(&self.dir, id);
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|source| StoreError::io(&directory, source))?;
        write_json(&directory.join(MAILBOX_FILE), &mailbox).await?;
        let mut listed = state.listed.clone();
        listed.push(Listed {
            name: name.to_owned(),
            id,
            special_use,
        });
        let next = Next {
            id: state.next.id.saturating_add(1),
            validity: validity.get().saturating_add(1),
        };
        self.write_catalog(next, &listed).await?;
        state.next = next;
        state.listed = listed;
        state.mailboxes.insert(id, mailbox);
        Ok(id)
    }

    /// Delete the mailbox named `name` and its messages.
    pub async fn delete(&self, name: &str) -> Result<(), StoreError> {
        let mut state = self.state.lock().await;
        let listed = state
            .find(name)
            .cloned()
            .ok_or_else(|| StoreError::NoMailbox(name.to_owned()))?;
        if same_mailbox_name(&listed.name, INBOX) {
            return Err(StoreError::Inbox);
        }
        let remaining = state
            .listed
            .iter()
            .filter(|other| other.id != listed.id)
            .cloned()
            .collect::<Vec<_>>();
        self.write_catalog(state.next, &remaining).await?;
        state.listed = remaining;
        state.mailboxes.remove(&listed.id);
        let directory = mailbox_dir(&self.dir, listed.id);
        if let Err(error) = tokio::fs::remove_dir_all(&directory).await {
            tracing::warn!("{}: {error}", directory.display());
        }
        let _ = self.changes.send(listed.id);
        Ok(())
    }

    /// Rename the mailbox named `from` to `to`.
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), StoreError> {
        let mut state = self.state.lock().await;
        let listed = state
            .find(from)
            .cloned()
            .ok_or_else(|| StoreError::NoMailbox(from.to_owned()))?;
        if same_mailbox_name(&listed.name, INBOX) {
            return Err(StoreError::Inbox);
        }
        if state.find(to).is_some() {
            return Err(StoreError::MailboxExists(to.to_owned()));
        }
        let renamed = state
            .listed
            .iter()
            .map(|other| {
                if other.id == listed.id {
                    Listed {
                        name: to.to_owned(),
                        ..other.clone()
                    }
                } else {
                    other.clone()
                }
            })
            .collect::<Vec<_>>();
        self.write_catalog(state.next, &renamed).await?;
        state.listed = renamed;
        Ok(())
    }

    /// Append `bytes` to the mailbox named `name` with `flags`, received at `internal_date`;
    /// return the mailbox's validity and the message's UID.
    pub async fn append(
        &self,
        name: &str,
        bytes: &[u8],
        flags: BTreeSet<Flag>,
        internal_date: i64,
    ) -> Result<(NonZeroU32, NonZeroU32), StoreError> {
        let size = self.admit(bytes)?;
        let mut state = self.state.lock().await;
        let id = state
            .find(name)
            .map(|listed| listed.id)
            .ok_or_else(|| StoreError::NoMailbox(name.to_owned()))?;
        let mut mailbox = state.mailbox(id)?.clone();
        let uid = mailbox.append(flags, internal_date, size)?;
        self.write_message(id, uid, bytes).await?;
        self.commit(&mut state, id, mailbox)
            .await
            .map(|mailbox| (mailbox.uid_validity, uid))
    }

    /// The bytes of message `uid` in mailbox `id`.
    pub async fn read(&self, id: MailboxId, uid: NonZeroU32) -> Result<Vec<u8>, StoreError> {
        let path = mailbox_dir(&self.dir, id).join(format!("{uid}.eml"));
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(StoreError::NoMessage(uid)),
            Err(error) => Err(StoreError::io(&path, error)),
        }
    }

    /// Change the flags of the messages `uids` of mailbox `id`; return each changed message's
    /// UID and new flags.
    pub async fn change_flags(
        &self,
        id: MailboxId,
        uids: &BTreeSet<NonZeroU32>,
        change: FlagChange,
        flags: &BTreeSet<Flag>,
    ) -> Result<Vec<(NonZeroU32, BTreeSet<Flag>)>, StoreError> {
        let mut state = self.state.lock().await;
        let mut mailbox = state.mailbox(id)?.clone();
        let changed = mailbox.change_flags(uids, change, flags);
        if !changed.is_empty() {
            self.commit(&mut state, id, mailbox).await?;
        }
        Ok(changed)
    }

    /// Expunge mailbox `id`, only the messages in `only` when given; return the removed UIDs.
    pub async fn expunge(
        &self,
        id: MailboxId,
        only: Option<&BTreeSet<NonZeroU32>>,
    ) -> Result<Vec<NonZeroU32>, StoreError> {
        let mut state = self.state.lock().await;
        let mut mailbox = state.mailbox(id)?.clone();
        let removed = mailbox.expunge(only);
        if removed.is_empty() {
            return Ok(removed);
        }
        self.commit(&mut state, id, mailbox).await?;
        for uid in &removed {
            let path = mailbox_dir(&self.dir, id).join(format!("{uid}.eml"));
            if let Err(error) = tokio::fs::remove_file(&path).await {
                tracing::warn!("{}: {error}", path.display());
            }
        }
        Ok(removed)
    }

    /// Copy the messages `uids` of mailbox `from` into the mailbox named `to`, with their flags
    /// and dates.
    pub async fn copy(
        &self,
        from: MailboxId,
        uids: &BTreeSet<NonZeroU32>,
        to: &str,
    ) -> Result<Copied, StoreError> {
        let mut state = self.state.lock().await;
        let target = state
            .find(to)
            .map(|listed| listed.id)
            .ok_or_else(|| StoreError::NoMailbox(to.to_owned()))?;
        let source = state.mailbox(from)?.clone();
        let mut mailbox = state.mailbox(target)?.clone();
        let mut copied = Vec::new();
        for message in source
            .messages
            .iter()
            .filter(|message| uids.contains(&message.uid))
        {
            let bytes = self.read(from, message.uid).await?;
            let uid = mailbox.append(message.flags.clone(), message.internal_date, message.size)?;
            self.write_message(target, uid, &bytes).await?;
            copied.push((message.uid, uid));
        }
        let mailbox = self.commit(&mut state, target, mailbox).await?;
        Ok(Copied {
            uid_validity: mailbox.uid_validity,
            uids: copied,
        })
    }

    /// The size of `bytes`, if the store keeps a message that large.
    fn admit(&self, bytes: &[u8]) -> Result<u32, StoreError> {
        u32::try_from(bytes.len())
            .ok()
            .filter(|_| bytes.len() <= self.max_message_size)
            .ok_or(StoreError::TooLarge(bytes.len()))
    }

    /// Persist `mailbox` as mailbox `id`, make it current in `state`, and announce the change.
    async fn commit<'s>(
        &self,
        state: &'s mut State,
        id: MailboxId,
        mailbox: Mailbox,
    ) -> Result<&'s Mailbox, StoreError> {
        write_json(&mailbox_dir(&self.dir, id).join(MAILBOX_FILE), &mailbox).await?;
        state.mailboxes.insert(id, mailbox);
        let _ = self.changes.send(id);
        state.mailbox(id)
    }

    /// Write message `uid` of mailbox `id`.
    async fn write_message(
        &self,
        id: MailboxId,
        uid: NonZeroU32,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        write_atomic(
            &mailbox_dir(&self.dir, id).join(format!("{uid}.eml")),
            bytes,
        )
        .await
    }

    /// Write the catalog of `listed`, assigning `next` next.
    async fn write_catalog(&self, next: Next, listed: &[Listed]) -> Result<(), StoreError> {
        write_json(&self.dir.join(CATALOG_FILE), &Catalog {
            next_id: next.id,
            next_validity: next.validity,
            mailboxes: listed.to_vec(),
        })
        .await
    }
}

/// The directory of mailbox `id`.
fn mailbox_dir(dir: &Path, id: MailboxId) -> PathBuf {
    dir.join(format!("m{}", id.0))
}

/// The UID validity of a mailbox created at `now` (seconds since the Unix epoch), when no
/// mailbox may have one below `least`: the later of the two, so validities only grow, and a
/// mailbox deleted and created again, within the same second even, gets a new one (RFC 3501
/// §2.3.1.1). The time keeps them growing across a store that was wiped and made again.
pub fn uid_validity(least: u32, now: i64) -> NonZeroU32 {
    let now = u32::try_from(now.max(0)).unwrap_or(u32::MAX);
    NonZeroU32::new(least.max(now)).unwrap_or(NonZeroU32::MIN)
}

/// The value the JSON file `path` holds, or `None` when there is no such file.
async fn read_json<T>(path: &Path) -> Result<Option<T>, StoreError>
where T: for<'de> Deserialize<'de> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| StoreError::Json {
                path: path.to_path_buf(),
                source,
            }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StoreError::io(path, error)),
    }
}

/// Replace the JSON file `path` with `value`, atomically.
async fn write_json<T>(path: &Path, value: &T) -> Result<(), StoreError>
where T: Serialize {
    let bytes = serde_json::to_vec(value).map_err(|source| StoreError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomic(path, &bytes).await
}

/// Replace the file `path` with `bytes`, atomically.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    crate::files::write_atomic(path, bytes)
        .await
        .map_err(|(path, source)| StoreError::Io { path, source })
}

#[cfg(test)]
mod test_store;
