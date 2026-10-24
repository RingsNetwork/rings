//! A message's MIME structure as IMAP sees it: its entities positioned in the message bytes,
//! and the views FETCH returns of them (RFC 3501 §6.4.5, §7.4.2).
//!
//! `mail-parser` reads the message; this module keeps only what IMAP addresses: each entity's
//! header and body ranges, its media type, parameters, id, description and transfer encoding,
//! and, for an encapsulated `message/rfc822`, the inner message.
//!
//! ```text
//!   part numbers:  a single-part message has only part 1 (its body);
//!                  a multipart's parts are 1, 2, …;  a message/rfc822 part n's parts are n.1, …
//!   child(e, n)  = children(e)[n − 1]              e multipart
//!                = top-child(inner(e), n)          e message/rfc822
//!   top-child(m, n) = children(top m)[n − 1]       top m multipart
//!                   = top m                        n = 1, top m single-part
//! ```

use std::num::NonZeroU32;
use std::ops::Range;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use imap_next::imap_types::body::BasicFields;
use imap_next::imap_types::body::Body;
use imap_next::imap_types::body::BodyStructure;
use imap_next::imap_types::body::SpecificFields;
use imap_next::imap_types::core::IString;
use imap_next::imap_types::core::NString;
use imap_next::imap_types::core::Vec1;
use imap_next::imap_types::envelope::Address;
use imap_next::imap_types::envelope::Envelope;
use imap_next::imap_types::fetch::Section;
use mail_parser::MessageParser;
use mail_parser::MimeHeaders;
use mail_parser::PartType;

/// One MIME entity, positioned in the bytes of the outermost message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entity {
    /// Its header, including the empty line that ends it.
    pub header: Range<usize>,
    /// Its body, as transferred (still encoded).
    pub body: Range<usize>,
    /// Its media type, lowercase.
    pub media_type: String,
    /// Its media subtype, lowercase.
    pub subtype: String,
    /// Its content-type parameters.
    pub params: Vec<(String, String)>,
    /// Its Content-ID.
    pub id: Option<String>,
    /// Its Content-Description.
    pub description: Option<String>,
    /// Its Content-Transfer-Encoding, uppercase; `7BIT` when absent.
    pub encoding: String,
    /// What it contains.
    pub content: Content,
}

/// What an entity contains.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Content {
    /// Data: text, an image, an attachment.
    Leaf,
    /// Further entities.
    Multipart(Vec<Entity>),
    /// An encapsulated message.
    Message(Box<Document>),
}

/// A message: its header, body and top entity, and the fields of its envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    /// The header, including the empty line that ends it.
    pub header: Range<usize>,
    /// The body.
    pub body: Range<usize>,
    /// The top entity: the message itself as a MIME entity.
    pub top: Entity,
    /// The envelope.
    pub envelope: EnvelopeFields,
}

/// The envelope fields of a message, raw as its header holds them (encoded-words kept).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvelopeFields {
    /// Date.
    pub date: Option<String>,
    /// Subject.
    pub subject: Option<String>,
    /// From.
    pub from: Vec<Mailbox>,
    /// Sender.
    pub sender: Vec<Mailbox>,
    /// Reply-To.
    pub reply_to: Vec<Mailbox>,
    /// To.
    pub to: Vec<Mailbox>,
    /// Cc.
    pub cc: Vec<Mailbox>,
    /// Bcc.
    pub bcc: Vec<Mailbox>,
    /// In-Reply-To.
    pub in_reply_to: Option<String>,
    /// Message-ID.
    pub message_id: Option<String>,
}

/// One address of an envelope.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Mailbox {
    /// The display name.
    pub name: Option<String>,
    /// The local part.
    pub local: Option<String>,
    /// The domain.
    pub domain: Option<String>,
}

/// Parse `bytes` as a message. Unparsable input is one text/plain entity, so every stored
/// message has a structure.
pub fn parse(bytes: &[u8]) -> Document {
    MessageParser::default()
        .parse(bytes)
        .map_or_else(|| opaque(bytes.len()), |message| document(&message, 0))
}

