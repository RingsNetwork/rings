//! `rings-mailbox`: the mailbox `me@<did>.<zone>` as a program a Rings node serves.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use rings_mailbox_example::mailbox::start;
use rings_mailbox_example::mailbox::MailboxConfig;
use rings_mailbox_example::mailbox::DEFAULT_MAX_MESSAGE_SIZE;
use rings_mailbox_example::outbound::DnsExchangers;

/// The mailbox `me@<did>.<zone>`: SMTP in, IMAP out, served through a Rings DNS relay.
#[derive(Debug, Parser)]
struct Args {
    /// The node's DID, as `rings run` prints it (`0x` and 40 hex digits).
    #[arg(long)]
    did: String,
    /// The mailbox's local part.
    #[arg(long, default_value = "me")]
    local: String,
    /// The directory to keep the mailbox in.
    #[arg(long, default_value = "~/.rings/mailbox")]
    dir: String,
    /// Where the receiver listens: map the node's served port 25 here.
    #[arg(long, default_value = "127.0.0.1:2525")]
    smtp: SocketAddr,
    /// Where submission listens, for the user's mail client.
    #[arg(long, default_value = "127.0.0.1:1587")]
    submission: SocketAddr,
    /// Where IMAP listens, for the user's mail client.
    #[arg(long, default_value = "127.0.0.1:1143")]
    imap: SocketAddr,
}

/// `path` with a leading `~` replaced by the home directory.
fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Start the mailbox from the command line and serve until interrupted.
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let label = args.did.to_ascii_lowercase();
    let exchangers = match DnsExchangers::system() {
        Ok(exchangers) => Arc::new(exchangers),
        Err(error) => {
            eprintln!("cannot read the system's DNS configuration: {error}");
            return ExitCode::FAILURE;
        }
    };
    let config = MailboxConfig {
        dir: expand_home(&args.dir),
        label: label.clone(),
        local: args.local.clone(),
        smtp: args.smtp,
        submission: args.submission,
        imap: args.imap,
        max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
    };
    let mut running = match start(&config, exchangers).await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("mailbox:     {}@{label}.<zone>", args.local);
    println!(
        "receiver:    {}  (serve it as port 25 in the node's services)",
        running.smtp
    );
    println!(
        "submission:  {}  (no password, loopback only)",
        running.submission
    );
    println!(
        "imap:        {}  user {}  password in {}",
        running.imap,
        running.user,
        config.dir.join("password").display()
    );
    tokio::select! {
        _ = tokio::signal::ctrl_c() => ExitCode::SUCCESS,
        _ = running.tasks.join_next() => ExitCode::FAILURE,
    }
}
