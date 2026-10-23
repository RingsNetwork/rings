//! Messages of relay custody (see `dht::relay_holder`): a node passing the previous holder it
//! knows to its successor head, a recipient fetching its inbox from its holder, and a target
//! handing a refused hold back to its writer.

use serde::Deserialize;
use serde::Serialize;

use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::Did;

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

/// A target's refusal of a relay hold its view does not let it take, sent back to the writer
/// with the hold, so the writer keeps it and sends it again on its next repair pass instead of
/// the hold being dropped (`dht::relay_holder::is_unplaced_hold`).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RelayCarrierRefused {
    /// The refused operation, as the writer sent it.
    pub operation: PlacedEntryOperation,
}