/// The structure of unparsable input of `len` bytes: no header, one text/plain body.
fn opaque(len: usize) -> Document {
    Document {
        header: 0..0,
        body: 0..len,
        top: Entity {
            header: 0..0,
            body: 0..len,
            media_type: "text".to_owned(),
            subtype: "plain".to_owned(),
            params: Vec::new(),
            id: None,
            description: None,
            encoding: "7BIT".to_owned(),
            content: Content::Leaf,
        },
        envelope: EnvelopeFields::default(),
    }
}

/// The document of `message`, whose offsets count from `base` of the outermost message.
fn document(message: &mail_parser::Message<'_>, base: usize) -> Document {
    let top = entity(message, 0, base);
    let header = top.header.start..top.body.start;
    let body = top.body.clone();
    Document {
        header,
        body,
        envelope: envelope_fields(message),
        top,
    }
}

/// The entity of `message`'s part `id`, positioned at `base`.
fn entity(message: &mail_parser::Message<'_>, id: u32, base: usize) -> Entity {
    let Some(part) = message.part(id) else {
        return opaque(0).top;
    };
    let at = |offset: u32| base.saturating_add(usize::try_from(offset).unwrap_or(usize::MAX));
    let header = at(part.offset_header)..at(part.offset_body);
    let body = at(part.offset_body)..at(part.offset_end);
    let (media_type, subtype, params) = part.content_type().map_or_else(
        || ("text".to_owned(), "plain".to_owned(), Vec::new()),
        |content_type| {
            (
                content_type.c_type.to_ascii_lowercase(),
                content_type
                    .c_subtype
                    .as_deref()
                    .unwrap_or("plain")
                    .to_ascii_lowercase(),
                content_type
                    .attributes
                    .iter()
                    .flatten()
                    .map(|attribute| (attribute.name.to_string(), attribute.value.to_string()))
                    .collect(),
            )
        },
    );
    let content = match &part.body {
        PartType::Multipart(children) => Content::Multipart(
            children
                .iter()
                .map(|child| entity(message, *child, base))
                .collect(),
        ),
        // `mail-parser` parses an encapsulated message in the outer message's own bytes, so its
        // offsets count from the same origin. An encoded message/rfc822 part was decoded into
        // new bytes instead, where no position is ours: it stays opaque.
        PartType::Message(inner) if same_bytes(&inner.raw_message, &message.raw_message) => {
            Content::Message(Box::new(document(inner, base)))
        }
        _ => Content::Leaf,
    };
    Entity {
        header,
        body,
        media_type,
        subtype,
        params,
        id: part.content_id().map(str::to_owned),
        description: part.content_description().map(str::to_owned),
        encoding: part
            .content_transfer_encoding()
            .map_or_else(|| "7BIT".to_owned(), str::to_ascii_uppercase),
        content,
    }
}

/// Whether `a` and `b` are the same bytes in memory, not merely equal ones.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    std::ptr::eq(a, b)
}

/// The envelope fields of `message`.
fn envelope_fields(message: &mail_parser::Message<'_>) -> EnvelopeFields {
    let raw = |name: mail_parser::HeaderName<'static>| message.header_raw(name).map(unfold);
    let mailboxes = |address: Option<&mail_parser::Address<'_>>| {
        address.map_or_else(Vec::new, |address| {
            address
                .iter()
                .map(|addr| {
                    let (local, domain) = addr
                        .address
                        .as_deref()
                        .and_then(|address| address.rsplit_once('@'))
                        .map_or(
                            (addr.address.as_deref().map(str::to_owned), None),
                            |(l, d)| (Some(l.to_owned()), Some(d.to_owned())),
                        );
                    Mailbox {
                        name: addr.name.as_deref().map(encode_word),
                        local,
                        domain,
                    }
                })
                .collect()
        })
    };
    let from = mailboxes(message.from());
    let sender = match mailboxes(message.sender()) {
        sender if sender.is_empty() => from.clone(),
        sender => sender,
    };
    let reply_to = match mailboxes(message.reply_to()) {
        reply_to if reply_to.is_empty() => from.clone(),
        reply_to => reply_to,
    };
    EnvelopeFields {
        date: raw(mail_parser::HeaderName::Date),
        subject: raw(mail_parser::HeaderName::Subject),
        to: mailboxes(message.to()),
        cc: mailboxes(message.cc()),
        bcc: mailboxes(message.bcc()),
        in_reply_to: raw(mail_parser::HeaderName::InReplyTo),
        message_id: raw(mail_parser::HeaderName::MessageId),
        from,
        sender,
        reply_to,
    }
}

