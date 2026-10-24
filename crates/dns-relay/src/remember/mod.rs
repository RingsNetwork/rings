//! A directory that remembers: the shell over [`AnnouncementMemory`], kept in the relay's state
//! directory as `announcements.json`.
//!
//! [`Remembering`] wraps the node's directory and is a directory itself, so the DNS authority
//! and the ingress read through it without knowing it exists. Each lookup asks the inner
//! directory, folds the answer into the memory, and answers what the memory says; a change worth
//! keeping is written before the answer returns, under the memory's lock, so the file always
//! holds a memory the relay held.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::announcement::Announcement;
use crate::announcement::ServiceName;
use crate::announcement::SubName;
use crate::announcement::TxtValue;
use crate::capability::Directory;
use crate::capability::LookupFailure;
use crate::memory::AnnouncementMemory;
use crate::owner::Owner;
use crate::registry::duration_ms;
use crate::state::ensure_dir;
use crate::state::now_ms;
use crate::state::read_json;
use crate::state::write_json;
use crate::state::StateError;

/// File name of the remembered announcements in the state directory.
const MEMORY_FILE: &str = "announcements.json";

/// The memory's file format: one entry per remembered owner.
#[derive(Debug, Default, Deserialize, Serialize)]
struct MemoryFile {
    /// The remembered announcements.
    owners: Vec<MemoryEntry>,
}

/// One remembered announcement in the memory file.
#[derive(Debug, Deserialize, Serialize)]
struct MemoryEntry {
    /// The owner's label.
    owner: String,
    /// The ports it serves, with their services.
    ports: Vec<(u16, String)>,
    /// Its TXT records, by name under the owner.
    txt: Vec<(String, Vec<String>)>,
    /// When the relay last saw the announcement, in milliseconds since the Unix epoch.
    seen_at_ms: u64,
}

impl MemoryEntry {
    /// The file form of `owner`'s remembered `announcement`, seen at `seen_at_ms`.
    fn of<O>(owner: &O, announcement: &Announcement, seen_at_ms: u64) -> Self
    where O: Owner {
        Self {
            owner: owner.label(),
            ports: announcement
                .served_ports()
                .map(|(port, service)| (port, service.as_str().to_owned()))
                .collect(),
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

    /// The owner, announcement and sighting this entry holds, or `None` when it does not
    /// validate: the relay never answers from a malformed file.
    fn into_parts<O>(self) -> Option<(O, Announcement, u64)>
    where O: Owner {
        let owner = O::from_label(&self.owner)?;
        let ports = self
            .ports
            .into_iter()
            .map(|(port, service)| ServiceName::new(service).map(|service| (port, service)))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
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
        let announcement = Announcement::new(ports, txt).ok()?;
        Some((owner, announcement, self.seen_at_ms))
    }
}

impl MemoryFile {
    /// The file form of `memory`.
    fn of<O>(memory: &AnnouncementMemory<O>) -> Self
    where O: Owner {
        Self {
            owners: memory
                .entries()
                .map(|(owner, remembered)| {
                    MemoryEntry::of(owner, &remembered.announcement, remembered.seen_at_ms)
                })
                .collect(),
        }
    }
}

/// A directory remembering what owners announced, for a grace period after they vanish.
pub struct Remembering<D>
where D: Directory
{
    directory: Arc<D>,
    memory: Mutex<AnnouncementMemory<D::Owner>>,
    path: PathBuf,
}

impl<D> Remembering<D>
where D: Directory
{
    /// Remember `directory`'s announcements for `grace`, keeping the memory in `state_dir`.
    pub async fn open(
        directory: Arc<D>,
        state_dir: &Path,
        grace: Duration,
    ) -> Result<Self, StateError> {
        ensure_dir(state_dir).await?;
        let path = state_dir.join(MEMORY_FILE);
        let grace_ms = duration_ms(grace);
        let entries = read_json::<MemoryFile>(&path)
            .await?
            .map(|file| file.owners)
            .unwrap_or_default();
        let memory = AnnouncementMemory::from_entries(
            grace_ms,
            entries.into_iter().filter_map(MemoryEntry::into_parts),
        );
        Ok(Self {
            directory,
            memory: Mutex::new(memory),
            path,
        })
    }
}

#[async_trait]
impl<D> Directory for Remembering<D>
where D: Directory
{
    type Owner = D::Owner;

    async fn announcement(
        &self,
        owner: Self::Owner,
    ) -> Result<Option<Announcement>, LookupFailure> {
        let observed = self.directory.announcement(owner).await;
        let now = now_ms();
        let mut memory = self.memory.lock().await;
        let observation = memory.observe(owner, observed, now);
        if observation.changed {
            memory.forget_expired(now);
            match write_json(&self.path, &MemoryFile::of(&memory)).await {
                Ok(()) => memory.mark_persisted(),
                // The answer stands; the next change writes the memory again.
                Err(error) => tracing::warn!("dns relay: memory not saved: {error}"),
            }
        }
        observation.answer
    }
}

#[cfg(test)]
mod test_remember;
