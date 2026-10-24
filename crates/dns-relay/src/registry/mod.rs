//! The address registry: the address map with its book, kept in the relay's state directory.
//!
//! This is the effectful shell over the pure [`AddressPlan`] and [`AddressBook`]. It owns the
//! two files that must outlive a restart:
//!
//! - `address.key`: the address key. A new key would move every owner to a new address, and
//!   the addresses resolvers still cache would stop working.
//! - `address-book.jsonl`: the book, as a [`Journal`]. Without it, a connection to an address a
//!   resolver cached before the restart would find no owner until someone resolves that name
//!   again.
//!
//! One lock serializes binding and journaling, so the journal follows the book's changes in
//! their order.

use std::collections::BTreeMap;
use std::net::Ipv6Addr;
use std::path::Path;
use std::time::Duration;

use ipnet::Ipv6Net;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::address::AddressBook;
use crate::address::AddressCollision;
use crate::address::AddressPlan;
use crate::address::BookChange;
use crate::owner::Owner;
use crate::state::ensure_dir;
use crate::state::load_or_create_key;
use crate::state::Journal;
use crate::state::StateError;

/// File name of the address key in the state directory.
const KEY_FILE: &str = "address.key";
/// File name of the address book's journal in the state directory.
const BOOK_FILE: &str = "address-book.jsonl";

/// Why an owner got no address.
#[derive(Debug, Error)]
pub enum AddressError {
    /// Its address is bound to another owner.
    #[error(transparent)]
    Collision(#[from] AddressCollision),
    /// The book's change could not be journaled; the binding is kept in memory.
    #[error(transparent)]
    State(#[from] StateError),
}

/// One binding in the journal: the owner by its label, and its last answer time.
#[derive(Debug, Deserialize, Serialize)]
struct BookRecord {
    /// The owner's label.
    owner: String,
    /// When the relay last answered with the address, in milliseconds since the Unix epoch.
    answered_at_ms: u64,
}

/// The book and its journal, under one lock.
struct Persisted<O> {
    book: AddressBook<O>,
    journal: Journal,
}

/// The address map of a running relay, with its book kept on disk.
pub struct AddressRegistry<O> {
    plan: AddressPlan,
    persisted: Mutex<Persisted<O>>,
}

/// `duration` in whole milliseconds, saturating.
pub(crate) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl<O> AddressRegistry<O>
where O: Owner
{
    /// Open the registry of `prefix` in `state_dir`: read its key, or make and store a new one,
    /// and replay its book. The book keeps a binding for `retention` after its last answer and
    /// holds at most `capacity` bindings. A replayed binding whose label names no owner, or whose
    /// address is not the owner's under the plan, is dropped: the registry never made it.
    pub async fn open(
        prefix: Ipv6Net,
        state_dir: &Path,
        retention: Duration,
        capacity: usize,
    ) -> Result<Self, StateError> {
        ensure_dir(state_dir).await?;
        let key = load_or_create_key(&state_dir.join(KEY_FILE)).await?;
        let plan = AddressPlan::new(prefix, key);
        let (journal, records) =
            Journal::open::<Ipv6Addr, BookRecord>(&state_dir.join(BOOK_FILE)).await?;
        let bindings = records.into_iter().filter_map(|(address, record)| {
            let owner = O::from_label(&record.owner)?;
            (plan.address_of(&owner) == address).then_some((address, owner, record.answered_at_ms))
        });
        let book = AddressBook::restore(duration_ms(retention), capacity, bindings);
        Ok(Self {
            plan,
            persisted: Mutex::new(Persisted { book, journal }),
        })
    }

    /// The prefix the owners' addresses lie in.
    pub fn prefix(&self) -> Ipv6Net {
        self.plan.prefix()
    }

    /// `owner`'s address, bound in the book as answered at `now_ms`.
    ///
    /// Post: on `Ok(a)`, `a = addr(owner)` and `owner_at(a) = Some(owner)`.
    pub async fn address_for(&self, owner: O, now_ms: u64) -> Result<Ipv6Addr, AddressError> {
        let address = self.plan.address_of(&owner);
        let mut persisted = self.persisted.lock().await;
        let changes = persisted.book.bind(address, owner, now_ms)?;
        persisted.journal_changes(changes).await?;
        Ok(address)
    }

    /// The owner `address` is bound to, if any.
    pub async fn owner_at(&self, address: &Ipv6Addr) -> Option<O> {
        self.persisted.lock().await.book.owner_at(address)
    }
}

impl<O> Persisted<O>
where O: Owner
{
    /// Journal `changes`, then compact the journal when it has outgrown the book.
    async fn journal_changes(&mut self, changes: Vec<BookChange<O>>) -> Result<(), StateError> {
        for change in changes {
            match change {
                BookChange::Bind {
                    address,
                    owner,
                    answered_at_ms,
                } => {
                    let record = BookRecord {
                        owner: owner.label(),
                        answered_at_ms,
                    };
                    self.journal.put(&address, &record).await?;
                }
                BookChange::Unbind(address) => self.journal.remove(&address).await?,
            }
        }
        if self.journal.needs_compaction(self.book.len()) {
            let snapshot = self
                .book
                .bindings()
                .map(|(address, owner, answered_at_ms)| {
                    (address, BookRecord {
                        owner: owner.label(),
                        answered_at_ms,
                    })
                })
                .collect::<BTreeMap<_, _>>();
            self.journal.compact(snapshot).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod test_registry;
