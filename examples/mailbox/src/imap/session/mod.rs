//! One IMAP session (RFC 3501 §3): its state, and its answer to each command.
//!
//! ```text
//!   Not authenticated ──LOGIN / AUTHENTICATE──▶ Authenticated ──SELECT / EXAMINE──▶ Selected
//!          │                                        ▲    │                          │
//!          └──────────────── LOGOUT ───────────────┼────┴──── CLOSE / UNSELECT ─────┘
//!                                                  └──────────────────────────────── (any)
//!   Selected ── the mailbox is deleted under the session ──▶ BYE, Logout
//! ```
//!
//! `handle` takes one command and returns its responses, untagged data first and the tagged
//! status last. After every command that may run while messages change under it, the session
//! syncs its view with the store, so the client learns of mail that arrived meanwhile. A
//! session in Logout has said BYE: the shell closes the connection once its responses are
//! sent. Extensions: IDLE (in the shell), UIDPLUS, MOVE, UNSELECT, SPECIAL-USE, NAMESPACE, ID,
//! ENABLE.
//!
//! The commands live by what they act on: `mailboxes` (whole mailboxes), `messages` and
//! `fetch` (the selected mailbox's messages); `names` and `respond` hold what they share.

mod fetch;
mod mailboxes;
mod messages;
mod names;
mod respond;

use std::sync::Arc;

use imap_next::imap_types::command::Command;
use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::core::QuotedChar;
use imap_next::imap_types::core::Tag;
use imap_next::imap_types::extensions::namespace::Namespace;
use imap_next::imap_types::response::Code;
use imap_next::imap_types::response::Data;
use imap_next::imap_types::response::Response;
use imap_next::imap_types::response::StatusKind;
pub use names::imap_flag;
pub use names::list_matches;
pub use names::DELIMITER;
pub use respond::bye;
pub use respond::capabilities;
use respond::changes_responses;
pub use respond::continuation;
pub use respond::status;
pub use respond::tagged;
pub use respond::text;
pub use respond::Failure;

use super::mime::istring;
use super::mime::nstring;
use super::view::View;
use crate::store::Store;

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
    /// After BYE: the connection closes.
    Logout,
}

/// One IMAP session.
pub struct Session {
    store: Arc<Store>,
    credentials: Credentials,
    state: State,
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

    /// Whether the session said BYE, so the connection closes once its responses are sent.
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
    /// when `expunge_allowed`. A selected mailbox deleted under the session ends the session:
    /// BYE, and Logout.
    pub async fn sync(&mut self, expunge_allowed: bool) -> Vec<Response<'static>> {
        let State::Selected(view) = &mut self.state else {
            return Vec::new();
        };
        let Ok(mailbox) = self.store.mailbox_by_id(view.id).await else {
            self.state = State::Logout;
            return vec![Response::Status(bye("the selected mailbox was deleted"))];
        };
        changes_responses(view.sync(&mailbox, expunge_allowed))
    }

    /// The responses to `command`: untagged data first, the tagged status last.
    pub async fn handle(&mut self, command: Command<'static>) -> Vec<Response<'static>> {
        let tag = command.tag;
        self.dispatch(&tag, command.body)
            .await
            .unwrap_or_else(|Failure { kind, code, text }| vec![tagged(&tag, kind, code, &text)])
    }

    /// The responses to one command body, or why it failed: the commands valid in any state
    /// here, the others by what they act on.
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
                    Response::Status(bye("logging out")),
                    tagged(tag, StatusKind::Ok, None, "LOGOUT completed"),
                ])
            }
            CommandBody::Login { username, password } => {
                self.login(tag, username.as_ref(), password.declassify().as_ref())
            }
            CommandBody::Enable { .. } => {
                self.authenticated()?;
                let enabled = Data::Enabled {
                    capabilities: Vec::new(),
                };
                let done = tagged(tag, StatusKind::Ok, None, "ENABLE completed");
                Ok(vec![Response::Data(enabled), done])
            }
            CommandBody::Id { .. } => Ok(vec![
                Response::Data(Data::Id {
                    parameters: Some(vec![(istring("name"), nstring(Some("rings-mailbox")))]),
                }),
                tagged(tag, StatusKind::Ok, None, "ID completed"),
            ]),
            CommandBody::Namespace => self.namespace(tag),
            CommandBody::Idle => Err(Failure::bad("IDLE is handled by the connection")),
            CommandBody::Authenticate { .. } => {
                Err(Failure::bad("AUTHENTICATE is handled by the connection"))
            }
            body @ (CommandBody::Select { .. }
            | CommandBody::Examine { .. }
            | CommandBody::Create { .. }
            | CommandBody::Delete { .. }
            | CommandBody::Rename { .. }
            | CommandBody::Subscribe { .. }
            | CommandBody::Unsubscribe { .. }
            | CommandBody::List { .. }
            | CommandBody::Lsub { .. }
            | CommandBody::Status { .. }
            | CommandBody::Append { .. }) => self.mailbox_command(tag, body).await,
            body => self.message_command(tag, body).await,
        }
    }

    /// LOGIN as `user` with `password`.
    fn login(
        &mut self,
        tag: &Tag<'static>,
        user: &[u8],
        password: &[u8],
    ) -> Result<Vec<Response<'static>>, Failure> {
        if !matches!(self.state, State::NotAuthenticated) {
            return Err(Failure::bad("already authenticated"));
        }
        if !self.credentials.accept(user, password) {
            return Err(Failure::no("invalid credentials"));
        }
        self.state = State::Authenticated;
        let code = Some(Code::Capability(capabilities()));
        Ok(vec![tagged(tag, StatusKind::Ok, code, "LOGIN completed")])
    }

    /// NAMESPACE: one personal namespace, at the root.
    fn namespace(&self, tag: &Tag<'static>) -> Result<Vec<Response<'static>>, Failure> {
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
}

#[cfg(test)]
mod test_session;
