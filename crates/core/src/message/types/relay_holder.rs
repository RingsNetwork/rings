//! Messages of relay custody (see `dht::relay_holder`): a node passing the previous holder it
//! knows to its successor head, a recipient fetching its inbox from its holder, a holder's
//! acknowledgement or a target's refusal of a hold, back to its writer, and a keeper asking a
//! key's owner for its holder.

use serde::Deserialize;
use serde::Serialize;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::Did;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;

/// A node passes to its successor head the previous holder it knows: itself when it holds relay
/// mail, otherwise the one its own predecessor passed it. Taken from the predecessor alone, the
/// record travels the storage-less run one link at a time, so the holder that ends the run places
/// the keys owned on it although their owners lie behind its predecessor.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AnnounceRelayHolder {
    /// The previous holder: the nearest node behind the receiver that holds relay mail.
    pub holder: Did,
    /// The previous holder's successor head, the first node of the run after it.
    pub run_start: Did,
}

/// A recipient asks the holder of its inbox key for its inbox. The authenticated origin names
/// the inbox, so a node fetches its own inbox and no other.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FetchRelayInbox;

/// A holder's answer to [`FetchRelayInbox`]: the live inbox it holds for the origin of the
/// request.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RelayInboxReport {
    /// The live carrier, or `None` when the holder keeps no mail for the recipient.
    pub inbox: Option<Entry>,
}

/// The digest of a relay hold as its writer sent it: Keccak-256 of the stamped operation's wire
/// encoding. A resend is byte-identical, so every send of one hold has one digest, and the
/// holder's acknowledgement and a target's refusal name the hold by it.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RelayHoldDigest([u8; 32]);

impl RelayHoldDigest {
    /// The digest of `hold`.
    pub(crate) fn of(hold: &EntryOperation) -> Result<Self> {
        let wire = rings_codec::serialize(hold).map_err(Error::CodecSerialize)?;
        Ok(Self(keccak256(&wire)))
    }
}

/// A holder's acknowledgement that it took the relay hold `hold`: the writer stops keeping it.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RelayCarrierHeld {
    /// The digest of the hold taken.
    pub hold: RelayHoldDigest,
}

/// A target's refusal of the relay hold `hold`, which its view does not let it take. The writer
/// keeps every hold until a holder acknowledges it, so the refusal only lets it send the hold
/// again sooner, to the holder its view names, when that is another node.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RelayCarrierRefused {
    /// The digest of the hold refused.
    pub hold: RelayHoldDigest,
}

/// A keeper whose view does not place the owner of the relay key `key` asks that owner for the
/// key's holder. The query is routed toward the owner as a lookup is, and the owner answers
/// (`RelayHolderFound`): it places every key of its own arc (`Law (Owner places)`).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FindRelayHolder {
    /// The relay key.
    pub key: Did,
}

/// The owner's answer to [`FindRelayHolder`]: the holder its view names for `key`, `None` for a
/// key without one. The keeper offers its carrier there; the holder takes it only if its own view
/// names it, so a wrong answer moves nothing.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RelayHolderFound {
    /// The relay key.
    pub key: Did,
    /// The holder the owner names.
    pub holder: Option<Did>,
}
