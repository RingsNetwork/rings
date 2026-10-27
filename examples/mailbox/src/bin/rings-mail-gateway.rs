//! `rings-mail-gateway`: a mail gateway (#1117), the SMTP receiver every domain whose MX names it
//! delivers `did@domain` to, writing each mail to its recipient's topic in the DHT.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use rings_mailbox_example::gateway::Gateway;
use rings_mailbox_example::node;
use rings_mailbox_example::smtp::tls;
use rings_node::mail::gateway::MailGateway;
use rings_node::util::expand_home;
use tokio::net::TcpListener;

/// A mail gateway: SMTP in, sealed mail out to the recipients' DHT topics.
#[derive(Debug, Parser)]
struct Args {
    /// The gateway node's `rings` configuration file; the gateway runs that node itself.
    #[arg(long, default_value = "~/.rings/config.yaml")]
    config: PathBuf,
    /// Where SMTP listens: port 25 of an address the domains' MX records reach.
    #[arg(long, default_value = "[::]:25")]
    smtp: SocketAddr,
    /// The name the gateway greets with: the host its MX records name.
    #[arg(long)]
    host: String,
    /// A PEM certificate chain for the gateway's host, to offer STARTTLS with.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// The PEM private key of `--tls-cert`.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
}

/// Start the gateway from the command line and serve until interrupted.
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();
    let tls = match (&args.tls_cert, &args.tls_key) {
        (Some(certificates), Some(key)) => match tls::acceptor(certificates, key) {
            Ok(acceptor) => Some(acceptor),
            Err(error) => {
                eprintln!("cannot load the TLS certificate: {error}");
                return ExitCode::FAILURE;
            }
        },
        _ => None,
    };
    let config = match expand_home(&args.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let mut node = match node::start(&config, None).await {
        Ok(node) => node,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match TcpListener::bind(args.smtp).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot listen on {}: {error}", args.smtp);
            return ExitCode::FAILURE;
        }
    };
    let mail = MailGateway::new(Arc::clone(&node.processor));
    let gateway = Arc::new(Gateway::new(Arc::new(mail)));
    let starttls = if tls.is_some() {
        "with STARTTLS"
    } else {
        "without TLS"
    };
    println!("node:     {}", node.processor.did());
    println!("gateway:  {} as {}, {starttls}", args.smtp, args.host);
    let service = gateway.smtp_service(args.host, tls).serve(listener);
    tokio::select! {
        _ = tokio::signal::ctrl_c() => ExitCode::SUCCESS,
        () = service => ExitCode::FAILURE,
        _ = node.tasks.join_next() => ExitCode::FAILURE,
    }
}
