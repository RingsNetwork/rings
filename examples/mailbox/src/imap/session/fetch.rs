//! FETCH (RFC 3501 §6.4.5): the data items of each message a client asks for.
//!
//! ```text
//!   items(m) = [render(name, m) | name ∈ names] ++ [UID | uid command]
//!   render(BODY[s]<o.n>, m) = bytes(s, m)[o .. o + n]
//!   a non-peek BODY[…], RFC822 or RFC822.TEXT sets \Seen first, and adds FLAGS
//! ```
//!
//! Only the items that need the message's bytes read them, and only the items that need its
//! structure parse it, so a client listing flags and UIDs never touches message files.

use std::collections::BTreeSet;
use std::num::NonZeroU32;

use imap_next::imap_types::core::IString;
use imap_next::imap_types::core::Literal;
use imap_next::imap_types::core::NString;
use imap_next::imap_types::core::Tag;
use imap_next::imap_types::core::Vec1;
use imap_next::imap_types::datetime::DateTime;
use imap_next::imap_types::extensions::binary::LiteralOrLiteral8;
use imap_next::imap_types::fetch::MacroOrMessageDataItemNames;
use imap_next::imap_types::fetch::MessageDataItem;
use imap_next::imap_types::fetch::MessageDataItemName;
use imap_next::imap_types::fetch::Section;
use imap_next::imap_types::flag::FlagFetch;
use imap_next::imap_types::response::Data;
use imap_next::imap_types::response::Response;
use imap_next::imap_types::sequence::SequenceSet;

use super::imap_flag;
use super::tagged;
use super::Failure;
use super::Session;
use crate::imap::mime;
use crate::store::Flag;
use crate::store::FlagChange;
use crate::store::Message;

/// The untagged FETCH reporting `flags` of message `seq`, with its `uid` when `with_uid`.
pub fn flags_response(
    seq: NonZeroU32,
    uid: NonZeroU32,
    flags: &BTreeSet<Flag>,
    with_uid: bool,
) -> Response<'static> {
    let mut items = vec![MessageDataItem::Flags(flag_list(flags))];
    if with_uid {
        items.push(MessageDataItem::Uid(uid));
    }
    let items = Vec1::try_from(items).unwrap_or_else(|_| Vec1::from(MessageDataItem::Uid(uid)));
    Response::Data(Data::Fetch { seq, items })
}

/// `flags` as a FETCH flag list.
fn flag_list(flags: &BTreeSet<Flag>) -> Vec<FlagFetch<'static>> {
    flags
        .iter()
        .filter_map(imap_flag)
        .map(FlagFetch::Flag)
        .collect()
}

/// The bytes of `data` as an IMAP literal. A literal cannot carry NUL (RFC 3501 §4.3), and a
/// message holding one breaks RFC 5322, so NULs are sent as spaces.
fn literal(data: Vec<u8>) -> NString<'static> {
    let data = if data.contains(&0) {
        data.into_iter()
            .map(|byte| if byte == 0 { b' ' } else { byte })
            .collect()
    } else {
        data
    };
    NString(Literal::try_from(data).ok().map(IString::Literal))
}

/// The part `<origin.length>` of `data`.
fn partial(data: Vec<u8>, range: Option<(u32, NonZeroU32)>) -> Vec<u8> {
    let Some((origin, length)) = range else {
        return data;
    };
    let start = usize::try_from(origin)
        .unwrap_or(usize::MAX)
        .min(data.len());
    let end = start
        .saturating_add(usize::try_from(length.get()).unwrap_or(usize::MAX))
        .min(data.len());
    data.get(start..end).map(<[u8]>::to_vec).unwrap_or_default()
}

/// Whether item `name` reads the message's bytes.
fn reads_bytes(name: &MessageDataItemName<'_>) -> bool {
    !matches!(
        name,
        MessageDataItemName::Flags
            | MessageDataItemName::Uid
            | MessageDataItemName::InternalDate
            | MessageDataItemName::Rfc822Size
    )
}

/// Whether item `name` marks the message `\Seen`.
fn marks_seen(name: &MessageDataItemName<'_>) -> bool {
    matches!(
        name,
        MessageDataItemName::BodyExt { peek: false, .. }
            | MessageDataItemName::Rfc822
            | MessageDataItemName::Rfc822Text
    )
}

