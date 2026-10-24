//! The responses a session sends: status lines, capabilities, and the untagged data that
//! brings a client up to date.

use imap_next::imap_types::auth::AuthMechanism;
use imap_next::imap_types::core::Tag;
use imap_next::imap_types::core::Text;
use imap_next::imap_types::core::Vec1;
use imap_next::imap_types::response::Bye;
use imap_next::imap_types::response::Capability;
use imap_next::imap_types::response::Code;
use imap_next::imap_types::response::CommandContinuationRequest;
use imap_next::imap_types::response::Data;
use imap_next::imap_types::response::Response;
use imap_next::imap_types::response::Status;
use imap_next::imap_types::response::StatusBody;
use imap_next::imap_types::response::StatusKind;
use imap_next::imap_types::response::Tagged;

use super::fetch::flags_response;
use crate::imap::view::Changes;
use crate::store::StoreError;

/// The capabilities this server advertises.
pub fn capabilities() -> Vec1<Capability<'static>> {
    let other = |name: &'static str| {
        imap_next::imap_types::core::Atom::try_from(name).map(Capability::from)
    };
    let mut capabilities = vec![
        Capability::Imap4Rev1,
        Capability::Auth(AuthMechanism::Plain),
        Capability::Idle,
        Capability::UidPlus,
        Capability::Move,
        Capability::Unselect,
        Capability::Namespace,
        Capability::Id,
        Capability::Enable,
    ];
    capabilities.extend(other("SPECIAL-USE"));
    Vec1::try_from(capabilities).unwrap_or_else(|_| Vec1::from(Capability::Imap4Rev1))
}

/// `text` as status text: one line, never empty.
pub fn text(text: &str) -> Text<'static> {
    let line = text.replace(['\r', '\n'], " ");
    Text::try_from(line).unwrap_or_else(|_| Text::unvalidated("-"))
}

/// The status of `kind` with `code` and `text`: tagged with `tag`, or untagged without one.
pub fn status(
    tag: Option<&Tag<'static>>,
    kind: StatusKind,
    code: Option<Code<'static>>,
    message: &str,
) -> Status<'static> {
    let body = StatusBody {
        kind,
        code,
        text: text(message),
    };
    match tag {
        Some(tag) => Status::Tagged(Tagged {
            tag: tag.clone(),
            body,
        }),
        None => Status::Untagged(body),
    }
}

/// The tagged status of `kind` for `tag`, with `code` and `text`.
pub fn tagged(
    tag: &Tag<'static>,
    kind: StatusKind,
    code: Option<Code<'static>>,
    message: &str,
) -> Response<'static> {
    Response::Status(status(Some(tag), kind, code, message))
}

/// The continuation request `+ text`; `+` alone when `text` cannot be one (it may not start
/// with `[`).
pub fn continuation(message: &str) -> CommandContinuationRequest<'static> {
    CommandContinuationRequest::basic(None, text(message))
        .unwrap_or_else(|_| CommandContinuationRequest::base64(Vec::new()))
}

/// An untagged OK with `code`.
pub(super) fn untagged_ok(code: Code<'static>, message: &str) -> Response<'static> {
    Response::Status(status(None, StatusKind::Ok, Some(code), message))
}

/// The untagged BYE with `message`.
pub fn bye(message: &str) -> Status<'static> {
    Status::Bye(Bye {
        code: None,
        text: text(message),
    })
}

/// The untagged responses of `changes`.
pub(super) fn changes_responses(changes: Changes) -> Vec<Response<'static>> {
    let mut responses = changes
        .expunged
        .into_iter()
        .map(|seq| Response::Data(Data::Expunge(seq)))
        .collect::<Vec<_>>();
    responses.extend(
        changes
            .flags
            .into_iter()
            .map(|(seq, uid, flags)| flags_response(seq, uid, &flags, true)),
    );
    if let Some(exists) = changes.exists {
        responses.push(Response::Data(Data::Exists(exists)));
        responses.push(Response::Data(Data::Recent(0)));
    }
    responses
}

/// Why a command failed: the tagged status to answer it with.
pub struct Failure {
    /// NO or BAD.
    pub(super) kind: StatusKind,
    /// The response code.
    pub(super) code: Option<Code<'static>>,
    /// The text.
    pub(super) text: String,
}

impl Failure {
    /// A BAD with `text`: the command was not valid here.
    pub(super) fn bad(text: &str) -> Self {
        Self {
            kind: StatusKind::Bad,
            code: None,
            text: text.to_owned(),
        }
    }

    /// A NO with `text`: the command was valid but failed.
    pub(super) fn no(text: &str) -> Self {
        Self {
            kind: StatusKind::No,
            code: None,
            text: text.to_owned(),
        }
    }

    /// The NO a store failure answers with.
    pub(super) fn store(error: StoreError) -> Self {
        Self::no(&error.to_string())
    }

    /// The NO a store failure answers with, telling the client to create the mailbox first when
    /// there is none of that name (`TRYCREATE`).
    pub(super) fn try_create(error: StoreError) -> Self {
        match error {
            StoreError::NoMailbox(_) => Self {
                kind: StatusKind::No,
                code: Some(Code::TryCreate),
                text: "no such mailbox".to_owned(),
            },
            error => Self::store(error),
        }
    }
}
