//! The gateway's DHT side: whether a DID takes mail, and the write of one mail.
//!
//! ```text
//!   recipient(d)  = μ(d).recipient()   μ(d) = the newest descriptor on descriptor_topic(d)
//!                                             that is signed for d, binds its key, verifies
//!                                             and is live
//!                 = ⊥                  no such descriptor                       (RCPT: 451)
//!   deliver(d, m) = Extend(mail_topic(d), seal(m, ek(μ(d))), lifetime = MAX_TTL_MS),
//!                   reported under StoreQuorum::Any:
//!                 = Ok    at the first owner's Stored report                    (DATA: 250)
//!                 = Err   once every owner refused, or no report within W       (DATA: 451)
//!                                                          W = MAIL_STORE_REPORT_WAIT
//! ```
//!
//! A DID with no live descriptor is `451`, not `550`: a gateway that keeps nothing cannot tell
//! a recipient that is away from one that never took mail, and the sender's retries reach one
//! that comes back.
//!
//! The gateway keeps nothing: each RCPT is one fetch, each DATA one reported write. `deliver`
//! returning `Ok` means an owner reported holding the mail (#866), so a recipient's fetch can
//! reach it; the writer's own read-back would be its cache (#982), not an owner's statement. One
//! owner is enough: waiting for every one would let one slow or refusing owner turn mail that
//! is held into a retry. A `451` is not a loss: the sender retries, and a copy an owner did hold
//! is a duplicate the recipient files once (by `Message-ID`).

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::select;
use futures::future::Either;
use futures::pin_mut;
use rand::rngs::OsRng;
use rings_core::consts::MAX_TTL_MS;
use rings_core::dht::entry::Entry;
use rings_core::lifecycle::StopSource;
use rings_core::swarm::storage_api::StoreQuorum;
use rings_core::swarm::storage_api::StoreUnconfirmed;
use rings_core::utils::get_epoch_ms;
use rings_runtime::sleep;
use rings_runtime::MaybeSendSync;
use thiserror::Error;

use super::descriptor::descriptor_topic;
use super::descriptor::MailDescriptor;
use super::descriptor::MailRecipient;
use super::envelope::seal;
use super::envelope::MailDigest;
use super::envelope::SealError;
use super::envelope::SealedMail;
use super::mail_topic;
use crate::descriptor::heard_descriptors;
use crate::prelude::rings_core::dht::Did;
use crate::processor::Processor;

/// How long the write of one mail waits for its first owner's store report, the rerouting of
/// its placements included: far above an overlay round trip, and well inside the ten minutes
/// an SMTP client waits for the reply to its final dot (RFC 5321 §4.5.3.2.6), so the client
/// reads a `451`, not its own timeout.
pub const MAIL_STORE_REPORT_WAIT: Duration = Duration::from_secs(60);

/// Why the gateway could not look a recipient up or write its mail.
#[derive(Debug, Error)]
pub enum MailGatewayError {
    /// The DHT key of the recipient's descriptor topic cannot be derived.
    #[error("no DHT key for {did}'s mail descriptor topic: {source}")]
    Key {
        /// The recipient.
        did: Did,
        /// Why.
        source: rings_core::error::Error,
    },
    /// The fetch of the recipient's descriptor topic failed.
    #[error("cannot fetch {did}'s mail descriptor: {source}")]
    Fetch {
        /// The recipient.
        did: Did,
        /// Why.
        source: crate::error::Error,
    },
    /// The message could not be sealed; [`SealError::TooLarge`] is the SMTP `552`.
    #[error(transparent)]
    Seal(#[from] SealError),
    /// The write of the sealed mail failed: a placement could not leave this node, as
    /// `ChordStorageInterface` states, or its rerouting outlived [`MAIL_STORE_REPORT_WAIT`].
    #[error("cannot write {did}'s mail: {source}")]
    Write {
        /// The recipient.
        did: Did,
        /// Why.
        source: crate::error::Error,
    },
    /// No owner stored the mail: every one refused it, or this node could not await their
    /// reports.
    #[error("no owner stored {did}'s mail: {cause:?}")]
    Unstored {
        /// The recipient.
        did: Did,
        /// Why the receipt resolved unstored.
        cause: StoreUnconfirmed,
    },
    /// No owner reported the mail within [`MAIL_STORE_REPORT_WAIT`].
    #[error("no owner reported {did}'s mail in time")]
    Unreported {
        /// The recipient.
        did: Did,
    },
}

/// The gateway's DHT side as its SMTP front sees it: the RCPT lookup and the DATA write. The
/// node's is [`MailGateway`]; a front's tests stand in another.
#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
pub trait MailDrop: MaybeSendSync {
    /// Where mail to `did` goes and the key it is sealed to, if `did` has a live mail
    /// descriptor: the RCPT rule, which looks it up once per RCPT.
    async fn recipient(&self, did: Did) -> Result<Option<MailRecipient>, MailGatewayError>;

