//! SEARCH (RFC 3501 §6.4.4): a search key as a predicate over one message.
//!
//! ```text
//!   matches(And ks, m)  = ∀k ∈ ks. matches(k, m)
//!   matches(Or a b, m)  = matches(a, m) ∨ matches(b, m)
//!   matches(Not k, m)   = ¬matches(k, m)
//!   flag keys           read the message's flags
//!   date and size keys  read its INTERNALDATE, Date header and size
//!   text keys           read its header or body, case-insensitively, as substrings
//! ```
//!
//! Only a search with a key that reads the content ([`needs_content`]) has the session read
//! the messages' bytes, so a search by flags never touches them. `\Recent` is never set in this
//! server, so `RECENT` and `NEW` match nothing and `OLD` matches everything.

use std::num::NonZeroU32;

use chrono::DateTime;
use chrono::NaiveDate;
use imap_next::imap_types::core::AString;
use imap_next::imap_types::search::SearchKey;

use super::view::set_contains;
use crate::store::Flag;
use crate::store::Message;

/// One message as SEARCH sees it: its numbers, its metadata, and its content when the search
/// reads it.
pub struct Candidate<'a> {
    /// Its sequence number.
    pub seq: NonZeroU32,
    /// Its metadata.
    pub message: &'a Message,
    /// The largest sequence number and UID of the view, for `*`.
    pub largest: (NonZeroU32, NonZeroU32),
    /// Its content; `None` when the search reads none.
    pub content: Option<&'a Content>,
}

/// The parts of a message's content the text and sent-date keys read.
#[derive(Debug, Default)]
pub struct Content {
    /// The header, lowercase.
    header: String,
    /// The body, lowercase.
    body: String,
    /// The Date header's day, if it parses.
    sent: Option<NaiveDate>,
}

impl Content {
    /// The content of the message `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map_or(bytes.len(), |at| at.saturating_add(4));
        let (header, body) = bytes.split_at(split.min(bytes.len()));
        let sent = mail_parser::MessageParser::default()
            .parse_headers(bytes)
            .and_then(|message| message.date().map(|date| date.to_rfc3339()))
            .and_then(|date| DateTime::parse_from_rfc3339(&date).ok())
            .map(|date| date.date_naive());
        Self {
            header: String::from_utf8_lossy(header).to_lowercase(),
            body: String::from_utf8_lossy(body).to_lowercase(),
            sent,
        }
    }
}

/// The content of no message, for keys read without content.
static EMPTY: Content = Content {
    header: String::new(),
    body: String::new(),
    sent: None,
};

impl Candidate<'_> {
    /// The message's content, empty when the search reads none.
    fn content(&self) -> &Content {
        self.content.unwrap_or(&EMPTY)
    }

    /// The day the store received the message.
    fn received(&self) -> Option<NaiveDate> {
        DateTime::from_timestamp(self.message.internal_date, 0).map(|date| date.date_naive())
    }

    /// Whether the message has `flag`.
    fn has(&self, flag: &Flag) -> bool {
        self.message.flags.contains(flag)
    }

    /// Whether the header field `name` contains `needle`, case-insensitively.
    fn header_contains(&self, name: &str, needle: &AString<'_>) -> bool {
        let needle = lower(needle);
        let name = name.to_lowercase();
        let mut fields = Vec::<String>::new();
        for line in self.content().header.split("\r\n") {
            if line.starts_with([' ', '\t']) {
                if let Some(field) = fields.last_mut() {
                    field.push_str(line);
                }
            } else {
                fields.push(line.to_owned());
            }
        }
        fields.iter().any(|field| {
            field.split_once(':').is_some_and(|(field_name, value)| {
                field_name.trim() == name && value.contains(&needle)
            })
        })
    }
}

