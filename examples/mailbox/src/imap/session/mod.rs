//! One IMAP session (RFC 3501 §3): its state, and its answer to each command.
//!
//! ```text
//!   Not authenticated ──LOGIN / AUTHENTICATE──▶ Authenticated ──SELECT / EXAMINE──▶ Selected
//!          │                                        ▲    │                          │
//!          └──────────────── LOGOUT ───────────────┼────┴──── CLOSE / UNSELECT ─────┘
//!                                                  └──────────────────────────────── (any)
//! ```
//!
//! `handle` takes one command and returns its responses, untagged data first and the tagged
//! status last. After every command that may run while messages change under it, the session
//! syncs its view with the store, so the client learns of mail that arrived meanwhile.
//! Extensions: IDLE (in the shell), UIDPLUS, MOVE, UNSELECT, SPECIAL-USE, NAMESPACE, ID,
//! ENABLE.

mod fetch;

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use imap_next::imap_types::auth::AuthMechanism;
use imap_next::imap_types::command::Command;
use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::core::Charset;
use imap_next::imap_types::core::QuotedChar;
use imap_next::imap_types::core::Tag;
use imap_next::imap_types::core::Text;
use imap_next::imap_types::core::Vec1;
use imap_next::imap_types::extensions::namespace::Namespace;
use imap_next::imap_types::extensions::uidplus::UidElement;
use imap_next::imap_types::extensions::uidplus::UidSet;
use imap_next::imap_types::flag::Flag as ImapFlag;
use imap_next::imap_types::flag::FlagNameAttribute;
use imap_next::imap_types::flag::FlagPerm;
use imap_next::imap_types::flag::StoreResponse;
use imap_next::imap_types::flag::StoreType;
use imap_next::imap_types::mailbox::ListMailbox;
use imap_next::imap_types::mailbox::Mailbox as ImapMailbox;
use imap_next::imap_types::response::Bye;
use imap_next::imap_types::response::Capability;
use imap_next::imap_types::response::Code;
use imap_next::imap_types::response::Data;
use imap_next::imap_types::response::Response;
use imap_next::imap_types::response::Status;
use imap_next::imap_types::response::StatusBody;
use imap_next::imap_types::response::StatusKind;
use imap_next::imap_types::response::Tagged;
use imap_next::imap_types::search::SearchKey;
use imap_next::imap_types::sequence::SequenceSet;
use imap_next::imap_types::status::StatusDataItem;
use imap_next::imap_types::status::StatusDataItemName;

use super::mime::istring;
use super::mime::nstring;
use super::search::matches;
use super::search::needs_content;
use super::search::Candidate;
use super::search::Content;
use super::view::Changes;
use super::view::View;
use crate::store::Flag;
use crate::store::FlagChange;
use crate::store::Listed;
use crate::store::SpecialUse;
use crate::store::Store;
use crate::store::StoreError;
use crate::store::INBOX;

/// The hierarchy delimiter of mailbox names.
pub const DELIMITER: char = '/';

/// The user's credentials: their user name and password.
#[derive(Clone)]
pub struct Credentials {
    /// The user name, such as `me`; `me@<anything>` is accepted too.
    pub user: String,
    /// The password.
    pub password: String,
}

impl Credentials {
    /// Whether `user` and `password` are these credentials. The password is compared in time
    /// independent of where it differs.
    pub fn accept(&self, user: &[u8], password: &[u8]) -> bool {
        let user = String::from_utf8_lossy(user);
        let user_matches = user == self.user
            || user
                .split_once('@')
                .is_some_and(|(local, _domain)| local == self.user);
        let expected = self.password.as_bytes();
        let differs = expected.len() != password.len()
            || expected
                .iter()
                .zip(password)
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                != 0;
        user_matches && !differs
    }
}

/// Where the session is.
enum State {
    /// Before LOGIN.
    NotAuthenticated,
    /// Logged in, no mailbox selected.
    Authenticated,
    /// A mailbox selected.
    Selected(View),
    /// After LOGOUT.
    Logout,
}

/// One IMAP session.
pub struct Session {
    store: Arc<Store>,
    credentials: Credentials,
    state: State,
}

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

