//! Standalone CLI entrypoint for the Rings signature-authenticated TURN server.

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rings_core::dht::Did;
use rings_core::utils::SystemClock;
use rings_turn::RelayAddressConfig;
use rings_turn::TurnAdmissionPolicy;
use rings_turn::TurnServer;
use rings_turn::TurnServerConfig;
use rings_turn::DEFAULT_PORT_RANGE_MAX_RETRIES;
use rings_turn::DEFAULT_TURN_REALM;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "rings-turn",
    version,
    about = "Signature-authenticated RFC 5766 TURN server for Rings Network"
)]
struct Cli {
    /// UDP socket address to listen on for STUN/TURN requests.
    #[arg(long, env = "RINGS_TURN_LISTEN", default_value = "0.0.0.0:3478")]
    listen: SocketAddr,

    /// TURN authentication realm advertised in 401/438 challenges.
    #[arg(long, env = "RINGS_TURN_REALM", default_value = DEFAULT_TURN_REALM)]
    realm: String,

    /// Public IPv4/IPv6 address advertised in XOR-RELAYED-ADDRESS.
    /// Defaults to the listener IP (or 127.0.0.1 when listening on 0.0.0.0).
    #[arg(long, env = "RINGS_TURN_PUBLIC_IP")]
    public_ip: Option<IpAddr>,

    /// Local IPv4/IPv6 address bound when allocating relay UDP sockets.
    /// Defaults to the listener IP.
    #[arg(long, env = "RINGS_TURN_RELAY_BIND_IP")]
    relay_bind_ip: Option<IpAddr>,

    /// Inclusive lower bound for allocated relay UDP ports.
    #[arg(long, env = "RINGS_TURN_MIN_PORT")]
    min_port: Option<u16>,

    /// Inclusive upper bound for allocated relay UDP ports.
    #[arg(long, env = "RINGS_TURN_MAX_PORT")]
    max_port: Option<u16>,

    /// ChannelBind timeout in seconds (0 uses the RFC 5766 default of 600s).
    #[arg(
        long,
        env = "RINGS_TURN_CHANNEL_BIND_TIMEOUT_SECS",
        default_value_t = 0
    )]
    channel_bind_timeout_secs: u64,

    /// Optional allowlist of delegator DIDs allowed to allocate TURN relays.
    /// When omitted, any cryptographically valid and live Rings Delegation is admitted.
    #[arg(
        long = "allow-did",
        env = "RINGS_TURN_ALLOW_DIDS",
        value_delimiter = ','
    )]
    allow_dids: Vec<Did>,
}

impl Cli {
    fn into_config(self) -> Result<TurnServerConfig, String> {
        let bind_ip = self.relay_bind_ip.unwrap_or_else(|| self.listen.ip());
        let relay_ip = self.public_ip.unwrap_or_else(|| {
            if self.listen.ip().is_unspecified() {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            } else {
                self.listen.ip()
            }
        });

        let relay = match (self.min_port, self.max_port) {
            (None, None) => RelayAddressConfig::Static { relay_ip, bind_ip },
            (Some(min_port), Some(max_port)) => RelayAddressConfig::Range {
                relay_ip,
                bind_ip,
                min_port,
                max_port,
                max_retries: DEFAULT_PORT_RANGE_MAX_RETRIES,
            },
            (Some(_), None) | (None, Some(_)) => {
                return Err(
                    "--min-port and --max-port must be specified together when constraining relay ports"
                        .to_string(),
                );
            }
        };
        relay.validate().map_err(|e| e.to_string())?;

        let admission = if self.allow_dids.is_empty() {
            TurnAdmissionPolicy::AllowAnyDelegation
        } else {
            TurnAdmissionPolicy::allowlist(self.allow_dids)
        };

        Ok(TurnServerConfig {
            listen_addr: self.listen,
            realm: self.realm,
            relay,
            channel_bind_timeout: Duration::from_secs(self.channel_bind_timeout_secs),
            admission,
            clock: Arc::new(SystemClock),
        })
    }
}

#[tokio::main]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();
    let config = match cli.into_config() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "invalid rings-turn configuration");
            std::process::exit(2);
        }
    };

    let server = match TurnServer::start(config).await {
        Ok(server) => server,
        Err(error) => {
            tracing::error!(%error, "failed to start rings-turn server");
            std::process::exit(1);
        }
    };

    tracing::info!(
        listen_addr = %server.local_addr(),
        realm = %server.realm(),
        "rings-turn server started; waiting for Ctrl-C"
    );

    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "failed to listen for ctrl-c signal");
    }

    tracing::info!("shutting down rings-turn server");
    if let Err(error) = server.close().await {
        tracing::error!(%error, "error while closing rings-turn server");
        std::process::exit(1);
    }
}
