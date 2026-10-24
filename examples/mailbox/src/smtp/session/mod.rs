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
//! with [`Session::data`], which yields the transaction to commit. Committing (to the store or
//! the outbound queue) is the shell's effect; the session only decides.

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
}

impl Role {
    /// Whether this role accepts mail for `address`.
    pub fn accepts(&self, address: &str) -> bool {
        match self {
            Self::Submission => address.contains('@'),
            Self::Inbound { local, label } => {
                address
                    .rsplit_once('@')
                    .is_some_and(|(address_local, domain)| {
                        address_local.eq_ignore_ascii_case(local) && is_owner_domain(domain, label)
                    })
            }
        }
    }
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
    pub to: Vec<String>,
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
        to: Vec<String>,
    },
    /// The message body is being read.
    Data {
        /// The client's name.
        client: String,
        /// The reverse path.
        from: String,
        /// The recipients.
        to: Vec<String>,
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
}

/// One SMTP session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    role: Role,
    host: String,
    max_size: usize,
    max_recipients: usize,
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
            phase: Phase::Fresh,
        }
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
            Request::StartTls
            | Request::Auth { .. }
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
        Next::Reply(Reply {
            code: 250,
            status: "",
            lines: vec![
                greeting,
                format!("SIZE {}", self.max_size),
                "8BITMIME".to_owned(),
                "PIPELINING".to_owned(),
                "ENHANCEDSTATUSCODES".to_owned(),
            ],
        })
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

    /// Answer RCPT TO `address`.
    fn rcpt(&mut self, address: String) -> Next {
        let accepts = self.role.accepts(&address);
        let max_recipients = self.max_recipients;
        let Phase::Transaction { to, .. } = &mut self.phase else {
            return Next::Reply(Reply::line(503, "5.5.1", "Send MAIL first"));
        };
        if !accepts {
            return Next::Reply(Reply::line(550, "5.1.1", "No such mailbox here"));
        }
        if to.len() >= max_recipients {
            return Next::Reply(Reply::line(452, "4.5.3", "Too many recipients"));
        }
        to.push(address);
        Next::Reply(Reply::line(250, "2.1.5", "OK"))
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

#[cfg(test)]
mod test_session;
