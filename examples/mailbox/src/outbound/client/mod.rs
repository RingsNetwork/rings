//! The SMTP client of the outbound queue (RFC 5321): one message to one recipient per
//! connection, STARTTLS when the exchanger offers it (RFC 3207), plain otherwise.
//!
//! ```text
//!   ─▶ 220 ─EHLO─▶ 250 ─[STARTTLS─▶ 220 ─TLS─▶ EHLO─▶ 250] ─MAIL─▶ 250 ─RCPT─▶ 25x
//!      ─DATA─▶ 354 ─message.─▶ 250 ─QUIT
//! ```
//!
//! TLS is opportunistic (RFC 7435): any certificate is accepted, as mail servers rarely hold
//! one for the address mail reaches them at, so it protects a delivery from passive eavesdroppers
//! and authenticates no one, as delivery without DANE or MTA-STS does. Every wait is bounded by
//! the timeouts of RFC 5321 §4.5.3.2.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smtp_proto::response::parser::ResponseReceiver;
use smtp_proto::Response;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::rustls;
use tokio_rustls::TlsConnector;

/// How long the TCP connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one block of the message may take to write (RFC 5321 §4.5.3.2.5).
const DATA_BLOCK_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// Bytes one read takes at most.
const READ_CHUNK: usize = 4 * 1024;

/// Where in a delivery the client is: what it waits for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    /// The TCP connection.
    Connect,
    /// The server's greeting.
    Greeting,
    /// The reply to EHLO.
    Ehlo,
    /// The reply to STARTTLS, and the TLS handshake.
    StartTls,
    /// The reply to MAIL FROM.
    MailFrom,
    /// The reply to RCPT TO.
    RcptTo,
    /// The reply to DATA.
    Data,
    /// The message's transfer, and the reply to its end.
    Message,
}

impl Step {
    /// How long the server may take to answer this step (RFC 5321 §4.5.3.2).
    fn timeout(self) -> Duration {
        let minutes = match self {
            Self::Connect => return CONNECT_TIMEOUT,
            Self::Data => 2,
            Self::Message => 10,
            Self::Greeting | Self::Ehlo | Self::StartTls | Self::MailFrom | Self::RcptTo => 5,
        };
        Duration::from_secs(minutes * 60)
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Connect => "connect",
            Self::Greeting => "greeting",
            Self::Ehlo => "EHLO",
            Self::StartTls => "STARTTLS",
            Self::MailFrom => "MAIL FROM",
            Self::RcptTo => "RCPT TO",
            Self::Data => "DATA",
            Self::Message => "message",
        })
    }
}

/// Why a delivery failed.
#[derive(Debug, Error)]
pub enum SendError {
    /// The connection failed.
    #[error("{step}: {source}")]
    Io {
        /// Where.
        step: Step,
        /// Why.
        source: std::io::Error,
    },
    /// The server did not answer in time.
    #[error("{0}: no answer in time")]
    Timeout(Step),
    /// The server refused.
    #[error("{step}: {code} {text}")]
    Refused {
        /// Where.
        step: Step,
        /// The reply code.
        code: u16,
        /// The reply text.
        text: String,
    },
    /// The server's reply does not parse.
    #[error("{step}: malformed reply ({error:?})")]
    Malformed {
        /// Where.
        step: Step,
        /// Why.
        error: smtp_proto::Error,
    },
    /// TLS cannot be set up on this side.
    #[error("TLS: {0}")]
    Tls(#[source] rustls::Error),
    /// The server sent bytes after its reply to STARTTLS, before TLS began: they would pass for
    /// replies under TLS though anyone on the path could have written them (RFC 3207 §5).
    #[error("STARTTLS: bytes before the TLS handshake")]
    Injected,
}

impl SendError {
    /// Whether the server refused for good (a 5yz reply): trying again cannot help.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Refused { code, .. } if *code >= 500)
    }
}

/// One message for one recipient, sent as `helo`.
#[derive(Clone, Copy, Debug)]
pub struct Delivery<'a> {
    /// The name the client greets with.
    pub helo: &'a str,
    /// The reverse path; empty for a bounce.
    pub from: &'a str,
    /// The recipient.
    pub recipient: &'a str,
    /// The message.
    pub message: &'a [u8],
}

/// Deliver `delivery` to the exchanger at `exchanger`.
pub async fn deliver(exchanger: SocketAddr, delivery: Delivery<'_>) -> Result<(), SendError> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(exchanger))
        .await
        .map_err(|_elapsed| SendError::Timeout(Step::Connect))?
        .map_err(|source| SendError::Io {
            step: Step::Connect,
            source,
        })?;
    let mut plain = Wire::new(stream);
    plain.expect(Step::Greeting, &[220]).await?;
    let extensions = plain.ehlo(delivery.helo).await?;
    if !offers_starttls(&extensions) {
        return plain.transaction(delivery).await;
    }
    plain
        .command(Step::StartTls, "STARTTLS\r\n", &[220])
        .await?;
    if !plain.unparsed.is_empty() {
        return Err(SendError::Injected);
    }
    let tls = opportunistic_tls()
        .map_err(SendError::Tls)?
        .connect(server_name(exchanger), plain.stream)
        .await;
    let mut tls = Wire::new(tls.map_err(|source| SendError::Io {
        step: Step::StartTls,
        source,
    })?);
    tls.ehlo(delivery.helo).await?;
    tls.transaction(delivery).await
}

