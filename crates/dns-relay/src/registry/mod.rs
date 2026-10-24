//! The address registry: the address map with its book, kept in the relay's state directory.
//!
//! This is the effectful shell over the pure [`AddressPlan`] and [`AddressBook`]. It owns the
//! two files that must outlive a restart:
//!
//! - `address.key`: the address key. A new key would move every owner to a new address, and
//!   the addresses resolvers still cache would stop working.
//! - `address-book.json`: the book. Without it, a connection to an address a resolver cached
//!   before the restart would find no owner until someone resolves that name again.
//!
//! The book is written when it grows or shrinks, never on a renewal, so its writes follow new
//! owners, not queries. One lock serializes binding and writing, so the file always holds a
//! book the registry held.

use std::net::Ipv6Addr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use ipnet::Ipv6Net;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::address::AddressBook;
use crate::address::AddressCollision;
use crate::address::AddressPlan;
use crate::address::Binding;
use crate::address::Bound;
use crate::owner::Owner;
use crate::state::ensure_dir;
use crate::state::load_or_create_key;
use crate::state::read_json;
use crate::state::write_json;
use crate::state::StateError;

/// File name of the address key in the state directory.
const KEY_FILE: &str = "address.key";
/// File name of the address book in the state directory.
const BOOK_FILE: &str = "address-book.json";

/// Why an owner got no address.
#[derive(Debug, Error)]
pub enum AddressError {
    /// Its address is bound to another owner.
    #[error(transparent)]
    Collision(#[from] AddressCollision),
    /// The grown book could not be written; the binding is kept in memory.
    #[error(transparent)]
    State(#[from] StateError),
}

/// The book's file format: one entry per binding, the owner by its label.
#[derive(Debug, Default, Deserialize, Serialize)]
struct BookFile {
    /// The bindings.
    bindings: Vec<BookEntry>,
}

/// One binding in the book file.
#[derive(Debug, Deserialize, Serialize)]
struct BookEntry {
    /// The bound address.
    address: Ipv6Addr,
    /// The owner's label.
    owner: String,
    /// When the relay last answered with the address, in milliseconds since the Unix epoch.
    answered_at_ms: u64,
}

impl BookFile {
    /// The file form of `book`.
    fn of<O>(book: &AddressBook<O>) -> Self
    where O: Owner {
        Self {
            bindings: book
                .bindings()
                .map(|(address, binding)| BookEntry {
                    address: *address,
                    owner: binding.owner.label(),
                    answered_at_ms: binding.answered_at_ms,
                })
                .collect(),
        }
    }

    /// The book this file holds. An entry whose label names no owner, or whose address is not
    /// the owner's under `plan`, is dropped: it cannot be a binding the registry made.
    fn into_book<O>(self, plan: &AddressPlan) -> AddressBook<O>
    where O: Owner {
        AddressBook::from_bindings(self.bindings.into_iter().filter_map(|entry| {
            let owner = O::from_label(&entry.owner)?;
            (plan.address_of(&owner) == entry.address).then_some((entry.address, Binding {
                owner,
                answered_at_ms: entry.answered_at_ms,
            }))
        }))
    }
}

/// The address map of a running relay, with its book kept on disk.
pub struct AddressRegistry<O> {
    plan: AddressPlan,
    book: Mutex<AddressBook<O>>,
    book_path: PathBuf,
    retention: Duration,
}

impl<O> AddressRegistry<O>
where O: Owner
{
    /// Open the registry of `prefix` in `state_dir`: read its key, or make and store a new one,
    /// and read its book, if any. A binding unanswered for `retention` is forgotten when the
    /// book next grows.
    pub async fn open(
        prefix: Ipv6Net,
        state_dir: &Path,
        retention: Duration,
    ) -> Result<Self, StateError> {
        ensure_dir(state_dir).await?;
        let key = load_or_create_key(&state_dir.join(KEY_FILE)).await?;
        let plan = AddressPlan::new(prefix, key);
        let book_path = state_dir.join(BOOK_FILE);
        let book = read_json::<BookFile>(&book_path)
            .await?
            .map_or_else(AddressBook::default, |file| file.into_book(&plan));
        Ok(Self {
            plan,
            book: Mutex::new(book),
            book_path,
            retention,
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
        let mut book = self.book.lock().await;
        if book.bind(address, owner, now_ms)? == Bound::Fresh {
            let horizon = now_ms.saturating_sub(duration_ms(self.retention));
            book.forget_answered_before(horizon);
            write_json(&self.book_path, &BookFile::of(&book)).await?;
        }
        Ok(address)
    }

    /// The owner `address` is bound to, if any.
    pub async fn owner_at(&self, address: &Ipv6Addr) -> Option<O> {
        self.book.lock().await.owner_at(address)
    }
}

/// `duration` in whole milliseconds, saturating.
pub(crate) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod test_registry;