/// The value of item `name` for `message`, whose bytes (when read) are `bytes` and whose
/// structure is `document`.
fn render(
    name: &MessageDataItemName<'static>,
    message: &Message,
    bytes: &[u8],
    document: &mime::Document,
) -> Option<MessageDataItem<'static>> {
    Some(match name {
        MessageDataItemName::Flags => MessageDataItem::Flags(flag_list(&message.flags)),
        MessageDataItemName::Uid => MessageDataItem::Uid(message.uid),
        MessageDataItemName::Rfc822Size => MessageDataItem::Rfc822Size(message.size),
        MessageDataItemName::InternalDate => {
            let date = chrono::DateTime::from_timestamp(message.internal_date, 0)?.fixed_offset();
            MessageDataItem::InternalDate(DateTime::try_from(date).ok()?)
        }
        MessageDataItemName::Envelope => MessageDataItem::Envelope(document.envelope()),
        MessageDataItemName::Body => MessageDataItem::Body(document.body_structure(bytes)),
        MessageDataItemName::BodyStructure => {
            MessageDataItem::BodyStructure(document.body_structure(bytes))
        }
        MessageDataItemName::Rfc822 => MessageDataItem::Rfc822(literal(bytes.to_vec())),
        MessageDataItemName::Rfc822Header => MessageDataItem::Rfc822Header(literal(
            document
                .section(bytes, &Section::Header(None))
                .unwrap_or_default(),
        )),
        MessageDataItemName::Rfc822Text => MessageDataItem::Rfc822Text(literal(
            document
                .section(bytes, &Section::Text(None))
                .unwrap_or_default(),
        )),
        MessageDataItemName::BodyExt {
            section,
            partial: range,
            ..
        } => {
            let data = match section {
                None => bytes.to_vec(),
                Some(section) => document.section(bytes, section).unwrap_or_default(),
            };
            MessageDataItem::BodyExt {
                section: section.clone(),
                origin: range.map(|(origin, _length)| origin),
                data: literal(partial(data, *range)),
            }
        }
        _ => return None,
    })
}

impl Session {
    /// FETCH, or UID FETCH, `names` of `set`.
    pub(super) async fn fetch(
        &mut self,
        tag: &Tag<'static>,
        set: &SequenceSet,
        names: MacroOrMessageDataItemNames<'static>,
        uid: bool,
    ) -> Result<Vec<Response<'static>>, Failure> {
        let view = self.selected()?.clone();
        let mut names = match names {
            MacroOrMessageDataItemNames::Macro(macro_) => macro_
                .expand()
                .into_iter()
                .map(|name| imap_next::imap_types::ToStatic::to_static(&name))
                .collect(),
            MacroOrMessageDataItemNames::MessageDataItemNames(names) => names,
        };
        if uid && !names.contains(&MessageDataItemName::Uid) {
            names.push(MessageDataItemName::Uid);
        }
        let targets = view.resolve(set, uid);
        let reading = names.iter().any(reads_bytes);
        if !view.read_only && names.iter().any(marks_seen) {
            let unseen = targets
                .iter()
                .map(|(_seq, uid)| *uid)
                .collect::<BTreeSet<_>>();
            let seen = BTreeSet::from([Flag::Seen]);
            let changed = self
                .store
                .change_flags(view.id, &unseen, FlagChange::Add, &seen)
                .await
                .map_err(Failure::store)?;
            if !changed.is_empty() && !names.contains(&MessageDataItemName::Flags) {
                names.push(MessageDataItemName::Flags);
            }
        }
        let mailbox = self
            .store
            .mailbox_by_id(view.id)
            .await
            .map_err(Failure::store)?;
        let mut responses = Vec::new();
        for (seq, target_uid) in targets {
            let Some(message) = mailbox.message(target_uid) else {
                continue;
            };
            let bytes = if reading {
                self.store
                    .read(view.id, target_uid)
                    .await
                    .map_err(Failure::store)?
            } else {
                Vec::new()
            };
            let document = mime::parse(&bytes);
            let items = names
                .iter()
                .filter_map(|name| render(name, message, &bytes, &document))
                .collect::<Vec<_>>();
            if names.contains(&MessageDataItemName::Flags) {
                if let Ok(view) = self.selected_mut() {
                    view.told_flags(target_uid, message.flags.clone());
                }
            }
            if let Ok(items) = Vec1::try_from(items) {
                responses.push(Response::Data(Data::Fetch { seq, items }));
            }
        }
        responses.extend(self.sync(false).await);
        responses.push(tagged(
            tag,
            imap_next::imap_types::response::StatusKind::Ok,
            None,
            "FETCH completed",
        ));
        Ok(responses)
    }
}

/// The bytes an APPEND carries.
pub fn appended(message: &LiteralOrLiteral8<'_>) -> Vec<u8> {
    match message {
        LiteralOrLiteral8::Literal(literal) => literal.data().to_vec(),
        LiteralOrLiteral8::Literal8(literal) => literal.data.to_vec(),
    }
}