/// The tagged status of `kind` for `tag`, with `code` and `text`.
pub fn tagged(
    tag: &Tag<'static>,
    kind: StatusKind,
    code: Option<Code<'static>>,
    message: &str,
) -> Response<'static> {
    Response::Status(Status::Tagged(Tagged {
        tag: tag.clone(),
        body: StatusBody {
            kind,
            code,
            text: text(message),
        },
    }))
}

/// An untagged OK with `code`.
fn untagged_ok(code: Code<'static>, message: &str) -> Response<'static> {
    Response::Status(Status::Untagged(StatusBody {
        kind: StatusKind::Ok,
        code: Some(code),
        text: text(message),
    }))
}

/// The untagged BYE with `message`.
fn bye(message: &str) -> Response<'static> {
    Response::Status(Status::Bye(Bye {
        code: None,
        text: text(message),
    }))
}

/// The text of a mailbox name: `INBOX` for the inbox.
fn mailbox_name(mailbox: &ImapMailbox<'_>) -> String {
    match mailbox {
        ImapMailbox::Inbox => INBOX.to_owned(),
        ImapMailbox::Other(other) => String::from_utf8_lossy(other.inner().as_ref()).into_owned(),
    }
}

/// `name` as an IMAP mailbox.
fn imap_mailbox(name: &str) -> ImapMailbox<'static> {
    ImapMailbox::try_from(name.to_owned()).unwrap_or(ImapMailbox::Inbox)
}

/// The store flag an IMAP flag names.
fn store_flag(flag: &ImapFlag<'_>) -> Option<Flag> {
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

/// The system flags, as FLAGS and PERMANENTFLAGS list them.
fn system_flags() -> Vec<ImapFlag<'static>> {
    vec![
        ImapFlag::Answered,
        ImapFlag::Flagged,
        ImapFlag::Deleted,
        ImapFlag::Seen,
        ImapFlag::Draft,
    ]
}

/// The UID set of `uids`.
fn uid_set(uids: &[NonZeroU32]) -> Option<UidSet> {
    let elements = uids
        .iter()
        .copied()
        .map(UidElement::Single)
        .collect::<Vec<_>>();
    Vec1::try_from(elements).ok().map(UidSet)
}

/// The LIST attributes of `listed`.
fn list_attributes(listed: &Listed) -> Vec<FlagNameAttribute<'static>> {
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

impl Session {
    /// A new session over `store`, admitting `credentials`.
    pub fn new(store: Arc<Store>, credentials: Credentials) -> Self {
        Self {
            store,
            credentials,
            state: State::NotAuthenticated,
        }
    }

    /// Whether the client logged out.
    pub fn is_logged_out(&self) -> bool {
        matches!(self.state, State::Logout)
    }

    /// Whether a mailbox is selected.
    pub fn is_selected(&self) -> bool {
        matches!(self.state, State::Selected(_))
    }

    /// Whether the session is authenticated.
    pub fn is_authenticated(&self) -> bool {
        matches!(self.state, State::Authenticated | State::Selected(_))
    }

    /// Authenticate with SASL PLAIN's `message` (RFC 4616: authzid NUL authcid NUL password).
    pub fn authenticate_plain(&mut self, message: &[u8]) -> bool {
        let mut fields = message.split(|byte| *byte == 0);
        let (_authzid, user, password) = (fields.next(), fields.next(), fields.next());
        let accepted = matches!(self.state, State::NotAuthenticated)
            && fields.next().is_none()
            && user
                .zip(password)
                .is_some_and(|(user, password)| self.credentials.accept(user, password));
        if accepted {
            self.state = State::Authenticated;
        }
        accepted
    }

