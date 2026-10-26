//! The store report of a reported entry operation (#866): the statement of the owner that
//! applied it at one of its placements, sent back to the operation's origin, so a publish can
//! learn that its owners hold it instead of polling for it.
//!
//! ```text
//!   origin ──OperateEntry { report: true }──▶ … ──▶ owner of placement p
//!   origin ◀──EntryStoreReport { operation, placement: p, outcome }── owner
//! ```
//!
//! Law (one report): a node reports an operation it received with `report` set from another
//! node at most once per receipt, and only where it applied or refused it as the owner of its
//! placement; a node that forwards an operation reports nothing. So a reported operation draws
//! at most one report per placement it reaches, and an unreported one draws none.
//!
//! A report is an observation, not a trust claim: an owner that lies shifts only its origin's
//! signal, never a reader's path, which reads the owners themselves.

use serde::Deserialize;
use serde::Serialize;

use crate::dht::entry::EntryOperation;
use crate::dht::Did;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;

/// The digest of a stamped entry operation as its origin sent it: Keccak-256 of its wire
/// encoding. Every placement of one operation carries the same stamped bytes, so the reports of
/// all its owners name the operation by one digest.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct OperationDigest([u8; 32]);

impl OperationDigest {
    /// The digest of `operation`.
    pub(crate) fn of(operation: &EntryOperation) -> Result<Self> {
        let wire = rings_codec::serialize(operation).map_err(Error::CodecSerialize)?;
        Ok(Self(keccak256(&wire)))
    }
}

/// What the owner of a placement did with a reported operation.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum StoreOutcome {
    /// The owner applied the operation at the placement.
    Stored,
    /// The owner refused the operation at the placement, as a write it does not take.
    Refused,
}

/// The owner's report of a reported operation at one placement, to the operation's origin (the
/// one-report law of this module).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct EntryStoreReport {
    /// The operation reported, by its digest.
    pub operation: OperationDigest,
    /// The placement the owner applied or refused it at.
    pub placement: Did,
    /// What the owner did.
    pub outcome: StoreOutcome,
}
