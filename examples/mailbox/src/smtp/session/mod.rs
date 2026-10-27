//! The SMTP session (RFC 5321) as a pure state machine: commands in, replies out.
//!
//! ```text
//!            HELO/EHLO            MAIL FROM           RCPT TO (accepted)        DATA
//!   Fresh ───────────▶ Ready ───────────▶ Mail ───────────────────▶ Rcpt ─────────▶ (data)
//!     ▲                  ▲  ▲               │ RSET                   │ RSET           │ end
//!     └── RSET keeps ────┘  └───────────────┴────────────────────────┘◀───────────────┘
//! ```
//!
//! `step` answers every command; the shell reads the message body after a `354` and reports it
//! with [`Session::data`], which yields the transaction to commit. Committing (to the store, the
//! outbound queue or the DHT) is the shell's effect; the session only decides.
//!
//! Two decisions take a fact only the shell can learn, so the session asks for it:
//!
//! - a recipient of [`Role::Directory`] is looked up by the shell ([`Next::Lookup`]), which
//!   answers with [`Session::admit`]; the session keeps what the directory said with the
//!   recipient, so DATA commits it without asking again;
//! - `STARTTLS`, when the session offers it, is answered with [`Next::StartTls`]; the shell runs
//!   the handshake and reports it with [`Session::secured`], which starts the session afresh
//!   (RFC 3207 §4.2).

use rings_node::mail::descriptor::MailRecipient;
use smtp_proto::Request;

/// A reply: its code, enhanced status, and text lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reply {
    /// The three-digit reply code.
    pub code: u16,
    /// The enhanced status code (RFC 3463), empty for replies that carry none.
    pub status: &'static str,
    /// The text lines; a multi-line reply has several.
    pub lines: Vec<String>,
}

impl Reply {
    /// A one-line reply.
    fn line(code: u16, status: &'static str, text: impl Into<String>) -> Self {
        Self {
            code,
            status,
            lines: vec![text.into()],
        }
    }

    /// The reply as it is sent: every line `code-text` but the last, `code text`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let last = self.lines.len().saturating_sub(1);
        for (index, line) in self.lines.iter().enumerate() {
            let separator = if index == last { ' ' } else { '-' };
            // RFC 2034: every line of a reply carries its enhanced status code.
            let status = if self.status.is_empty() {
                String::new()
            } else {
                format!("{} ", self.status)
            };
            out.extend_from_slice(format!("{}{separator}{status}{line}\r\n", self.code).as_bytes());
        }
        out
    }
}

/// What the session accepts mail for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Role {
    /// Final delivery: only `local@<label>.<any zone>`, the mailbox's own address in every zone
    /// that relays it.
    Inbound {
        /// The mailbox's local part, such as `me`.
        local: String,
        /// The owner's label, the DID's canonical text.
        label: String,
    },
    /// Submission from the mailbox's own user: any recipient, to be delivered onward.
    Submission,
    /// Final delivery for every recipient a directory outside the session admits: the mail
    /// gateway (#1117), whose shell looks each one up in the DHT once, at RCPT.
    Directory,
}

impl Role {
    /// Whether this role accepts mail for `address`, or `None` when a directory outside the
    /// session decides ([`Role::Directory`]).
    pub fn accepts(&self, address: &str) -> Option<bool> {
        match self {
            Self::Submission => Some(address.contains('@')),
            Self::Inbound { local, label } => Some(address.rsplit_once('@').is_some_and(
                |(address_local, domain)| {
                    address_local.eq_ignore_ascii_case(local) && is_owner_domain(domain, label)
                },
            )),
            Self::Directory => None,
        }
    }
}

/// What a directory says of a recipient: the answer to [`Next::Lookup`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lookup {
    /// The recipient takes mail here, where its mail goes: kept with it for DATA.
    Admitted(MailRecipient),
    /// No such recipient: `550`.
    Refused,
    /// The recipient takes no mail now, whether it is away or never took any, which a directory
    /// that keeps nothing cannot tell apart: `451`, so the client tries again later.
    Offline,
    /// The directory could not tell: `451`, so the client tries again later.
    Unavailable,
}

