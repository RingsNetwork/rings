//! The commands on mailboxes as a whole (RFC 3501 §6.3): SELECT and EXAMINE, CREATE, DELETE,
//! RENAME, SUBSCRIBE, LIST and LSUB, STATUS, and APPEND.

use std::num::NonZeroU32;

use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::core::QuotedChar;
use imap_next::imap_types::core::Tag;
use imap_next::imap_types::datetime::DateTime;
use imap_next::imap_types::extensions::binary::LiteralOrLiteral8;
use imap_next::imap_types::flag::Flag as ImapFlag;
use imap_next::imap_types::flag::FlagNameAttribute;
use imap_next::imap_types::flag::FlagPerm;
use imap_next::imap_types::mailbox::ListMailbox;
use imap_next::imap_types::mailbox::Mailbox as ImapMailbox;
use imap_next::imap_types::response::Code;
use imap_next::imap_types::response::Data;
use imap_next::imap_types::response::Response;
use imap_next::imap_types::response::StatusKind;
use imap_next::imap_types::status::StatusDataItem;
use imap_next::imap_types::status::StatusDataItemName;

use super::fetch;
use super::names::imap_mailbox;
use super::names::list_attributes;
use super::names::list_matches;
use super::names::mailbox_name;
use super::names::store_flag;
use super::names::system_flags;
use super::names::DELIMITER;
use super::respond::tagged;
use super::respond::untagged_ok;
use super::respond::Failure;
use super::Session;
use super::State;
use crate::imap::view::View;
use crate::store::Flag;
use crate::store::INBOX;

impl Session {
    /// The responses to the mailbox command `body`.
    pub(super) async fn mailbox_command(
        &mut self,
        tag: &Tag<'static>,
        body: CommandBody<'static>,
    ) -> Result<Vec<Response<'static>>, Failure> {
        match body {
            CommandBody::Select { mailbox, .. } => self.select(tag, &mailbox, false).await,
            CommandBody::Examine { mailbox, .. } => self.select(tag, &mailbox, true).await,
            CommandBody::Create { mailbox } => self.create(tag, &mailbox).await,
            CommandBody::Delete { mailbox } => {
                self.authenticated()?;
                let name = mailbox_name(&mailbox);
                self.store.delete(&name).await.map_err(Failure::store)?;
                Ok(vec![tagged(tag, StatusKind::Ok, None, "DELETE completed")])
            }
            CommandBody::Rename { from, to } => {
                self.authenticated()?;
                let (from, to) = (mailbox_name(&from), mailbox_name(&to));
                self.store
                    .rename(&from, &to)
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
            } => self.append(tag, &mailbox, &flags, date, &message).await,
            _ => Err(Failure::bad("command not supported")),
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

    /// CREATE `mailbox`.
    async fn create(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &ImapMailbox<'static>,
    ) -> Result<Vec<Response<'static>>, Failure> {
        self.authenticated()?;
        let name = mailbox_name(mailbox);
        let name = name.trim_end_matches(DELIMITER);
        self.store.create(name).await.map_err(Failure::store)?;
        Ok(vec![tagged(tag, StatusKind::Ok, None, "CREATE completed")])
    }

    /// APPEND `message` to `mailbox` with `flags`, received at `date` or now.
    async fn append(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &ImapMailbox<'static>,
        flags: &[ImapFlag<'static>],
        date: Option<DateTime>,
        message: &LiteralOrLiteral8<'static>,
    ) -> Result<Vec<Response<'static>>, Failure> {
        self.authenticated()?;
        let flags = flags.iter().filter_map(store_flag).collect();
        let date = date.map_or_else(
            || chrono::Utc::now().timestamp(),
            |date| date.as_ref().timestamp(),
        );
        let bytes = fetch::appended(message);
        let (uid_validity, uid) = self
            .store
            .append(&mailbox_name(mailbox), &bytes, flags, date)
            .await
            .map_err(Failure::try_create)?;
        let mut responses = self.sync(true).await;
        responses.push(tagged(
            tag,
            StatusKind::Ok,
            Some(Code::AppendUid { uid_validity, uid }),
            "APPEND completed",
        ));
        Ok(responses)
    }
}
