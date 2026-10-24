//! The mailbox as one program: its store, its three listeners and its outbound queue, started
//! from one configuration.
//!
//! ```text
//!   <dir>/store/      the mailboxes            (store)
//!   <dir>/outbound/   mail waiting to go out   (outbound)
//!   <dir>/password    the IMAP password, made on first start, readable by its owner only
//!
//!   smtp        ─▶ INBOX                 the node serves it as port 25 of <did>.<zone>
//!   submission  ─▶ outbound ─▶ MX        the user's mail client sends here
//!   imap        ─▶ store                 the user's mail client reads here
//! ```
//!
//! Every listener binds a loopback address: the node reaches the receiver on loopback through
//! its TCP relay, and submission and IMAP serve the user on this machine. Submission takes mail
//! for anyone without a password, which only a loopback listener makes safe.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use rand::distributions::Alphanumeric;
use rand::Rng;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::imap::Credentials;
use crate::imap::ImapService;
use crate::outbound::Exchangers;
use crate::outbound::Outbound;
use crate::outbound::OutboundError;
use crate::smtp::Role;
use crate::smtp::Sink;
use crate::smtp::SmtpService;
use crate::store::Store;
use crate::store::StoreError;

/// The default largest message, in bytes: 25 MiB, a common limit of mail servers.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 25 * 1024 * 1024;
/// Length of a generated password.
const PASSWORD_LEN: usize = 24;

/// Why the mailbox could not start.
#[derive(Debug, Error)]
pub enum StartError {
    /// A listener was given an address other than loopback.
    #[error("{0} is not a loopback address; the mailbox listens on loopback only")]
    NotLoopback(SocketAddr),
    /// A listener could not bind.
    #[error("cannot listen on {address}: {source}")]
    Bind {
        /// The address.
        address: SocketAddr,
        /// The failure.
        source: std::io::Error,
    },
    /// The password file could not be read or written.
    #[error("{path}: {source}")]
    Password {
        /// The file.
        path: PathBuf,
        /// The failure.
        source: std::io::Error,
    },
    /// The store could not open.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The outbound queue could not open.
    #[error(transparent)]
    Outbound(#[from] OutboundError),
}

/// The mailbox's configuration.
#[derive(Clone, Debug)]
pub struct MailboxConfig {
    /// The directory the mailbox keeps its state in.
    pub dir: PathBuf,
    /// The owner's label: the DID's canonical text, as in `me@<label>.<zone>`.
    pub label: String,
    /// The mailbox's local part: `me`.
    pub local: String,
    /// Where the receiver listens: the target of the node's served port 25.
    pub smtp: SocketAddr,
    /// Where submission listens.
    pub submission: SocketAddr,
    /// Where IMAP listens.
    pub imap: SocketAddr,
    /// The largest message, in bytes.
    pub max_message_size: usize,
}

/// A started mailbox: where it listens, its password, and the tasks serving it.
pub struct Running {
    /// The receiver's address.
    pub smtp: SocketAddr,
    /// Submission's address.
    pub submission: SocketAddr,
    /// IMAP's address.
    pub imap: SocketAddr,
    /// The IMAP user name.
    pub user: String,
    /// The IMAP password.
    pub password: String,
    /// The store, for inspection.
    pub store: Arc<Store>,
    /// The tasks; dropping the set stops the mailbox.
    pub tasks: JoinSet<()>,
}

/// Start the mailbox of `config`, finding mail exchangers with `exchangers`.
pub async fn start(
    config: &MailboxConfig,
    exchangers: Arc<dyn Exchangers>,
) -> Result<Running, StartError> {
    for address in [config.smtp, config.submission, config.imap] {
        if !address.ip().is_loopback() {
            return Err(StartError::NotLoopback(address));
        }
    }
    let host = format!("{}.rings", config.label);
    let store = Arc::new(Store::open(&config.dir.join("store"), config.max_message_size).await?);
    let outbound = Arc::new(
        Outbound::open(
            &config.dir.join("outbound"),
            host.clone(),
            Arc::clone(&store),
            exchangers,
        )
        .await?,
    );
    let password = load_or_create_password(&config.dir.join("password")).await?;
    let smtp = bind(config.smtp).await?;
    let submission = bind(config.submission).await?;
    let imap = bind(config.imap).await?;
    let running_at = |listener: &TcpListener, configured: SocketAddr| {
        listener.local_addr().unwrap_or(configured)
    };
    let (smtp_at, submission_at, imap_at) = (
        running_at(&smtp, config.smtp),
        running_at(&submission, config.submission),
        running_at(&imap, config.imap),
    );
    let mut tasks = JoinSet::new();
    tasks.spawn(
        SmtpService {
            role: Role::Inbound {
                local: config.local.clone(),
                label: config.label.clone(),
            },
            host: host.clone(),
            max_size: config.max_message_size,
            sink: Sink::Inbox(Arc::clone(&store)),
        }
        .serve(smtp),
    );
    tasks.spawn(
        SmtpService {
            role: Role::Submission,
            host,
            max_size: config.max_message_size,
            sink: Sink::Outbound(Arc::clone(&outbound)),
        }
        .serve(submission),
    );
    tasks.spawn(
        ImapService {
            store: Arc::clone(&store),
            credentials: Credentials {
                user: config.local.clone(),
                password: password.clone(),
            },
        }
        .serve(imap),
    );
    tasks.spawn(outbound.run());
    Ok(Running {
        smtp: smtp_at,
        submission: submission_at,
        imap: imap_at,
        user: config.local.clone(),
        password,
        store,
        tasks,
    })
}

/// Bind `address`.
async fn bind(address: SocketAddr) -> Result<TcpListener, StartError> {
    TcpListener::bind(address)
        .await
        .map_err(|source| StartError::Bind { address, source })
}

/// The password stored at `path`, or a new random one, stored there readable by its owner only.
async fn load_or_create_password(path: &Path) -> Result<String, StartError> {
    let failed = |source| StartError::Password {
        path: path.to_path_buf(),
        source,
    };
    match tokio::fs::read_to_string(path).await {
        Ok(password) => Ok(password.trim().to_owned()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let password = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(PASSWORD_LEN)
                .map(char::from)
                .collect::<String>();
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(failed)?;
            }
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(path).await.map_err(failed)?;
            file.write_all(password.as_bytes()).await.map_err(failed)?;
            Ok(password)
        }
        Err(error) => Err(failed(error)),
    }
}
