//! Messages of relay custody (see `dht::relay_holder`): a node passing the previous holder it
//! knows to its successor head, and a recipient fetching its inbox from its holder.

use serde::Deserialize;
use serde::Serialize;

use crate::dht::entry::Entry;
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
