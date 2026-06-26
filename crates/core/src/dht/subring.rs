#![warn(missing_docs)]

use serde::Deserialize;
use serde::Serialize;

use super::entry::Entry;
use super::entry::EntryCrdt;
use super::entry::EntryKind;
use super::FingerTable;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoder;

/// A lightweight ring descriptor stored as an [`Entry`].
///
/// The entry key of a subring is the hash of its name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subring {
    /// name of subring
    pub name: String,
    /// finger table
    pub finger: FingerTable,
    /// creator
    pub creator: Did,
}

impl Subring {
    /// Create a new Subring
    pub fn new(name: &str, creator: Did) -> Result<Self> {
        let did = Entry::gen_did(name)?;
        Ok(Self {
            name: name.to_string(),
            finger: FingerTable::new(did, 1),
            creator,
        })
    }
}

impl TryFrom<Subring> for Entry {
    type Error = Error;
    fn try_from(ring: Subring) -> Result<Self> {
        let data = serde_json::to_string(&ring).map_err(|_| Error::SerializeToString)?;
        Ok(Self {
            did: Self::gen_did(&ring.name)?,
            data: vec![data.encode()?],
            kind: EntryKind::Subring,
            crdt: EntryCrdt::default(),
        })
    }
}

impl TryFrom<Entry> for Subring {
    type Error = Error;
    fn try_from(entry: Entry) -> Result<Self> {
        match &entry.kind {
            EntryKind::Subring => {
                let data = entry.data.first().ok_or_else(|| {
                    Error::InvalidMessage("subring entry has no encoded payload".to_string())
                })?;
                let decoded: String = data.decode()?;
                let subring: Subring =
                    serde_json::from_str(&decoded).map_err(Error::Deserialize)?;
                Ok(subring)
            }
            _ => Err(Error::InvalidEntryKind),
        }
    }
}