/// Whether `key` reads the message content: a text key or a sent-date key, anywhere in it.
pub fn needs_content(key: &SearchKey<'_>) -> bool {
    match key {
        SearchKey::And(keys) => keys.as_ref().iter().any(needs_content),
        SearchKey::Or(left, right) => needs_content(left) || needs_content(right),
        SearchKey::Not(key) => needs_content(key),
        SearchKey::SentBefore(_)
        | SearchKey::SentOn(_)
        | SearchKey::SentSince(_)
        | SearchKey::From(_)
        | SearchKey::To(_)
        | SearchKey::Cc(_)
        | SearchKey::Bcc(_)
        | SearchKey::Subject(_)
        | SearchKey::Header(..)
        | SearchKey::Body(_)
        | SearchKey::Text(_) => true,
        _ => false,
    }
}

/// `text` lowercase.
fn lower(text: &AString<'_>) -> String {
    String::from_utf8_lossy(text.as_ref()).to_lowercase()
}

/// Whether `candidate` matches `key`.
pub fn matches(key: &SearchKey<'_>, candidate: &Candidate<'_>) -> bool {
    let day = |date: &imap_next::imap_types::datetime::NaiveDate| *date.as_ref();
    match key {
        SearchKey::And(keys) => keys.as_ref().iter().all(|key| matches(key, candidate)),
        SearchKey::Or(left, right) => matches(left, candidate) || matches(right, candidate),
        SearchKey::Not(key) => !matches(key, candidate),
        SearchKey::All | SearchKey::Old => true,
        SearchKey::New | SearchKey::Recent => false,
        SearchKey::SequenceSet(set) => set_contains(set, candidate.seq, candidate.largest.0),
        SearchKey::Uid(set) => set_contains(set, candidate.message.uid, candidate.largest.1),
        SearchKey::Answered => candidate.has(&Flag::Answered),
        SearchKey::Unanswered => !candidate.has(&Flag::Answered),
        SearchKey::Deleted => candidate.has(&Flag::Deleted),
        SearchKey::Undeleted => !candidate.has(&Flag::Deleted),
        SearchKey::Draft => candidate.has(&Flag::Draft),
        SearchKey::Undraft => !candidate.has(&Flag::Draft),
        SearchKey::Flagged => candidate.has(&Flag::Flagged),
        SearchKey::Unflagged => !candidate.has(&Flag::Flagged),
        SearchKey::Seen => candidate.has(&Flag::Seen),
        SearchKey::Unseen => !candidate.has(&Flag::Seen),
        SearchKey::Keyword(keyword) => candidate.has(&Flag::Keyword(keyword.inner().to_owned())),
        SearchKey::Unkeyword(keyword) => !candidate.has(&Flag::Keyword(keyword.inner().to_owned())),
        SearchKey::Larger(size) => candidate.message.size > *size,
        SearchKey::Smaller(size) => candidate.message.size < *size,
        SearchKey::Before(date) => candidate.received().is_some_and(|got| got < day(date)),
        SearchKey::On(date) => candidate.received().is_some_and(|got| got == day(date)),
        SearchKey::Since(date) => candidate.received().is_some_and(|got| got >= day(date)),
        SearchKey::SentBefore(date) => candidate.content().sent.is_some_and(|got| got < day(date)),
        SearchKey::SentOn(date) => candidate.content().sent.is_some_and(|got| got == day(date)),
        SearchKey::SentSince(date) => candidate.content().sent.is_some_and(|got| got >= day(date)),
        SearchKey::From(text) => candidate.header_contains("from", text),
        SearchKey::To(text) => candidate.header_contains("to", text),
        SearchKey::Cc(text) => candidate.header_contains("cc", text),
        SearchKey::Bcc(text) => candidate.header_contains("bcc", text),
        SearchKey::Subject(text) => candidate.header_contains("subject", text),
        SearchKey::Header(name, text) => {
            candidate.header_contains(&String::from_utf8_lossy(name.as_ref()), text)
        }
        SearchKey::Body(text) => candidate.content().body.contains(&lower(text)),
        SearchKey::Text(text) => {
            let needle = lower(text);
            let content = candidate.content();
            content.header.contains(&needle) || content.body.contains(&needle)
        }
    }
}

#[cfg(test)]
mod test_search;
