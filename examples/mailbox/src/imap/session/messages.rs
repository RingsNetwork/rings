//! The commands on the selected mailbox's messages (RFC 3501 §6.4): CLOSE and UNSELECT,
//! EXPUNGE, SEARCH, STORE, COPY and MOVE; FETCH is its own module.

use std::collections::BTreeSet;
use std::num::NonZeroU32;

use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::core::Charset;
use imap_next::imap_types::core::Tag;
use imap_next::imap_types::core::Vec1;
use imap_next::imap_types::flag::Flag as ImapFlag;
use imap_next::imap_types::flag::StoreResponse;
use imap_next::imap_types::flag::StoreType;
use imap_next::imap_types::mailbox::Mailbox as ImapMailbox;
use imap_next::imap_types::response::Code;
use imap_next::imap_types::response::Data;
use imap_next::imap_types::response::Response;
use imap_next::imap_types::response::StatusKind;
use imap_next::imap_types::search::SearchKey;
use imap_next::imap_types::sequence::SequenceSet;

use super::fetch;
use super::names::mailbox_name;
use super::names::store_flag;
use super::names::uid_set;
use super::respond::tagged;
use super::respond::untagged_ok;
use super::respond::Failure;
use super::Session;
use super::State;
use crate::imap::search::matches;
use crate::imap::search::needs_content;
use crate::imap::search::Candidate;
use crate::imap::search::Content;
use crate::store::Flag;
use crate::store::FlagChange;

impl Session {
    /// The responses to the message command `body`.
    pub(super) async fn message_command(
        &mut self,
        tag: &Tag<'static>,
        body: CommandBody<'static>,
    ) -> Result<Vec<Response<'static>>, Failure> {
        match body {
            CommandBody::Close => self.close(tag).await,
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
            _ => Err(Failure::bad("command not supported")),
        }
    }

    /// CLOSE: expunge the selected mailbox unless it is read-only, and leave it.
    async fn close(&mut self, tag: &Tag<'static>) -> Result<Vec<Response<'static>>, Failure> {
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
            .map_err(Failure::try_create)?;
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
            if let Some(code) = code {
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