/// A recipient the session accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Recipient {
    /// The address RCPT named.
    pub address: String,
    /// Where the directory said its mail goes ([`Role::Directory`]); `None` for a role that
    /// decides by itself.
    pub mailbox: Option<MailRecipient>,
}

/// Whether the session offers `STARTTLS`, and whether TLS is up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tls {
    /// No certificate: `STARTTLS` is not offered.
    Unavailable,
    /// Offered, not yet begun.
    Offered,
    /// The connection runs over TLS.
    Active,
}

/// Whether `domain` is `label.<zone>` for some zone.
fn is_owner_domain(domain: &str, label: &str) -> bool {
    domain
        .split_once('.')
        .is_some_and(|(first, zone)| first.eq_ignore_ascii_case(label) && !zone.is_empty())
}

/// A transaction ready to commit: its envelope and message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transaction {
    /// The client's HELO/EHLO name.
    pub client: String,
    /// The reverse path, empty for a bounce.
    pub from: String,
    /// The accepted recipients.
    pub to: Vec<Recipient>,
    /// The message, dot-unstuffed, without the terminating `.`.
    pub message: Vec<u8>,
}

/// Where the session is.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Phase {
    /// No HELO or EHLO yet.
    Fresh,
    /// Greeted, no transaction.
    Ready {
        /// The client's name.
        client: String,
    },
    /// A transaction with a reverse path and its accepted recipients.
    Transaction {
        /// The client's name.
        client: String,
        /// The reverse path.
        from: String,
        /// The recipients accepted so far.
        to: Vec<Recipient>,
    },
    /// The message body is being read.
    Data {
        /// The client's name.
        client: String,
        /// The reverse path.
        from: String,
        /// The recipients.
        to: Vec<Recipient>,
    },
}

/// What the shell does after a command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Next {
    /// Send the reply and read the next command.
    Reply(Reply),
    /// Send the reply (`354`) and read the message body.
    ReadData(Reply),
    /// Send the reply and close the connection.
    Close(Reply),
    /// Look the recipient up in the directory and answer RCPT with [`Session::admit`].
    Lookup(String),
    /// Send the reply (`220`), discard every byte read after the command (RFC 3207 §5), run the
    /// TLS handshake and report it with [`Session::secured`].
    StartTls(Reply),
}

/// One SMTP session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    role: Role,
    host: String,
    max_size: usize,
    max_recipients: usize,
    tls: Tls,
    phase: Phase,
}

impl Session {
    /// A session of `role`, naming itself `host`, refusing messages above `max_size` bytes and
    /// more than `max_recipients` recipients.
    pub fn new(
        role: Role,
        host: impl Into<String>,
        max_size: usize,
        max_recipients: usize,
    ) -> Self {
        Self {
            role,
            host: host.into(),
            max_size,
            max_recipients,
            tls: Tls::Unavailable,
            phase: Phase::Fresh,
        }
    }

    /// This session, offering `STARTTLS`: its shell holds a certificate.
    pub fn offering_starttls(self) -> Self {
        Self {
            tls: Tls::Offered,
            ..self
        }
    }

    /// The TLS handshake the shell began on [`Next::StartTls`] has completed: the session starts
    /// afresh, the client to greet again, and offers `STARTTLS` no more (RFC 3207 §4.2).
    pub fn secured(&mut self) {
        self.tls = Tls::Active;
        self.phase = Phase::Fresh;
    }

    /// The greeting the server sends on connect.
    pub fn greeting(&self) -> Reply {
        Reply::line(220, "", format!("{} ESMTP rings-mailbox", self.host))
    }

