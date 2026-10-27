//! The mail gateway (#1117): a stateless SMTP receiver for every domain whose MX names it, which
//! writes each mail to its recipient's topic in the DHT, where the recipient's node, native or
//! in a browser, fetches it.
//!
//! ```text
//!   RCPT TO:<l@any domain>   l is not a DID                               550
//!                            l is a DID d, μ(d) live: kept with the RCPT  250
//!                            no live μ(d), or the DHT cannot tell         451
//!   DATA m                   for each distinct d among the recipients,
//!                            concurrently: deliver(μ(d), traced m)       552 above N_max chunks,
//!                                                                         451 if no owner holds
//!                                                                         a write within W
//!                            250                                          an owner of every
//!                                                                         write reported it
//! ```
//!
//! The gateway keeps nothing between sessions: RCPT looks a recipient up, and the session keeps
//! what it found for DATA (`rings_node::mail::gateway`). A DID with no live descriptor gets
//! `451`, not `550`: a gateway that keeps nothing cannot tell a recipient that is away from one
//! that never took mail, and the sender's retries reach a recipient that comes back. `250`
//! means an owner reported holding every write; a mail its recipient does not fetch within its
//! lifetime, 100 minutes, is lost. The writes run concurrently, so the reply waits about one
//! report wait `W = MAIL_STORE_REPORT_WAIT` however many recipients a mail has. Recipients that
//! are one DID at several domains (`d@a`, `d@b`) are written once.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::join_all;
use rings_node::mail::address::addressed_did;
use rings_node::mail::envelope::MAIL_MESSAGE_MAX;
use rings_node::mail::gateway::MailDrop;
use rings_node::mail::gateway::MailGatewayError;
use tokio_rustls::TlsAcceptor;

use crate::smtp::Lookup;
use crate::smtp::Recipient;
use crate::smtp::Role;
use crate::smtp::Sink;
use crate::smtp::SmtpService;

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
            Ok(Some(mailbox)) => Lookup::Admitted(mailbox),
            Ok(None) => Lookup::Offline,
            Err(error) => {
                tracing::warn!("gateway: cannot look {did} up: {error}");
                Lookup::Unavailable
            }
        }
    }

    /// Write `message` to the mail topic of every distinct DID among `to`, where RCPT found it
    /// (the DATA rule): all the writes run to their ends, concurrently, and the first failure
    /// in DID order is the reply.
    ///
    /// Pre: every recipient of `to` was admitted at RCPT, so each carries its mailbox. A
    /// message too large fails before any write, since its size alone decides it.
    pub async fn deliver(&self, to: &[Recipient], message: &[u8]) -> Result<(), MailGatewayError> {
        let mailboxes = to
            .iter()
            .filter_map(|recipient| recipient.mailbox)
            .map(|mailbox| (mailbox.did, mailbox))
            .collect::<BTreeMap<_, _>>();
        let delivered = join_all(
            mailboxes
                .values()
                .map(|mailbox| self.mail.deliver(mailbox, message)),
        )
        .await;
        for mail in delivered {
            mail?;
        }
        Ok(())
    }
}
