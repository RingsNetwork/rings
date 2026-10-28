//! The recipient's opening of its mail topic (pure).
//!
//! ```text
//!   by_digest  = { H(e) ↦ e | e ∈ elements }
//!   manifests  = { M | e ∈ elements decodes to M ∧ 1 ≤ |M.chunks| ≤ N_max }
//!   open(M)    = Incomplete             some H(chunk_i) of M names no chunk element
//!              | ⊥ (forged)             H(chunk_0 ‖ … ‖ chunk_{n−1}) ≠ M.mail_id
//!              | Unreadable             the recipient's key does not open c
//!              | Opened(m)
//!   opening(x) = max { open(M) | M ∈ manifests, M.mail_id = x }
//!                under Incomplete < Unreadable < Opened
//! ```
//!
//! Laws:
//! - **Integrity.** A chunk is read only through a manifest that lists its element's digest, in
//!   that manifest's order, and the mail opens only if the listed chunks concatenate to the
//!   preimage of its `mail_id`: a forged chunk, or a manifest listing one, never alters a mail.
//! - **All or nothing.** The AEAD tag covers all of `c`: a missing chunk leaves the mail
//!   `Incomplete`, and an altered one is a different element, so its digest is not listed.
//! - **Content address.** A manifest whose chunks do not concatenate to `H⁻¹(mail_id)` is
//!   forged and ignored: no forger can make a mail `Unreadable` and so stop it being retried.
//! - **Join.** The outcome per `mail_id` is the maximum over its manifests, so the result does
//!   not depend on the order of elements, and a forged manifest never hides a genuine one.
//! - **Round trip.** `seal(m, (d, ek))` opens to `Opened(m)` under `ek`'s secret.

use std::collections::BTreeMap;

use bytes::Bytes;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::elgamal::impls::secp256k1::AeadCiphertext;

use super::envelope::mail_aad;
use super::envelope::MailDigest;
use super::envelope::MailElement;
use super::envelope::MailManifest;

/// A mail opened from its recipient's topic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenedMail {
    /// `H(c)` of the ciphertext it was opened from.
    pub mail_id: MailDigest,
    /// The message, as the gateway received it with its trace headers.
    pub message: Vec<u8>,
}

impl OpenedMail {
    /// The message's `Message-ID` (RFC 5322 §3.6.4), unfolded and trimmed, if its header has
    /// one: what the recipient deduplicates by.
    pub fn message_id(&self) -> Option<String> {
        header_field(&self.message, "message-id")
    }
}

/// What one mail id in a topic opens to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Opening {
    /// A manifest lists a chunk the topic does not hold: the mail is not filed, and is
    /// tried again on the next read.
    Incomplete {
        /// The mail's id.
        mail_id: MailDigest,
    },
    /// The whole ciphertext is present but the key does not open it: it is remembered and not
    /// tried again.
    Unreadable {
        /// The mail's id.
        mail_id: MailDigest,
    },
    /// The mail.
    Opened(OpenedMail),
}

impl Opening {
    /// The id of the mail this outcome is for.
    pub fn mail_id(&self) -> MailDigest {
        match self {
            Self::Incomplete { mail_id } | Self::Unreadable { mail_id } => *mail_id,
            Self::Opened(mail) => mail.mail_id,
        }
    }

    /// This outcome's rank in `Incomplete < Unreadable < Opened`.
    fn rank(&self) -> u8 {
        match self {
            Self::Incomplete { .. } => 0,
            Self::Unreadable { .. } => 1,
            Self::Opened(_) => 2,
        }
    }

    /// The join of two outcomes for one mail id: the higher-ranked, `self` on a tie.
    fn join(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// Open the mail in `elements` for `recipient` with the secret of `key`, skipping every mail id
/// `fresh` rejects: one outcome per mail id with a well-formed manifest whose chunks are not
/// forged (the laws of this module).
pub fn open_mail(
    elements: &[Bytes],
    recipient: Did,
    key: &DelegateeKey,
    fresh: impl Fn(&MailDigest) -> bool,
) -> Vec<Opening> {
    let by_digest = elements
        .iter()
        .map(|element| (MailDigest::of(element), element))
        .collect::<BTreeMap<_, _>>();
    let manifests = elements
        .iter()
        .filter_map(|element| match MailElement::decode(element) {
            Ok(MailElement::Manifest(manifest)) => Some(manifest),
            Ok(MailElement::Chunk(_)) | Err(_) => None,
        });
    let mut outcomes = BTreeMap::<MailDigest, Opening>::new();
    for manifest in manifests {
        if !fresh(&manifest.mail_id) || !manifest.is_well_formed() {
            continue;
        }
        let Some(outcome) = open_manifest(&manifest, &by_digest, recipient, key) else {
            continue;
        };
        let joined = match outcomes.remove(&outcome.mail_id()) {
            Some(earlier) => earlier.join(outcome),
            None => outcome,
        };
        outcomes.insert(joined.mail_id(), joined);
    }
    outcomes.into_values().collect()
}

/// What the well-formed `manifest` opens to over the elements `by_digest`, or `None` when it is
/// forged (the content-address law).
fn open_manifest(
    manifest: &MailManifest,
    by_digest: &BTreeMap<MailDigest, &Bytes>,
    recipient: Did,
    key: &DelegateeKey,
) -> Option<Opening> {
    let mail_id = manifest.mail_id;
    let Some(ciphertext) = assemble(manifest, by_digest) else {
        return Some(Opening::Incomplete { mail_id });
    };
    if MailDigest::of(&ciphertext) != mail_id {
        return None;
    }
    let aad = mail_aad(recipient);
    let message = rings_codec::deserialize::<AeadCiphertext>(&ciphertext)
        .ok()
        .and_then(|sealed| key.decrypt_elgamal_aead(&sealed, &aad).ok());
    Some(match message {
        Some(message) => Opening::Opened(OpenedMail { mail_id, message }),
        None => Opening::Unreadable { mail_id },
    })
}

/// The ciphertext the chunks `manifest` lists concatenate to, in its order, or `None` when one
/// is missing or is not a chunk element.
fn assemble(manifest: &MailManifest, by_digest: &BTreeMap<MailDigest, &Bytes>) -> Option<Vec<u8>> {
    manifest
        .chunks
        .iter()
        .try_fold(Vec::new(), |mut ciphertext, digest| {
            let MailElement::Chunk(slice) = MailElement::decode(by_digest.get(digest)?).ok()?
            else {
                return None;
            };
            ciphertext.extend_from_slice(&slice);
            Some(ciphertext)
        })
}

/// The value of the first header field `name` (lower case) of `message`, unfolded (RFC 5322
/// §2.2.3) and trimmed; the header ends at the first empty line.
fn header_field(message: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(message);
    let mut value: Option<String> = None;
    for line in text.split('\n').map(|line| line.trim_end_matches('\r')) {
        if line.is_empty() {
            break;
        }
        let folded = line.starts_with([' ', '\t']);
        match (value.as_mut(), folded) {
            (Some(value), true) => value.push_str(line),
            (Some(_), false) => break,
            (None, true) => {}
            (None, false) => {
                value = line
                    .split_once(':')
                    .filter(|(field, _)| field.trim().eq_ignore_ascii_case(name))
                    .map(|(_, rest)| rest.to_owned());
            }
        }
    }
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}