    /// The largest message this session accepts.
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Answer `request`.
    pub fn step(&mut self, request: Request<String>) -> Next {
        match request {
            Request::Ehlo { host } => self.hello(host, true),
            Request::Helo { host } | Request::Lhlo { host } => self.hello(host, false),
            Request::Mail { from } => self.mail(from.address, from.size),
            Request::Rcpt { to } => self.rcpt(to.address),
            Request::Data => self.begin_data(),
            Request::Rset => {
                self.reset();
                Next::Reply(Reply::line(250, "2.0.0", "Reset"))
            }
            Request::Noop { .. } => Next::Reply(Reply::line(250, "2.0.0", "OK")),
            Request::Vrfy { .. } => Next::Reply(Reply::line(
                252,
                "2.5.0",
                "Cannot verify, will attempt delivery",
            )),
            Request::Quit => Next::Close(Reply::line(221, "2.0.0", "Bye")),
            Request::StartTls => self.start_tls(),
            Request::Auth { .. }
            | Request::Bdat { .. }
            | Request::Expn { .. }
            | Request::Help { .. }
            | Request::Etrn { .. }
            | Request::Atrn { .. }
            | Request::Burl { .. } => {
                Next::Reply(Reply::line(502, "5.5.1", "Command not implemented"))
            }
        }
    }

    /// The reply that closes a session whose client fell silent.
    pub fn timed_out(&self) -> Reply {
        let text = format!("{} closing: no command in time", self.host);
        Reply::line(421, "4.4.2", text)
    }

    /// Answer a command the shell could not parse.
    pub fn unparsable(&self) -> Next {
        Next::Reply(Reply::line(500, "5.5.2", "Syntax error"))
    }

    /// The message body of the transaction in progress has been read; return the reply and the
    /// transaction to commit. A body read in any other phase is refused.
    pub fn data(&mut self, message: Vec<u8>) -> (Option<Transaction>, Reply) {
        let Phase::Data { client, from, to } = std::mem::replace(&mut self.phase, Phase::Fresh)
        else {
            return (None, Reply::line(503, "5.5.1", "Bad sequence of commands"));
        };
        self.phase = Phase::Ready {
            client: client.clone(),
        };
        if message.len() > self.max_size {
            return (None, Reply::line(552, "5.3.4", "Message too big"));
        }
        (
            Some(Transaction {
                client,
                from,
                to,
                message,
            }),
            Reply::line(250, "2.0.0", "OK"),
        )
    }

    /// The body was larger than the session accepts; the transaction is dropped.
    pub fn data_too_large(&mut self) -> Reply {
        self.reset();
        Reply::line(552, "5.3.4", "Message too big")
    }

    /// The client's name, once greeted.
    fn client(&self) -> Option<&str> {
        match &self.phase {
            Phase::Fresh => None,
            Phase::Ready { client }
            | Phase::Transaction { client, .. }
            | Phase::Data { client, .. } => Some(client),
        }
    }

    /// Drop any transaction, keeping the greeting.
    fn reset(&mut self) {
        if let Some(client) = self.client().map(str::to_owned) {
            self.phase = Phase::Ready { client };
        }
    }

    /// Answer HELO (`extended = false`) or EHLO from `client`.
    fn hello(&mut self, client: String, extended: bool) -> Next {
        self.phase = Phase::Ready {
            client: client.clone(),
        };
        let greeting = format!("{} greets {client}", self.host);
        if !extended {
            return Next::Reply(Reply::line(250, "", greeting));
        }
        let starttls = (self.tls == Tls::Offered).then(|| "STARTTLS".to_owned());
        Next::Reply(Reply {
            code: 250,
            status: "",
            lines: [
                greeting,
                format!("SIZE {}", self.max_size),
                "8BITMIME".to_owned(),
                "PIPELINING".to_owned(),
                "ENHANCEDSTATUSCODES".to_owned(),
            ]
            .into_iter()
            .chain(starttls)
            .collect(),
        })
    }

    /// Answer STARTTLS: begin TLS when it is offered and the client has greeted, with no
    /// transaction open.
    fn start_tls(&self) -> Next {
        match (self.tls, &self.phase) {
            (Tls::Unavailable, _) => {
                Next::Reply(Reply::line(502, "5.5.1", "Command not implemented"))
            }
            (Tls::Active, _) => Next::Reply(Reply::line(503, "5.5.1", "TLS already active")),
            (Tls::Offered, Phase::Ready { .. }) => {
                Next::StartTls(Reply::line(220, "2.0.0", "Ready to start TLS"))
            }
            (Tls::Offered, Phase::Fresh) => {
                Next::Reply(Reply::line(503, "5.5.1", "Send EHLO first"))
            }
            (Tls::Offered, Phase::Transaction { .. } | Phase::Data { .. }) => {
                Next::Reply(Reply::line(503, "5.5.1", "Bad sequence of commands"))
            }
        }
    }