    /// The untagged responses that bring the client up to date with the store; removals only
    /// when `expunge_allowed`.
    pub async fn sync(&mut self, expunge_allowed: bool) -> Vec<Response<'static>> {
        let State::Selected(view) = &mut self.state else {
            return Vec::new();
        };
        let Ok(mailbox) = self.store.mailbox_by_id(view.id).await else {
            // The selected mailbox was deleted under the session: the session leaves it.
            self.state = State::Authenticated;
            return vec![bye("the selected mailbox was deleted")];
        };
        changes_responses(view.sync(&mailbox, expunge_allowed))
    }

    /// The responses to `command`, the tagged status last.
    pub async fn handle(&mut self, command: Command<'static>) -> Vec<Response<'static>> {
        let tag = command.tag;
        let mut responses = match self.dispatch(&tag, command.body).await {
            Ok(responses) => responses,
            Err(Failure { kind, code, text }) => vec![tagged(&tag, kind, code, &text)],
        };
        // The tagged status stays last: sync responses go before it.
        let status = responses.pop();
        responses.extend(status);
        responses
    }

    /// The responses to one command body, or why it failed.
    async fn dispatch(
        &mut self,
        tag: &Tag<'static>,
        body: CommandBody<'static>,
    ) -> Result<Vec<Response<'static>>, Failure> {
        match body {
            CommandBody::Capability => Ok(vec![
                Response::Data(Data::Capability(capabilities())),
                tagged(tag, StatusKind::Ok, None, "CAPABILITY completed"),
            ]),
            CommandBody::Noop | CommandBody::Check => {
                let mut responses = self.sync(true).await;
                responses.push(tagged(tag, StatusKind::Ok, None, "completed"));
                Ok(responses)
            }
            CommandBody::Logout => {
                self.state = State::Logout;
                Ok(vec![
                    bye("logging out"),
                    tagged(tag, StatusKind::Ok, None, "LOGOUT completed"),
                ])
            }
            CommandBody::Login { username, password } => {
                if !matches!(self.state, State::NotAuthenticated) {
                    return Err(Failure::bad("already authenticated"));
                }
                if !self
                    .credentials
                    .accept(username.as_ref(), password.declassify().as_ref())
                {
                    return Err(Failure::no("invalid credentials"));
                }
                self.state = State::Authenticated;
                Ok(vec![tagged(
                    tag,
                    StatusKind::Ok,
                    Some(Code::Capability(capabilities())),
                    "LOGIN completed",
                )])
            }
            CommandBody::Enable { .. } => {
                self.authenticated()?;
                Ok(vec![
                    Response::Data(Data::Enabled {
                        capabilities: Vec::new(),
                    }),
                    tagged(tag, StatusKind::Ok, None, "ENABLE completed"),
                ])
            }
            CommandBody::Id { .. } => Ok(vec![
                Response::Data(Data::Id {
                    parameters: Some(vec![(istring("name"), nstring(Some("rings-mailbox")))]),
                }),
                tagged(tag, StatusKind::Ok, None, "ID completed"),
            ]),
            CommandBody::Namespace => {
                self.authenticated()?;
                let personal = vec![Namespace {
                    prefix: istring(""),
                    delimiter: QuotedChar::try_from(DELIMITER).ok(),
                    extensions: Vec::new(),
                }];
                Ok(vec![
                    Response::Data(Data::Namespace {
                        personal,
                        other: Vec::new(),
                        shared: Vec::new(),
                    }),
                    tagged(tag, StatusKind::Ok, None, "NAMESPACE completed"),
                ])
            }
            CommandBody::Select { mailbox, .. } => self.select(tag, &mailbox, false).await,
            CommandBody::Examine { mailbox, .. } => self.select(tag, &mailbox, true).await,
            CommandBody::Create { mailbox } => {
                self.authenticated()?;
                let name = mailbox_name(&mailbox);
                let name = name.trim_end_matches(DELIMITER);
                self.store.create(name).await.map_err(Failure::store)?;
                Ok(vec![tagged(tag, StatusKind::Ok, None, "CREATE completed")])
            }
            CommandBody::Delete { mailbox } => {
                self.authenticated()?;
                self.store
                    .delete(&mailbox_name(&mailbox))
                    .await
                    .map_err(Failure::store)?;
                Ok(vec![tagged(tag, StatusKind::Ok, None, "DELETE completed")])
            }
            CommandBody::Rename { from, to } => {
                self.authenticated()?;
                self.store
                    .rename(&mailbox_name(&from), &mailbox_name(&to))
                    .await
                    .map_err(Failure::store)?;
                Ok(vec![tagged(tag, StatusKind::Ok, None, "RENAME completed")])
            }
            CommandBody::Subscribe { .. } | CommandBody::Unsubscribe { .. } => {
                // Every mailbox is subscribed, always.
                self.authenticated()?;
                Ok(vec![tagged(tag, StatusKind::Ok, None, "completed")])
            }
            CommandBody::List {
                reference,
                mailbox_wildcard,
            } => self.list(tag, &reference, &mailbox_wildcard, false).await,
            CommandBody::Lsub {
                reference,
                mailbox_wildcard,
            } => self.list(tag, &reference, &mailbox_wildcard, true).await,
            CommandBody::Status {
                mailbox,
                item_names,
            } => self.status(tag, &mailbox, &item_names).await,
            CommandBody::Append {
                mailbox,
                flags,
                date,
                message,
            } => {
                self.authenticated()?;
                let flags = flags.iter().filter_map(store_flag).collect();
                let date = date.map_or_else(
                    || chrono::Utc::now().timestamp(),
                    |date| date.as_ref().timestamp(),
                );
                let bytes = fetch::appended(&message);
                let (uid_validity, uid) = self
                    .store
                    .append(&mailbox_name(&mailbox), &bytes, flags, date)
                    .await
                    .map_err(|error| match error {
                        StoreError::NoMailbox(_) => Failure {
                            kind: StatusKind::No,
                            code: Some(Code::TryCreate),
                            text: "no such mailbox".to_owned(),
                        },
                        error => Failure::store(error),
                    })?;
                let mut responses = self.sync(true).await;
                responses.push(tagged(
                    tag,
                    StatusKind::Ok,
                    Some(Code::AppendUid { uid_validity, uid }),
                    "APPEND completed",
                ));
                Ok(responses)
            }
            CommandBody::Close => {
                let view = self.selected()?.clone();
                if !view.read_only {
                    self.store
                        .expunge(view.id, None)
                        .await
                        .map_err(Failure::store)?;
                }
                self.state = State::Authenticated;
                Ok(vec![tagged(tag, StatusKind::Ok, None, "CLOSE completed")])
            }
            CommandBody::Unselect => {
                self.selected()?;
                self.state = State::Authenticated;
                Ok(vec![tagged(
                    tag,
                    StatusKind::Ok,
                    None,
                    "UNSELECT completed",
                )])
            }
            CommandBody::Expunge => self.expunge(tag, None).await,
            CommandBody::ExpungeUid { sequence_set } => {
                self.expunge(tag, Some(&sequence_set)).await
            }
            CommandBody::Search {
                charset,
                criteria,
                uid,
            } => self.search(tag, charset.as_ref(), &criteria, uid).await,
            CommandBody::Fetch {
                sequence_set,
                macro_or_item_names,
                uid,
                ..
            } => {
                self.fetch(tag, &sequence_set, macro_or_item_names, uid)
                    .await
            }
            CommandBody::Store {
                sequence_set,
                kind,
                response,
                flags,
                uid,
                ..
            } => {
                self.store_flags(tag, &sequence_set, kind, response, &flags, uid)
                    .await
            }
            CommandBody::Copy {
                sequence_set,
                mailbox,
                uid,
            } => self.copy(tag, &sequence_set, &mailbox, uid, false).await,
            CommandBody::Move {
                sequence_set,
                mailbox,
                uid,
            } => self.copy(tag, &sequence_set, &mailbox, uid, true).await,
            CommandBody::Idle => Err(Failure::bad("IDLE is handled by the connection")),
            CommandBody::Authenticate { .. } => {
                Err(Failure::bad("AUTHENTICATE is handled by the connection"))
            }
            _ => Err(Failure::bad("command not supported")),
        }
    }

    /// Fail unless authenticated.
    fn authenticated(&self) -> Result<(), Failure> {
        if self.is_authenticated() {
            Ok(())
        } else {
            Err(Failure::bad("log in first"))
        }
    }

    /// The selected view, or a failure when none is selected.
    fn selected(&self) -> Result<&View, Failure> {
        match &self.state {
            State::Selected(view) => Ok(view),
            _ => Err(Failure::bad("select a mailbox first")),
        }
    }

    /// The selected view, mutable, or a failure when none is selected.
    fn selected_mut(&mut self) -> Result<&mut View, Failure> {
        match &mut self.state {
            State::Selected(view) => Ok(view),
            _ => Err(Failure::bad("select a mailbox first")),
        }
    }

    /// SELECT (`read_only = false`) or EXAMINE `mailbox`.
    async fn select(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &ImapMailbox<'static>,
        read_only: bool,
    ) -> Result<Vec<Response<'static>>, Failure> {
        self.authenticated()?;
        // A failed SELECT leaves no mailbox selected (RFC 3501 §6.3.1).
        self.state = State::Authenticated;
        let (listed, mailbox) = self
            .store
            .mailbox(&mailbox_name(mailbox))
            .await
            .map_err(Failure::store)?;
        let view = View::open(listed.id, &mailbox, read_only);
        let mut permanent = system_flags()
            .into_iter()
            .map(FlagPerm::Flag)
            .collect::<Vec<_>>();
        permanent.push(FlagPerm::Asterisk);
        let mut responses = vec![
            Response::Data(Data::Flags(system_flags())),
            Response::Data(Data::Exists(view.count())),
            Response::Data(Data::Recent(0)),
            untagged_ok(Code::UidValidity(mailbox.uid_validity), "UIDs valid"),
            untagged_ok(Code::UidNext(mailbox.uid_next), "predicted next UID"),
            untagged_ok(
                Code::PermanentFlags(if read_only { Vec::new() } else { permanent }),
                "flags permitted",
            ),
        ];
        let first_unseen = mailbox
            .messages
            .iter()
            .position(|message| !message.flags.contains(&Flag::Seen))
            .and_then(|index| u32::try_from(index.saturating_add(1)).ok())
            .and_then(NonZeroU32::new);
        if let Some(seq) = first_unseen {
            responses.push(untagged_ok(Code::Unseen(seq), "first unseen"));
        }
        let code = if read_only {
            Code::ReadOnly
        } else {
            Code::ReadWrite
        };
        responses.push(tagged(tag, StatusKind::Ok, Some(code), "SELECT completed"));
        self.state = State::Selected(view);
        Ok(responses)
    }

    /// LIST (or LSUB when `lsub`) `pattern` under `reference`.
    async fn list(
        &mut self,
        tag: &Tag<'static>,
        reference: &ImapMailbox<'static>,
        pattern: &ListMailbox<'static>,
        lsub: bool,
    ) -> Result<Vec<Response<'static>>, Failure> {
        self.authenticated()?;
        let pattern = match pattern {
            ListMailbox::Token(token) => token.as_ref().to_vec(),
            ListMailbox::String(string) => string.as_ref().to_vec(),
        };
        let reference = match reference {
            ImapMailbox::Inbox => INBOX.as_bytes().to_vec(),
            ImapMailbox::Other(other) => other.inner().as_ref().to_vec(),
        };
        let delimiter = QuotedChar::try_from(DELIMITER).ok();
        let mut responses = Vec::new();
        if pattern.is_empty() {
            // An empty pattern asks for the delimiter and the reference's root.
            responses.push(Response::Data(Data::List {
                items: vec![FlagNameAttribute::Noselect],
                delimiter,
                mailbox: imap_mailbox(""),
            }));
        } else {
            let full = [reference.as_slice(), pattern.as_slice()].concat();
            for listed in self.store.list().await {
                let name = listed.name.as_bytes();
                let inbox_match =
                    listed.name == INBOX && list_matches(&full.to_ascii_uppercase(), name);
                if !(list_matches(&full, name) || inbox_match) {
                    continue;
                }
                let items = list_attributes(&listed);
                let mailbox = imap_mailbox(&listed.name);
                responses.push(Response::Data(if lsub {
                    Data::Lsub {
                        items,
                        delimiter,
                        mailbox,
                    }
                } else {
                    Data::List {
                        items,
                        delimiter,
                        mailbox,
                    }
                }));
            }
        }
        responses.push(tagged(tag, StatusKind::Ok, None, "LIST completed"));
        Ok(responses)
    }

    /// STATUS of `mailbox`.
    async fn status(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &ImapMailbox<'static>,
        names: &[StatusDataItemName],
    ) -> Result<Vec<Response<'static>>, Failure> {
        self.authenticated()?;
        let (_, model) = self
            .store
            .mailbox(&mailbox_name(mailbox))
            .await
            .map_err(Failure::store)?;
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let items = names
            .iter()
            .filter_map(|name| match name {
                StatusDataItemName::Messages => {
                    Some(StatusDataItem::Messages(count(model.messages.len())))
                }
                StatusDataItemName::Recent => Some(StatusDataItem::Recent(0)),
                StatusDataItemName::UidNext => Some(StatusDataItem::UidNext(model.uid_next)),
                StatusDataItemName::UidValidity => {
                    Some(StatusDataItem::UidValidity(model.uid_validity))
                }
                StatusDataItemName::Unseen => Some(StatusDataItem::Unseen(count(model.unseen()))),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut responses = vec![Response::Data(Data::Status {
            mailbox: mailbox.clone(),
            items: items.into(),
        })];
        responses.extend(self.sync(true).await);
        responses.push(tagged(tag, StatusKind::Ok, None, "STATUS completed"));
        Ok(responses)
    }

    /// EXPUNGE, or UID EXPUNGE of `only`.
    async fn expunge(
        &mut self,
        tag: &Tag<'static>,
        only: Option<&SequenceSet>,
    ) -> Result<Vec<Response<'static>>, Failure> {
        let view = self.selected()?;
        if view.read_only {
            return Err(Failure::no("mailbox is read-only"));
        }
        let only = only.map(|set| {
            view.resolve(set, true)
                .into_iter()
                .map(|(_seq, uid)| uid)
                .collect::<BTreeSet<_>>()
        });
        let id = view.id;
        self.store
            .expunge(id, only.as_ref())
            .await
            .map_err(Failure::store)?;
        let mut responses = self.sync(true).await;
        responses.push(tagged(tag, StatusKind::Ok, None, "EXPUNGE completed"));
        Ok(responses)
    }

    /// SEARCH, or UID SEARCH, for `criteria`.
    async fn search(
        &mut self,
        tag: &Tag<'static>,
        charset: Option<&Charset<'static>>,
        criteria: &Vec1<SearchKey<'static>>,
        uid: bool,
    ) -> Result<Vec<Response<'static>>, Failure> {
        if charset.is_some_and(|charset| {
            let name = charset.as_ref().to_ascii_uppercase();
            name != "UTF-8" && name != "US-ASCII"
        }) {
            return Err(Failure {
                kind: StatusKind::No,
                code: Some(Code::BadCharset {
                    allowed: Vec::new(),
                }),
                text: "only UTF-8 and US-ASCII".to_owned(),
            });
        }
        let view = self.selected()?.clone();
        let mailbox = self
            .store
            .mailbox_by_id(view.id)
            .await
            .map_err(Failure::store)?;
        let key = match criteria.as_ref() {
            [single] => single.clone(),
            _ => SearchKey::And(criteria.clone()),
        };
        let reads_content = needs_content(&key);
        let (Some(largest_seq), Some(largest_uid)) =
            (NonZeroU32::new(view.count()), view.uids().last().copied())
        else {
            let mut responses = vec![Response::Data(Data::Search(Vec::new()))];
            responses.extend(self.sync(false).await);
            responses.push(tagged(tag, StatusKind::Ok, None, "SEARCH completed"));
            return Ok(responses);
        };
        let mut found = Vec::new();
        for (index, view_uid) in view.uids().iter().enumerate() {
            let Some(message) = mailbox.message(*view_uid) else {
                continue;
            };
            let content = if reads_content {
                Some(Content::of(
                    &self
                        .store
                        .read(view.id, *view_uid)
                        .await
                        .unwrap_or_default(),
                ))
            } else {
                None
            };
            let Some(seq) = u32::try_from(index.saturating_add(1))
                .ok()
                .and_then(NonZeroU32::new)
            else {
                continue;
            };
            let candidate = Candidate {
                seq,
                message,
                largest: (largest_seq, largest_uid),
                content: content.as_ref(),
            };
            if matches(&key, &candidate) {
                found.push(if uid { *view_uid } else { seq });
            }
        }
        let mut responses = vec![Response::Data(Data::Search(found))];
        responses.extend(self.sync(false).await);
        responses.push(tagged(tag, StatusKind::Ok, None, "SEARCH completed"));
        Ok(responses)
    }

    /// STORE, or UID STORE, `flags` on `set`.
    async fn store_flags(
        &mut self,
        tag: &Tag<'static>,
        set: &SequenceSet,
        kind: StoreType,
        response: StoreResponse,
        flags: &[ImapFlag<'static>],
        uid: bool,
    ) -> Result<Vec<Response<'static>>, Failure> {
        let view = self.selected()?.clone();
        if view.read_only {
            return Err(Failure::no("mailbox is read-only"));
        }
        let targets = view.resolve(set, uid);
        let uids = targets
            .iter()
            .map(|(_seq, uid)| *uid)
            .collect::<BTreeSet<_>>();
        let change = match kind {
            StoreType::Add => FlagChange::Add,
            StoreType::Remove => FlagChange::Remove,
            StoreType::Replace => FlagChange::Replace,
        };
        let given = flags.iter().filter_map(store_flag).collect::<BTreeSet<_>>();
        self.store
            .change_flags(view.id, &uids, change, &given)
            .await
            .map_err(Failure::store)?;
        let mailbox = self
            .store
            .mailbox_by_id(view.id)
            .await
            .map_err(Failure::store)?;
        let mut responses = Vec::new();
        let view = self.selected_mut()?;
        for (seq, target_uid) in targets {
            let Some(message) = mailbox.message(target_uid) else {
                continue;
            };
            view.told_flags(target_uid, message.flags.clone());
            if response == StoreResponse::Answer {
                responses.push(fetch::flags_response(seq, target_uid, &message.flags, uid));
            }
        }
        responses.extend(self.sync(false).await);
        responses.push(tagged(tag, StatusKind::Ok, None, "STORE completed"));
        Ok(responses)
    }

    /// COPY, or MOVE when `moving`, `set` to `target`.
    async fn copy(
        &mut self,
        tag: &Tag<'static>,
        set: &SequenceSet,
        target: &ImapMailbox<'static>,
        uid: bool,
        moving: bool,
    ) -> Result<Vec<Response<'static>>, Failure> {
        let view = self.selected()?.clone();
        if moving && view.read_only {
            return Err(Failure::no("mailbox is read-only"));
        }
        let uids = view
            .resolve(set, uid)
            .into_iter()
            .map(|(_seq, uid)| uid)
            .collect::<BTreeSet<_>>();
        let copied = self
            .store
            .copy(view.id, &uids, &mailbox_name(target))
            .await
            .map_err(|error| match error {
                StoreError::NoMailbox(_) => Failure {
                    kind: StatusKind::No,
                    code: Some(Code::TryCreate),
                    text: "no such mailbox".to_owned(),
                },
                error => Failure::store(error),
            })?;
        let sources = copied
            .uids
            .iter()
            .map(|(source, _)| *source)
            .collect::<Vec<_>>();
        let targets = copied
            .uids
            .iter()
            .map(|(_, target)| *target)
            .collect::<Vec<_>>();
        let code = uid_set(&sources)
            .zip(uid_set(&targets))
            .map(|(source, destination)| Code::CopyUid {
                uid_validity: copied.uid_validity,
                source,
                destination,
            });
        let mut responses = Vec::new();
        if moving {
            let moved = sources.iter().copied().collect::<BTreeSet<_>>();
            let deleted = BTreeSet::from([Flag::Deleted]);
            self.store
                .change_flags(view.id, &moved, FlagChange::Add, &deleted)
                .await
                .map_err(Failure::store)?;
            self.store
                .expunge(view.id, Some(&moved))
                .await
                .map_err(Failure::store)?;
            if let Some(code) = code.clone() {
                responses.push(untagged_ok(code, "moved"));
            }
            responses.extend(self.sync(true).await);
            responses.push(tagged(tag, StatusKind::Ok, None, "MOVE completed"));
        } else {
            responses.extend(self.sync(true).await);
            responses.push(tagged(tag, StatusKind::Ok, code, "COPY completed"));
        }
        Ok(responses)
    }
}

/// The untagged responses of `changes`.
fn changes_responses(changes: Changes) -> Vec<Response<'static>> {
    let mut responses = changes
        .expunged
        .into_iter()
        .map(|seq| Response::Data(Data::Expunge(seq)))
        .collect::<Vec<_>>();
    responses.extend(
        changes
            .flags
            .into_iter()
            .map(|(seq, uid, flags)| fetch::flags_response(seq, uid, &flags, true)),
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
    kind: StatusKind,
    /// The response code.
    code: Option<Code<'static>>,
    /// The text.
    text: String,
}

impl Failure {
    /// A BAD with `text`: the command was not valid here.
    fn bad(text: &str) -> Self {
        Self {
            kind: StatusKind::Bad,
            code: None,
            text: text.to_owned(),
        }
    }

    /// A NO with `text`: the command was valid but failed.
    fn no(text: &str) -> Self {
        Self {
            kind: StatusKind::No,
            code: None,
            text: text.to_owned(),
        }
    }

    /// The NO a store failure answers with.
    fn store(error: StoreError) -> Self {
        Self::no(&error.to_string())
    }
}

#[cfg(test)]
mod test_session;
