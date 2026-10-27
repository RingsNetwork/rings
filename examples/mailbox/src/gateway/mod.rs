//! The mail gateway (#1117): a stateless SMTP receiver for every domain whose MX names it, which
//! writes each mail to its recipient's topic in the DHT, where the recipient's node, native or
//! in a browser, fetches it.
//!
//! ```text
//!   RCPT TO:<l@any domain>   admitted ⟺ l is a DID d ∧ μ(d) is live     else 550; 451 when
//!                                                                         the DHT cannot tell
//!   DATA m                   for each distinct d among the recipients,
//!                            concurrently: deliver(d, traced m)         552 above N_max chunks,
//!                                                                         451 if no owner holds
//!                                                                         a write within W
//!                            250                                          an owner of every
//!                                                                         write reported it
//! ```
//!
//! The gateway keeps nothing between commands: RCPT looks a recipient up, and DATA looks each
//! one up again before it writes (`rings_node::mail::gateway`). `250` means an owner reported
//! holding every write; a mail its recipient does not fetch within its lifetime, 100 minutes,
//! is lost. The writes run concurrently, so the reply waits about one report wait
//! `W = MAIL_STORE_REPORT_WAIT` however many recipients a mail has. Recipients that are one DID
//! at several domains (`d@a`, `d@b`) are written once.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::future::join_all;
use rings_node::mail::address::addressed_did;
use rings_node::mail::envelope::SealError;
use rings_node::mail::envelope::MAIL_MESSAGE_MAX;
use rings_node::mail::gateway::MailDrop;
use rings_node::mail::gateway::MailGatewayError;
use rings_node::prelude::rings_core::dht::Did;
use thiserror::Error;
use tokio_rustls::TlsAcceptor;

use crate::smtp::Lookup;
use crate::smtp::Role;
use crate::smtp::Sink;
use crate::smtp::SmtpService;

/// Why the gateway did not write a transaction's mail.
#[derive(Debug, Error)]
pub enum GatewayError {
    /// A recipient admitted at RCPT has no live descriptor any more.
    #[error("{0} takes no mail any more")]
    Gone(Did),
    /// The DHT could not look a recipient up, or no owner reported a write stored.
    #[error(transparent)]
    Dht(#[from] MailGatewayError),
}

impl GatewayError {
    /// Whether the message is too large for a mail ([`SealError::TooLarge`]): `552`.
    pub fn is_too_large(&self) -> bool {
        matches!(
            self,
            Self::Dht(MailGatewayError::Seal(SealError::TooLarge { .. }))
        )
    }
}

/// The gateway over the DHT side `mail`: a node's (`rings_node::mail::gateway::MailGateway`), or
/// a test's stand-in.
pub struct Gateway {
    mail: Arc<dyn MailDrop>,
}

impl Gateway {
    /// The gateway writing through `mail`.
    pub fn new(mail: Arc<dyn MailDrop>) -> Self {
        Self { mail }
    }

    /// The SMTP listener of this gateway, greeting as `host`, offering `STARTTLS` with `tls`
    /// when given: the directory role, a message bound of [`MAIL_MESSAGE_MAX`], no PROXY header.
    pub fn smtp_service(self: Arc<Self>, host: String, tls: Option<TlsAcceptor>) -> SmtpService {
        SmtpService {
            role: Role::Directory,
            host,
            max_size: MAIL_MESSAGE_MAX,
            sink: Sink::Gateway(self),
            proxy_protocol: false,
            tls,
        }
    }

    /// What the DHT says of the recipient `address` (the RCPT rule).
    pub async fn lookup(&self, address: &str) -> Lookup {
        let Some(did) = addressed_did(address) else {
            return Lookup::Refused;
        };
        match self.mail.recipient(did).await {
            Ok(Some(_)) => Lookup::Admitted,
            Ok(None) => Lookup::Refused,
            Err(error) => {
                tracing::warn!("gateway: cannot look {did} up: {error}");
                Lookup::Unavailable
            }
        }
    }

    /// Write `message` to the mail topic of every distinct DID `to` names (the DATA rule): all
    /// the writes run to their ends, concurrently, and the first failure in DID order is the
    /// reply.
    ///
    /// Pre: every address of `to` was admitted at RCPT, so each names a DID; one that no longer
    /// does is skipped. A message too large fails before any write, since its size alone
    /// decides it.
    pub async fn deliver(&self, to: &[String], message: &[u8]) -> Result<(), GatewayError> {
        let recipients = to
            .iter()
            .filter_map(|address| addressed_did(address))
            .collect::<BTreeSet<_>>();
        join_all(
            recipients
                .into_iter()
                .map(|did| self.deliver_to(did, message)),
        )
        .await
        .into_iter()
        .collect()
    }

    /// Look `did` up again and write `message` to its mail topic.
    async fn deliver_to(&self, did: Did, message: &[u8]) -> Result<(), GatewayError> {
        let recipient = self
            .mail
            .recipient(did)
            .await?
            .ok_or(GatewayError::Gone(did))?;
        self.mail.deliver(&recipient, message).await?;
        Ok(())
    }
}
