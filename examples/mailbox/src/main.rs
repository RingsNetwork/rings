//! `rings-mailbox`: the mailbox `me@<did>.<zone>` as a program a Rings node serves, or the
//! mailbox `<did>@<domain>` of a node it runs, receiving through mail gateways.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use rings_mailbox_example::mailbox::start;
use rings_mailbox_example::mailbox::MailboxConfig;
use rings_mailbox_example::mailbox::DEFAULT_MAX_MESSAGE_SIZE;
use rings_mailbox_example::node;
use rings_mailbox_example::node::EmbeddedNode;
use rings_mailbox_example::outbound::DnsExchangers;
use rings_mailbox_example::receive;
use rings_node::mail::registration::MailOffer;
use rings_node::prelude::rings_core::lifecycle::StopToken;

/// The mailbox `me@<did>.<zone>`: SMTP in, IMAP out, served through a Rings DNS relay; or, with
/// `--rings-config`, the mailbox `<did>@<domain>` of the node it runs, through mail gateways.
#[derive(Debug, Parser)]
struct Args {
    /// The node's DID, as `rings run` prints it (`0x` and 40 hex digits).
    #[arg(
        long,
        required_unless_present = "rings_config",
        conflicts_with = "rings_config"
    )]
    did: Option<String>,
    /// Receive mail at `<did>@<domain>` through mail gateways (#1117): run the Rings node of
    /// this configuration instead of `rings run`, publish its mail descriptor, and file the mail
    /// it fetches in the INBOX.
    #[arg(long)]
    rings_config: Option<String>,
    /// The mailbox's local part.
    #[arg(long, default_value = "me")]
    local: String,
    /// The directory to keep the mailbox in.
    #[arg(long, default_value = "~/.rings/mailbox")]
    dir: String,
    /// Where the receiver listens: map the node's served port 25 here.
    #[arg(long, default_value = "127.0.0.1:2525")]
    smtp: SocketAddr,
    /// The receiver's connections start with a PROXY protocol v2 header naming the client: set
    /// it when the node serves port 25 with `proxy_protocol: v2`.
    #[arg(long)]
    receiver_proxy_protocol: bool,
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
    let mut node = match &args.rings_config {
        Some(config) => match node::start(&expand_home(config), Some(MailOffer::default())).await {
            Ok(node) => Some(node),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    let Some(label) = node
        .as_ref()
        .map(|node| node.processor.did().to_string())
        .or_else(|| args.did.as_deref().map(str::to_ascii_lowercase))
    else {
        eprintln!("--did or --rings-config names the mailbox");
        return ExitCode::FAILURE;
    };
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
        receiver_proxy_protocol: args.receiver_proxy_protocol,
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
    if let Some(node) = &node {
        let records = config.dir.join("recipient");
        let store = Arc::clone(&running.store);
        match receive::recipient(Arc::clone(&node.processor), store, &records).await {
            Ok(recipient) => {
                running
                    .tasks
                    .spawn(async move { recipient.run(StopToken::never()).await });
                println!("mailbox:     {label}@<any gateway domain>");
            }
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        }
    }
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
        () = node_ended(node.as_mut()) => ExitCode::FAILURE,
    }
}

/// Resolve once a task of `node` ends; never without a node.
async fn node_ended(node: Option<&mut EmbeddedNode>) {
    match node {
        Some(node) => drop(node.tasks.join_next().await),
        None => std::future::pending().await,
    }
}
