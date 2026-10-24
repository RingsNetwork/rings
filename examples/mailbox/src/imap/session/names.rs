//! Names and flags between IMAP and the store: mailbox names, LIST patterns and attributes,
//! flags, and UID sets.

use std::num::NonZeroU32;

use imap_next::imap_types::core::Vec1;
use imap_next::imap_types::extensions::uidplus::UidElement;
use imap_next::imap_types::extensions::uidplus::UidSet;
use imap_next::imap_types::flag::Flag as ImapFlag;
use imap_next::imap_types::flag::FlagNameAttribute;
use imap_next::imap_types::mailbox::Mailbox as ImapMailbox;

use crate::store::Flag;
use crate::store::Listed;
use crate::store::SpecialUse;
use crate::store::INBOX;

/// The hierarchy delimiter of mailbox names.
pub const DELIMITER: char = '/';

/// The text of a mailbox name: `INBOX` for the inbox.
pub(super) fn mailbox_name(mailbox: &ImapMailbox<'_>) -> String {
    match mailbox {
        ImapMailbox::Inbox => INBOX.to_owned(),
        ImapMailbox::Other(other) => String::from_utf8_lossy(other.inner().as_ref()).into_owned(),
    }
}

/// `name` as an IMAP mailbox.
pub(super) fn imap_mailbox(name: &str) -> ImapMailbox<'static> {
    ImapMailbox::try_from(name.to_owned()).unwrap_or(ImapMailbox::Inbox)
}

/// The store flag an IMAP flag names.
pub(super) fn store_flag(flag: &ImapFlag<'_>) -> Option<Flag> {
    match flag {
        ImapFlag::Seen => Some(Flag::Seen),
        ImapFlag::Answered => Some(Flag::Answered),
        ImapFlag::Flagged => Some(Flag::Flagged),
        ImapFlag::Deleted => Some(Flag::Deleted),
        ImapFlag::Draft => Some(Flag::Draft),
        ImapFlag::Keyword(atom) => Some(Flag::Keyword(atom.inner().to_owned())),
        ImapFlag::Extension(_) => None,
    }
}

/// The system flags, as FLAGS and PERMANENTFLAGS list them.
pub(super) fn system_flags() -> Vec<ImapFlag<'static>> {
    vec![
        ImapFlag::Answered,
        ImapFlag::Flagged,
        ImapFlag::Deleted,
        ImapFlag::Seen,
        ImapFlag::Draft,
    ]
}

/// The UID set of `uids`.
pub(super) fn uid_set(uids: &[NonZeroU32]) -> Option<UidSet> {
    let elements = uids
        .iter()
        .copied()
        .map(UidElement::Single)
        .collect::<Vec<_>>();
    Vec1::try_from(elements).ok().map(UidSet)
}

/// The LIST attributes of `listed`.
pub(super) fn list_attributes(listed: &Listed) -> Vec<FlagNameAttribute<'static>> {
    let mut attributes = vec![attribute("HasNoChildren")];
    let special = listed.special_use.map(|special| match special {
        SpecialUse::Sent => "Sent",
        SpecialUse::Drafts => "Drafts",
        SpecialUse::Trash => "Trash",
        SpecialUse::Junk => "Junk",
        SpecialUse::Archive => "Archive",
    });
    attributes.extend(special.map(attribute));
    attributes
}

/// The mailbox attribute `\name`.
fn attribute(name: &'static str) -> FlagNameAttribute<'static> {
    imap_next::imap_types::core::Atom::try_from(name)
        .map_or(FlagNameAttribute::Noinferiors, FlagNameAttribute::from)
}

/// The IMAP flag of a store flag.
pub fn imap_flag(flag: &Flag) -> Option<ImapFlag<'static>> {
    Some(match flag {
        Flag::Seen => ImapFlag::Seen,
        Flag::Answered => ImapFlag::Answered,
        Flag::Flagged => ImapFlag::Flagged,
        Flag::Deleted => ImapFlag::Deleted,
        Flag::Draft => ImapFlag::Draft,
        Flag::Keyword(keyword) => {
            ImapFlag::Keyword(imap_next::imap_types::core::Atom::try_from(keyword.clone()).ok()?)
        }
    })
}

/// Whether the mailbox `name` matches the LIST pattern `pattern` (RFC 3501 §6.3.8): `*`
/// matches anything, `%` anything but the delimiter.
pub fn list_matches(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => {
            (0..=name.len()).any(|skip| list_matches(rest, name.get(skip..).unwrap_or_default()))
        }
        Some((b'%', rest)) => (0..=name.len())
            .take_while(|skip| {
                name.get(..*skip)
                    .is_some_and(|head| !head.contains(&(DELIMITER as u8)))
            })
            .any(|skip| list_matches(rest, name.get(skip..).unwrap_or_default())),
        Some((byte, rest)) => name
            .split_first()
            .is_some_and(|(first, tail)| first == byte && list_matches(rest, tail)),
    }
}
