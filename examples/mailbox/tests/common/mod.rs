//! Helpers shared by the mailbox's integration tests: a started mailbox, and plain SMTP and
//! IMAP clients that speak the protocols line by line, as a mail client would.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use rings_mailbox_example::mailbox::start;
use rings_mailbox_example::mailbox::MailboxConfig;
use rings_mailbox_example::mailbox::Running;
use rings_mailbox_example::outbound::Exchangers;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;

/// How long a test waits for an event before it fails.
pub const DEADLINE: Duration = Duration::from_secs(20);

/// Exchangers the test sets: domain to address.
#[derive(Default)]
pub struct FixedExchangers(pub Mutex<BTreeMap<String, SocketAddr>>);

#[async_trait]
impl Exchangers for FixedExchangers {
    async fn exchangers(&self, domain: &str) -> Result<Vec<SocketAddr>, String> {
        self.0
            .lock()
            .unwrap()
            .get(&domain.to_ascii_lowercase())
            .map(|address| vec![*address])
            .ok_or_else(|| format!("no exchanger for {domain}"))
    }
}

/// A started mailbox for the owner `label`, in `dir`, finding exchangers with `exchangers`.
pub async fn mailbox(
    dir: &std::path::Path,
    label: &str,
    exchangers: Arc<dyn Exchangers>,
) -> Running {
    let loopback = "127.0.0.1:0".parse().unwrap();
    start(
        &MailboxConfig {
            dir: dir.to_path_buf(),
            label: label.to_owned(),
            local: "me".to_owned(),
            smtp: loopback,
            submission: loopback,
            imap: loopback,
            max_message_size: 1024 * 1024,
        },
        exchangers,
    )
    .await
    .unwrap()
}

/// A plain client for a line protocol.
pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    /// Connect to `address`.
    pub async fn connect(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).await.unwrap();
        let (read, writer) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer,
        }
    }

    /// Send `text` as is.
    pub async fn send(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).await.unwrap();
    }

    /// Read one line, without its line break; with any literal it announces (`{n}`) read in.
    pub async fn line(&mut self) -> String {
        let mut line = String::new();
        loop {
            let mut chunk = String::new();
            let read = tokio::time::timeout(DEADLINE, self.reader.read_line(&mut chunk))
                .await
                .expect("the server answers in time")
                .unwrap();
            assert!(read > 0, "the server closed the connection after {line:?}");
            let chunk = chunk.trim_end_matches(['\r', '\n']).to_owned();
            let literal = chunk
                .strip_suffix('}')
                .and_then(|head| head.rsplit_once('{'))
                .and_then(|(_, len)| len.parse::<usize>().ok());
            line.push_str(&chunk);
            let Some(len) = literal else {
                return line;
            };
            let mut bytes = vec![0u8; len];
            self.reader.read_exact(&mut bytes).await.unwrap();
            line.push_str("\r\n");
            line.push_str(&String::from_utf8_lossy(&bytes));
        }
    }

    /// Read an SMTP reply: its lines, up to the one with a space after the code.
    pub async fn smtp_reply(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let line = self.line().await;
            let last = line.as_bytes().get(3) == Some(&b' ');
            lines.push(line);
            if last {
                return lines;
            }
        }
    }

    /// Send the SMTP command `command` and return the reply's code.
    pub async fn smtp(&mut self, command: &str) -> u16 {
        self.send(&format!("{command}\r\n")).await;
        let reply = self.smtp_reply().await;
        reply.last().unwrap()[..3].parse().unwrap()
    }

    /// Send the IMAP command `command` under `tag`; return every response line, the tagged one
    /// last.
    pub async fn imap(&mut self, tag: &str, command: &str) -> Vec<String> {
        self.send(&format!("{tag} {command}\r\n")).await;
        self.until_tagged(tag).await
    }

    /// Read response lines up to the one tagged `tag`.
    pub async fn until_tagged(&mut self, tag: &str) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let line = self.line().await;
            let done = line.starts_with(&format!("{tag} "));
            lines.push(line);
            if done {
                return lines;
            }
        }
    }
}

/// Deliver `message` from `from` to `to` over SMTP at `address`; return the final reply code.
pub async fn deliver(address: SocketAddr, from: &str, to: &str, message: &str) -> u16 {
    let mut client = Client::connect(address).await;
    assert_eq!(
        client.smtp_reply().await.last().unwrap()[..3].to_owned(),
        "220"
    );
    assert_eq!(client.smtp("EHLO test.example").await, 250);
    assert_eq!(client.smtp(&format!("MAIL FROM:<{from}>")).await, 250);
    let rcpt = client.smtp(&format!("RCPT TO:<{to}>")).await;
    if rcpt != 250 {
        return rcpt;
    }
    assert_eq!(client.smtp("DATA").await, 354);
    let code = client.smtp(&format!("{message}\r\n.")).await;
    assert_eq!(client.smtp("QUIT").await, 221);
    code
}

/// A short message from `from` to `to` with `subject`.
pub fn message(from: &str, to: &str, subject: &str, body: &str) -> String {
    format!(
        "From: Alice <{from}>\r\nTo: <{to}>\r\nSubject: {subject}\r\nDate: Sat, 24 Oct 2026 \
         12:00:00 +0000\r\nMessage-ID: <{subject}@test>\r\n\r\n{body}"
    )
}

/// An IMAP client logged in to `running`.
pub async fn logged_in(running: &Running) -> Client {
    let mut client = Client::connect(running.imap).await;
    assert!(client.line().await.starts_with("* OK"));
    let login = client
        .imap(
            "a0",
            &format!("LOGIN {} {}", running.user, running.password),
        )
        .await;
    assert!(login.last().unwrap().starts_with("a0 OK"), "{login:?}");
    client
}