    /// Seal `message` for `recipient`, write it to the recipient's mail topic as one delta that
    /// lives [`MAX_TTL_MS`], and await an owner's report that it holds it: the DATA rule.
    /// Returns the mail's id.
    async fn deliver(
        &self,
        recipient: &MailRecipient,
        message: &[u8],
    ) -> Result<MailDigest, MailGatewayError>;
}

/// The gateway over a node's DHT, signing manifests with the node's authority.
pub struct MailGateway {
    processor: Arc<Processor>,
}

impl MailGateway {
    /// The gateway writing through `processor`.
    pub fn new(processor: Arc<Processor>) -> Self {
        Self { processor }
    }
}

#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
impl MailDrop for MailGateway {
    async fn recipient(&self, did: Did) -> Result<Option<MailRecipient>, MailGatewayError> {
        let key = Entry::gen_did(&descriptor_topic(did))
            .map_err(|source| MailGatewayError::Key { did, source })?;
        let entry = self
            .processor
            .fetch_storage_entry(key)
            .await
            .map_err(|source| MailGatewayError::Fetch { did, source })?;
        let now_ms = get_epoch_ms();
        let network_id = self.processor.swarm.network_id();
        Ok(entry.and_then(|entry| {
            MailDescriptor::live_recipient(heard_descriptors(&entry), did, now_ms, network_id)
        }))
    }

    async fn deliver(
        &self,
        recipient: &MailRecipient,
        message: &[u8],
    ) -> Result<MailDigest, MailGatewayError> {
        let SealedMail { mail_id, elements } = seal(message, recipient, &mut OsRng)?;
        let did = recipient.did;
        let deadline = StopSource::new();
        let stop = deadline.token();
        let stored = async {
            let receipt = self
                .processor
                .storage_extend_reported(
                    &mail_topic(did),
                    elements,
                    MAX_TTL_MS,
                    StoreQuorum::Any,
                    &stop,
                )
                .await
                .map_err(|source| MailGatewayError::Write { did, source })?;
            confirmed(did, receipt, stop.stopped()).await
        };
        stopping_after(MAIL_STORE_REPORT_WAIT, &deadline, stored).await?;
        Ok(mail_id)
    }
}

/// The verdict on a mail written to `did`'s topic: stored once its `receipt` resolves stored,
/// unstored once it resolves otherwise, unreported once `elapsed` resolves first. The receipt
/// is polled first, so a report that arrives as the wait ends still counts.
pub(super) async fn confirmed(
    did: Did,
    receipt: impl Future<Output = Result<(), StoreUnconfirmed>>,
    elapsed: impl Future<Output = ()>,
) -> Result<(), MailGatewayError> {
    pin_mut!(receipt, elapsed);
    match select(receipt, elapsed).await {
        Either::Left((Ok(()), _)) => Ok(()),
        Either::Left((Err(cause), _)) => Err(MailGatewayError::Unstored { did, cause }),
        Either::Right(((), _)) => Err(MailGatewayError::Unreported { did }),
    }
}

/// Drive `write` to its end, requesting `deadline`'s stop once `wait` has elapsed. `write` ends
/// at the stop cooperatively, never dropped midway: its storage operations run to completion
/// (`ScopedStorage`), and only its waits observe the stop. A timer that cannot be scheduled
/// stops it at once.
pub(super) async fn stopping_after<F: Future>(
    wait: Duration,
    deadline: &StopSource,
    write: F,
) -> F::Output {
    let timer = async {
        if let Err(error) = sleep(wait).await {
            tracing::debug!(
                ?error,
                "no timer for a mail's store report wait; stopping now"
            );
        }
        deadline.request_stop();
        std::future::pending::<Infallible>().await
    };
    pin_mut!(write, timer);
    match select(write, timer).await {
        Either::Left((output, _)) => output,
        Either::Right((never, _)) => match never {},
    }
}