/// Whether the EHLO reply `extensions` offers STARTTLS: one of the lines after the greeting
/// names it.
pub fn offers_starttls(extensions: &str) -> bool {
    extensions
        .lines()
        .skip(1)
        .any(|line| line.trim().eq_ignore_ascii_case("STARTTLS"))
}

/// `message` as DATA carries it (RFC 5321 §4.5.2): CRLF line ends, every line that starts with
/// `.` given one more, and the closing `.` line.
pub fn data_of(message: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(message.len().saturating_add(5));
    let mut line_start = true;
    let mut previous = 0u8;
    for &byte in message {
        if line_start && byte == b'.' {
            data.push(b'.');
        }
        if byte == b'\n' && previous != b'\r' {
            data.push(b'\r');
        }
        data.push(byte);
        line_start = byte == b'\n';
        previous = byte;
    }
    if !line_start {
        data.extend_from_slice(b"\r\n");
    }
    data.extend_from_slice(b".\r\n");
    data
}

/// One side of an SMTP conversation: the stream and what was read of it but not yet parsed.
struct Wire<S> {
    stream: S,
    unparsed: Vec<u8>,
}

impl<S> Wire<S>
where S: AsyncRead + AsyncWrite + Unpin
{
    /// A conversation over `stream`.
    fn new(stream: S) -> Self {
        Self {
            stream,
            unparsed: Vec::new(),
        }
    }

    /// Greet as `helo`; return the reply's text, its extension lines included.
    async fn ehlo(&mut self, helo: &str) -> Result<String, SendError> {
        let command = format!("EHLO {helo}\r\n");
        self.command(Step::Ehlo, &command, &[250]).await
    }

    /// MAIL, RCPT, DATA and the message of `delivery`, then QUIT.
    async fn transaction(&mut self, delivery: Delivery<'_>) -> Result<(), SendError> {
        let mail = format!("MAIL FROM:<{}>\r\n", delivery.from);
        self.command(Step::MailFrom, &mail, &[250]).await?;
        let rcpt = format!("RCPT TO:<{}>\r\n", delivery.recipient);
        self.command(Step::RcptTo, &rcpt, &[250, 251]).await?;
        self.command(Step::Data, "DATA\r\n", &[354]).await?;
        let data = data_of(delivery.message);
        timeout(DATA_BLOCK_TIMEOUT, self.stream.write_all(&data))
            .await
            .map_err(|_elapsed| SendError::Timeout(Step::Message))?
            .map_err(|source| SendError::Io {
                step: Step::Message,
                source,
            })?;
        self.expect(Step::Message, &[250]).await?;
        // The message is delivered; a failed QUIT changes nothing.
        let _quit = self.stream.write_all(b"QUIT\r\n").await;
        Ok(())
    }

    /// Send `command`, then read the reply, which must have a code in `expected`; return its
    /// text.
    async fn command(
        &mut self,
        step: Step,
        command: &str,
        expected: &[u16],
    ) -> Result<String, SendError> {
        self.stream
            .write_all(command.as_bytes())
            .await
            .map_err(|source| SendError::Io { step, source })?;
        self.expect(step, expected).await
    }

    /// Read the reply to `step`, which must have a code in `expected`; return its text.
    async fn expect(&mut self, step: Step, expected: &[u16]) -> Result<String, SendError> {
        let reply = timeout(step.timeout(), self.reply(step))
            .await
            .map_err(|_elapsed| SendError::Timeout(step))??;
        if !expected.contains(&reply.code) {
            return Err(SendError::Refused {
                step,
                code: reply.code,
                text: reply.message,
            });
        }
        Ok(reply.message)
    }

    /// Read one whole reply, its lines joined.
    async fn reply(&mut self, step: Step) -> Result<Response<String>, SendError> {
        let mut receiver = ResponseReceiver::default();
        let mut chunk = vec![0u8; READ_CHUNK];
        loop {
            let mut bytes = self.unparsed.iter();
            let parsed = receiver.parse(&mut bytes);
            let rest = bytes.as_slice().to_vec();
            self.unparsed = rest;
            match parsed {
                Ok(reply) => return Ok(reply),
                Err(smtp_proto::Error::NeedsMoreData { .. }) => {}
                Err(error) => return Err(SendError::Malformed { step, error }),
            }
            let read = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|source| SendError::Io { step, source })?;
            if read == 0 {
                let source = std::io::ErrorKind::UnexpectedEof.into();
                return Err(SendError::Io { step, source });
            }
            self.unparsed
                .extend_from_slice(chunk.get(..read).unwrap_or_default());
        }
    }
}

/// The name TLS presents for `exchanger`: its address, as the client knows no other.
fn server_name(exchanger: SocketAddr) -> rustls::pki_types::ServerName<'static> {
    rustls::pki_types::ServerName::IpAddress(exchanger.ip().into())
}

/// A TLS client that accepts any certificate (the module doc).
fn opportunistic_tls() -> Result<TlsConnector, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Accepts any server certificate, but checks that the server holds its key.
#[derive(Debug)]
struct AnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.0.signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.0.signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod test_client;