    /// Answer MAIL FROM `from`, declaring `size` bytes (0 when undeclared).
    fn mail(&mut self, from: String, size: usize) -> Next {
        let client = match &self.phase {
            Phase::Ready { client } => client.clone(),
            Phase::Fresh => {
                return Next::Reply(Reply::line(503, "5.5.1", "Send HELO or EHLO first"));
            }
            Phase::Transaction { .. } | Phase::Data { .. } => {
                return Next::Reply(Reply::line(503, "5.5.1", "Nested MAIL command"));
            }
        };
        if size > self.max_size {
            return Next::Reply(Reply::line(552, "5.3.4", "Message too big"));
        }
        self.phase = Phase::Transaction {
            client,
            from,
            to: Vec::new(),
        };
        Next::Reply(Reply::line(250, "2.1.0", "OK"))
    }

    /// Answer RCPT TO `address`: by the role's own decision, or by asking the shell's directory
    /// when a transaction is open to take the recipient.
    fn rcpt(&mut self, address: String) -> Next {
        if !self.in_transaction() {
            return Next::Reply(send_mail_first());
        }
        match self.role.accepts(&address) {
            Some(true) => Next::Reply(self.accept(Recipient {
                address,
                mailbox: None,
            })),
            Some(false) => Next::Reply(no_such_mailbox()),
            None => Next::Lookup(address),
        }
    }

    /// Answer RCPT TO `address`, which the directory found as `lookup`.
    pub fn admit(&mut self, address: String, lookup: Lookup) -> Reply {
        if !self.in_transaction() {
            return send_mail_first();
        }
        match lookup {
            Lookup::Admitted(mailbox) => self.accept(Recipient {
                address,
                mailbox: Some(mailbox),
            }),
            Lookup::Refused => no_such_mailbox(),
            Lookup::Offline => Reply::line(451, "4.2.1", "Mailbox not online, try again later"),
            Lookup::Unavailable => {
                Reply::line(451, "4.4.3", "Directory unavailable, try again later")
            }
        }
    }

    /// Add `recipient` to the open transaction, unless it holds as many as the session takes.
    fn accept(&mut self, recipient: Recipient) -> Reply {
        let max_recipients = self.max_recipients;
        let Phase::Transaction { to, .. } = &mut self.phase else {
            return send_mail_first();
        };
        if to.len() >= max_recipients {
            return Reply::line(452, "4.5.3", "Too many recipients");
        }
        to.push(recipient);
        Reply::line(250, "2.1.5", "OK")
    }

    /// Whether a transaction is open: MAIL was accepted and DATA not yet begun.
    fn in_transaction(&self) -> bool {
        matches!(self.phase, Phase::Transaction { .. })
    }

    /// Answer DATA.
    fn begin_data(&mut self) -> Next {
        match std::mem::replace(&mut self.phase, Phase::Fresh) {
            Phase::Transaction { client, from, to } if !to.is_empty() => {
                self.phase = Phase::Data { client, from, to };
                Next::ReadData(Reply::line(354, "", "End data with <CR><LF>.<CR><LF>"))
            }
            phase => {
                self.phase = phase;
                Next::Reply(Reply::line(503, "5.5.1", "No valid recipients"))
            }
        }
    }
}

/// The reply to RCPT outside a transaction.
fn send_mail_first() -> Reply {
    Reply::line(503, "5.5.1", "Send MAIL first")
}

/// The reply to RCPT for a recipient not taken here.
fn no_such_mailbox() -> Reply {
    Reply::line(550, "5.1.1", "No such mailbox here")
}

#[cfg(test)]
mod test_session;
