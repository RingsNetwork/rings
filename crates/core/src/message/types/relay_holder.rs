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

/// A node passes to its successor head the previous holders it knows, nearest first: itself
/// followed by the nearer ones its predecessor passed it when it holds relay mail, otherwise the
/// ones its own predecessor passed it, at most `r` (#777). Taken from the
/// predecessor alone, the record travels each storage-less run one link at a time, so every
/// holder places the keys owned on the runs behind its previous holders, and decides on its own
/// view whether it is a member of a key (`dht::relay_holder`, `Law (Members)`).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AnnounceRelayHolder {
    /// The previous holders, nearest first.
    pub previous: Vec<PreviousRelayHolder>,
}

/// One previous holder of an [`AnnounceRelayHolder`] record.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct PreviousRelayHolder {
    /// A node behind the receiver that holds relay mail.
    pub holder: Did,
    /// Its successor head, the first node of the run after it.
    pub run_start: Did,
}

/// A member forwards a hold it took to its next member, byte for byte as its writer stamped it
/// (#777, `Law (Chain replication)`). The next member takes it iff its own view makes the sender
/// its previous member, forwards it on unless it ends the chain, and acknowledges it to the sender
/// with [`RelayCarrierHeld`] once the rest of the chain did; the holder acknowledges the writer
/// last. A forward sent again, on the writer's resend, carries the digest alone: a member that
/// does not know the hold refuses it ([`RelayCarrierRefused`]), and the sender then sends the
/// bytes.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ReplicateRelayHold {
    /// The digest of the hold.
    pub digest: RelayHoldDigest,
    /// The hold as its writer stamped it, or `None` on a forward sent again.
    pub hold: Option<EntryOperation>,
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

/// An acknowledgement that the relay hold `hold` is held down the chain: from a member to the
/// member it came from once the rest of the chain holds it, and from the holder to the writer
/// last (`Law (Chain replication)`), so the writer stops keeping it.
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
/// names it, so a wrong answer moves nothing. With no holder, the owner names the last node it
/// places, `horizon`, so a member looking for its next member asks again past it (#777).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RelayHolderFound {
    /// The relay key.
    pub key: Did,
    /// The holder the owner names.
    pub holder: Option<Did>,
    /// The last node the owner places, when it names no holder.
    pub horizon: Option<Did>,
}

/// One carrier in a [`RelayCarrierDigests`]: its key and the digest of its stored state.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct RelayCarrierDigest {
    /// The carrier's key.
    pub key: Did,
    /// Keccak-256 of the carrier's stored state.
    pub digest: [u8; 32],
}

impl RelayCarrierDigest {
    /// The digest of `carrier`, stored at `key`.
    pub(crate) fn of(key: Did, carrier: &Entry) -> Result<Self> {
        let wire = rings_codec::serialize(carrier).map_err(Error::CodecSerialize)?;
        Ok(Self {
            key,
            digest: keccak256(&wire),
        })
    }
}

/// A member's repair offer to one chain neighbour (#777, `Law (Chain repair)`): the digest of
/// every carrier it keeps whose chain includes that neighbour, in one message per neighbour and
/// pass. The neighbour asks for the carriers it lacks or holds otherwise
/// ([`RelayCarriersWanted`]).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RelayCarrierDigests {
    /// The carriers offered.
    pub carriers: Vec<RelayCarrierDigest>,
}

/// A chain neighbour's answer to [`RelayCarrierDigests`]: the keys whose carrier it lacks or
/// holds in another state. The offering member copies those carriers to it.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RelayCarriersWanted {
    /// The keys wanted.
    pub keys: Vec<Did>,
}

/// A node's declaration of relay custody, sent to every peer it is connected to when it
/// changes after the handshake: once the node has been online for the age gate, it holds relay
/// mail (#777, `Law (Age gate)`). A peer that connects later reads it in the handshake.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DeclareRelayCustody {
    /// The declaration.
    pub custody: crate::dht::RelayCustody,
}