/// A raw header value unfolded and trimmed: folding line breaks become nothing.
fn unfold(value: &str) -> String {
    value
        .replace("\r\n", "")
        .replace('\n', "")
        .trim()
        .to_owned()
}

/// `text` as an envelope may carry it: as is when it is printable ASCII, else one RFC 2047
/// encoded-word in UTF-8.
fn encode_word(text: &str) -> String {
    if text.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
        return text.to_owned();
    }
    format!("=?utf-8?b?{}?=", BASE64.encode(text.as_bytes()))
}

impl Document {
    /// The entity with part number `path` (the module doc's numbering).
    pub fn entity(&self, path: &[NonZeroU32]) -> Option<&Entity> {
        let (first, rest) = path.split_first()?;
        let start = top_child(&self.top, *first)?;
        rest.iter().try_fold(start, |entity, n| child(entity, *n))
    }

    /// The bytes `section` names in this message's `bytes`; `None` when the section names
    /// nothing.
    pub fn section(&self, bytes: &[u8], section: &Section<'_>) -> Option<Vec<u8>> {
        let slice = |range: &Range<usize>| bytes.get(range.clone()).map(<[u8]>::to_vec);
        let inner = |part: &Option<imap_next::imap_types::fetch::Part>| match part {
            None => Some(self),
            Some(part) => match &self.entity(part.0.as_ref())?.content {
                Content::Message(inner) => Some(inner.as_ref()),
                _ => None,
            },
        };
        match section {
            Section::Part(part) => slice(&self.entity(part.0.as_ref())?.body),
            Section::Mime(part) => slice(&self.entity(part.0.as_ref())?.header),
            Section::Header(part) => slice(&inner(part)?.header),
            Section::Text(part) => slice(&inner(part)?.body),
            Section::HeaderFields(part, names) => {
                let header = bytes.get(inner(part)?.header.clone())?;
                Some(header_fields(header, |name| {
                    contains_name(names.as_ref(), name)
                }))
            }
            Section::HeaderFieldsNot(part, names) => {
                let header = bytes.get(inner(part)?.header.clone())?;
                Some(header_fields(header, |name| {
                    !contains_name(names.as_ref(), name)
                }))
            }
        }
    }

    /// The message's ENVELOPE.
    pub fn envelope(&self) -> Envelope<'static> {
        envelope(&self.envelope)
    }

    /// The message's BODYSTRUCTURE, measured in `bytes`.
    pub fn body_structure(&self, bytes: &[u8]) -> BodyStructure<'static> {
        body_structure(&self.top, bytes)
    }
}

/// The child numbered `n` of `entity`.
fn child(entity: &Entity, n: NonZeroU32) -> Option<&Entity> {
    match &entity.content {
        Content::Multipart(children) => nth(children, n),
        Content::Message(inner) => top_child(&inner.top, n),
        Content::Leaf => None,
    }
}

/// The part numbered `n` of a message whose top entity is `top`.
fn top_child(top: &Entity, n: NonZeroU32) -> Option<&Entity> {
    match &top.content {
        Content::Multipart(children) => nth(children, n),
        _ => (n == NonZeroU32::MIN).then_some(top),
    }
}

/// The `n`th of `entities`, counting from 1.
fn nth(entities: &[Entity], n: NonZeroU32) -> Option<&Entity> {
    usize::try_from(n.get() - 1)
        .ok()
        .and_then(|index| entities.get(index))
}

