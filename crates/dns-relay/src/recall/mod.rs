//! The relay's front of the directory: [`Recall`] answers from the [`AnnouncementMemory`] while
//! it is fresh, asks the directory within a bounded budget otherwise, and keeps the memory in the
//! state directory as the journal `announcements.jsonl`.
//!
//! ```text
//!   announcement(o) = answer               recall(μ, o) = Fresh(answer)
//!                   = observe(μ, o, D(o))   a lookup slot is free
//!                   = graced(μ, o)          no slot: the remembered announcement, or Busy
//! ```
//!
//! Any label of 40 hex digits names an owner, so a stream of queries for random owners, each
//! unknown to the memory, would turn into as many directory lookups. The budget bounds the
//! lookups in flight; a query that finds it spent is answered from the grace memory or refused
//! at once (`SERVFAIL`), never queued.
//!
//! The DNS authority and the ingress both read owners through it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

use crate::announcement::Announcement;
use crate::announcement::ProxyProtocol;
use crate::announcement::SubName;
use crate::announcement::TxtValue;
use crate::capability::Directory;
use crate::memory::AnnouncementMemory;
use crate::memory::MemoryChange;
use crate::memory::MemoryWindows;
use crate::memory::Recalled;
use crate::owner::Owner;
use crate::state::ensure_dir;
use crate::state::now_ms;
use crate::state::Journal;
use crate::state::StateError;

/// File name of the memory's journal in the state directory.
const MEMORY_FILE: &str = "announcements.jsonl";

/// Why [`Recall`] could not answer.
#[derive(Debug, Error)]
pub enum RecallError<E>
where E: std::error::Error + 'static
{
    /// The directory failed, and nothing is remembered within the grace period.
    #[error(transparent)]
    Directory(E),
    /// Every lookup slot is taken, and nothing is remembered within the grace period.
    #[error("every directory lookup slot is taken")]
    Busy,
}

/// One remembered announcement in the journal.
#[derive(Debug, Deserialize, Serialize)]
struct MemoryRecord {
    /// The ports served, with how their services learn origins.
    ports: Vec<(u16, ProxyProtocol)>,
    /// The TXT records, by name under the owner.
    txt: Vec<(String, Vec<String>)>,
    /// When the relay last saw the announcement, in milliseconds since the Unix epoch.
    seen_at_ms: u64,
}

impl MemoryRecord {
    /// The record of `announcement`, seen at `seen_at_ms`.
    fn of(announcement: &Announcement, seen_at_ms: u64) -> Self {
        Self {
            ports: announcement.served_ports().collect(),
            txt: announcement
                .txt_records()
                .map(|(name, values)| {
                    let values = values
                        .iter()
                        .map(|value| value.as_str().to_owned())
                        .collect();
                    (name.as_str().to_owned(), values)
                })
                .collect(),
            seen_at_ms,
        }
    }

    /// The announcement and sighting this record holds, or `None` when it does not validate:
    /// the relay never answers from a malformed record.
    fn into_parts(self) -> Option<(Announcement, u64)> {
        let txt = self
            .txt
            .into_iter()
            .map(|(name, values)| {
                let values = values
                    .into_iter()
                    .map(TxtValue::new)
                    .collect::<Result<Vec<_>, _>>()?;
                SubName::new(name).map(|name| (name, values))
            })
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        let announcement = Announcement::new(self.ports, txt).ok()?;
        Some((announcement, self.seen_at_ms))
    }
}

/// The memory and its journal, under one lock.
struct Persisted<O> {
    memory: AnnouncementMemory<O>,
    journal: Journal,
}

impl<O> Persisted<O>
where O: Owner
{
    /// Journal `changes`, then compact the journal when it has outgrown the memory.
    async fn journal_changes(&mut self, changes: Vec<MemoryChange<O>>) -> Result<(), StateError> {
        for change in changes {
            match change {
                MemoryChange::Keep {
                    owner,
                    announcement,
                    seen_at_ms,
                } => {
                    let record = MemoryRecord::of(&announcement, seen_at_ms);
                    self.journal.put(&owner.label(), &record).await?;
                }
                MemoryChange::Forget(owner) => self.journal.remove(&owner.label()).await?,
            }
        }
        let remembered = self.memory.remembered().count();
        if self.journal.needs_compaction(remembered) {
            let snapshot = self
                .memory
                .remembered()
                .map(|(owner, announcement, seen_at_ms)| {
                    (owner.label(), MemoryRecord::of(announcement, seen_at_ms))
                })
                .collect::<BTreeMap<_, _>>();
            self.journal.compact(snapshot).await?;
        }
        Ok(())
    }
}

/// The relay's front of a directory (see the module doc).
pub struct Recall<D>
where D: Directory
{
    directory: Arc<D>,
    persisted: Mutex<Persisted<D::Owner>>,
    budget: Semaphore,
}

impl<D> Recall<D>
where D: Directory
{
    /// Front `directory` with a memory of `windows` holding at most `capacity` owners, kept in
    /// `state_dir`, and at most `max_lookups` directory lookups in flight.
    pub async fn open(
        directory: Arc<D>,
        state_dir: &Path,
        windows: MemoryWindows,
        capacity: usize,
        max_lookups: usize,
    ) -> Result<Self, StateError> {
        ensure_dir(state_dir).await?;
        let (journal, records) =
            Journal::open::<String, MemoryRecord>(&state_dir.join(MEMORY_FILE)).await?;
        let remembered = records.into_iter().filter_map(|(label, record)| {
            let owner = D::Owner::from_label(&label)?;
            let (announcement, seen_at_ms) = record.into_parts()?;
            Some((owner, announcement, seen_at_ms))
        });
        let memory = AnnouncementMemory::restore(windows, capacity, remembered);
        Ok(Self {
            directory,
            persisted: Mutex::new(Persisted { memory, journal }),
            budget: Semaphore::new(max_lookups),
        })
    }
}

#[async_trait]
impl<D> Directory for Recall<D>
where D: Directory
{
    type Error = RecallError<D::Error>;
    type Owner = D::Owner;

    async fn announcement(&self, owner: Self::Owner) -> Result<Option<Announcement>, Self::Error> {
        let asked_at = now_ms();
        let recalled = self.persisted.lock().await.memory.recall(&owner, asked_at);
        if let Recalled::Fresh(answer) = recalled {
            return Ok(answer);
        }
        let Ok(_slot) = self.budget.try_acquire() else {
            let persisted = self.persisted.lock().await;
            return persisted
                .memory
                .graced(&owner, asked_at)
                .map(Some)
                .ok_or(RecallError::Busy);
        };
        let observed = self.directory.announcement(owner).await;
        let mut persisted = self.persisted.lock().await;
        let observation = persisted.memory.observe(owner, observed, now_ms());
        if let Err(error) = persisted.journal_changes(observation.changes).await {
            // The answer stands; the memory is journaled again on its next change or refresh.
            tracing::warn!("dns relay: memory not journaled: {error}");
        }
        observation.answer.map_err(RecallError::Directory)
    }
}

#[cfg(test)]
mod test_recall;