/// Whether `names` holds `name`, ignoring case.
fn contains_name(names: &[imap_next::imap_types::core::AString<'_>], name: &str) -> bool {
    names.iter().any(|candidate| {
        let candidate: &[u8] = candidate.as_ref();
        candidate.eq_ignore_ascii_case(name.as_bytes())
    })
}

/// The fields of `header` whose names `keep`, each with its continuation lines, followed by
/// the empty line (RFC 3501 §6.4.5 HEADER.FIELDS).
fn header_fields(header: &[u8], keep: impl Fn(&str) -> bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut keeping = false;
    for line in header.split_inclusive(|byte| *byte == b'\n') {
        let continuation = line
            .first()
            .is_some_and(|byte| *byte == b' ' || *byte == b'\t');
        if !continuation {
            let name = line
                .iter()
                .position(|byte| *byte == b':')
                .and_then(|colon| line.get(..colon))
                .and_then(|name| std::str::from_utf8(name).ok());
            keeping = name.is_some_and(|name| keep(name.trim()));
        }
        if keeping {
            out.extend_from_slice(line);
        }
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// The ENVELOPE of `fields`.
fn envelope(fields: &EnvelopeFields) -> Envelope<'static> {
    Envelope {
        date: nstring(fields.date.as_deref()),
        subject: nstring(fields.subject.as_deref()),
        from: addresses(&fields.from),
        sender: addresses(&fields.sender),
        reply_to: addresses(&fields.reply_to),
        to: addresses(&fields.to),
        cc: addresses(&fields.cc),
        bcc: addresses(&fields.bcc),
        in_reply_to: nstring(fields.in_reply_to.as_deref()),
        message_id: nstring(fields.message_id.as_deref()),
    }
}

/// The envelope addresses of `mailboxes`.
fn addresses(mailboxes: &[Mailbox]) -> Vec<Address<'static>> {
    mailboxes
        .iter()
        .map(|mailbox| Address {
            name: nstring(mailbox.name.as_deref()),
            adl: NString(None),
            mailbox: nstring(mailbox.local.as_deref()),
            host: nstring(mailbox.domain.as_deref()),
        })
        .collect()
}

/// The BODYSTRUCTURE of `entity`, measured in `bytes`.
fn body_structure(entity: &Entity, bytes: &[u8]) -> BodyStructure<'static> {
    if let Content::Multipart(children) = &entity.content {
        let bodies = children
            .iter()
            .map(|child| body_structure(child, bytes))
            .collect::<Vec<_>>();
        if let Ok(bodies) = Vec1::try_from(bodies) {
            return BodyStructure::Multi {
                bodies,
                subtype: istring(&entity.subtype),
                extension_data: None,
            };
        }
    }
    let body = bytes.get(entity.body.clone()).unwrap_or_default();
    let lines =
        u32::try_from(body.iter().filter(|byte| **byte == b'\n').count()).unwrap_or(u32::MAX);
    let basic = BasicFields {
        parameter_list: entity
            .params
            .iter()
            .map(|(name, value)| (istring(name), istring(value)))
            .collect(),
        id: nstring(entity.id.as_deref()),
        description: nstring(entity.description.as_deref()),
        content_transfer_encoding: istring(&entity.encoding),
        size: u32::try_from(body.len()).unwrap_or(u32::MAX),
    };
    let specific = match &entity.content {
        Content::Message(inner) => SpecificFields::Message {
            envelope: Box::new(inner.envelope()),
            body_structure: Box::new(inner.body_structure(bytes)),
            number_of_lines: lines,
        },
        _ if entity.media_type == "text" => SpecificFields::Text {
            subtype: istring(&entity.subtype),
            number_of_lines: lines,
        },
        _ => SpecificFields::Basic {
            r#type: istring(&entity.media_type),
            subtype: istring(&entity.subtype),
        },
    };
    BodyStructure::Single {
        body: Body { basic, specific },
        extension_data: None,
    }
}

/// `text` as an IMAP string. A string IMAP cannot carry (it holds NUL) loses its NULs.
pub fn istring(text: &str) -> IString<'static> {
    IString::try_from(text.to_owned())
        .or_else(|_| IString::try_from(text.replace('\0', "")))
        .unwrap_or_else(|_| IString::Quoted(imap_next::imap_types::core::Quoted::unvalidated("")))
}

/// `text` as an IMAP nstring: NIL when absent.
pub fn nstring(text: Option<&str>) -> NString<'static> {
    NString(text.map(istring))
}

#[cfg(test)]
mod test_mime;
