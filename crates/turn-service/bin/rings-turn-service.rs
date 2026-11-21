//! Standalone native daemon hosting `rings-turn` + `rings-node` overlay discovery and
//! `turn.ticket` Sphinx `invoke` issuance without embedding WebRTC/TURN primitives inside
//! `rings-node`.

use std::fs;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use rings_core::lifecycle::StopSource;
use rings_node::native::config::Config as NodeConfig;
use rings_node::provider::Provider;
use rings_turn::TurnServer;
use rings_turn_service::announce_turn_service;
use rings_turn_service::install_turn_client_rpc;
use rings_turn_service::register_turn_ticket_invoke_backend;
use rings_turn_service::TurnServiceConfig;
use rings_turn_service::TurnServiceDaemonConfig;
use tracing_subscriber::EnvFilter;

/// Rings TURN overlay service daemon (`rings-turn` + `rings-node`).
#[derive(Debug, Parser)]
#[command(name = "rings-turn-service", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Generate a sample `rings-turn-service.yaml` configuration file.
    Init {
        /// Output YAML file path.
        #[arg(short, long, default_value = "rings-turn-service.yaml")]
        output: PathBuf,
        /// Path to the node's `delegatee_key` file referenced by `node.delegatee_key`.
        #[arg(long, default_value = "~/.rings/delegatee_key")]
        delegatee_key: PathBuf,
        /// Public IPv4/IPv6 address advertised for TURN relay allocations.
        #[arg(long, default_value = "203.0.113.10")]
        public_ip: IpAddr,
    },
    /// Start the TURN server, publish `TurnServiceDescriptor` to the DHT, and serve `turn.ticket`.
    Run {
        /// Path to the `TurnServiceDaemonConfig` YAML file.
        #[arg(short, long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Init {
            output,
            delegatee_key,
            public_ip,
        } => {
            let mut node = NodeConfig::new(delegatee_key);
            node.advertise_onion_relay = true;
            let daemon_cfg = TurnServiceDaemonConfig {
                node,
                turn_service: TurnServiceConfig {
                    public_ip: Some(public_ip),
                    advertised_ip: Some(match public_ip {
                        IpAddr::V4(v4) => IpAddr::V4(v4),
                        IpAddr::V6(_) => IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)),
                    }),
                    ..TurnServiceConfig::default()
                },
            };
            let yaml = serde_saphyr::to_string(&daemon_cfg)
                .context("serializing TurnServiceDaemonConfig to YAML")?;
            fs::write(&output, yaml)
                .with_context(|| format!("writing config to {}", output.display()))?;
            tracing::info!(path = %output.display(), "wrote sample rings-turn-service configuration");
            Ok(())
        }
        Commands::Run { config } => {
            let raw = fs::read_to_string(&config)
                .with_context(|| format!("reading config from {}", config.display()))?;
            let daemon_cfg: TurnServiceDaemonConfig = serde_saphyr::from_str(&raw)
                .with_context(|| format!("parsing YAML config {}", config.display()))?;
            let (builder, clock) = daemon_cfg
                .to_processor_builder()
                .context("validating TurnServiceDaemonConfig")?;

            let server_cfg = daemon_cfg
                .turn_service
                .to_turn_server_config(clock)
                .context("building TurnServerConfig")?;
            let turn_server = Arc::new(
                TurnServer::start(server_cfg)
                    .await
                    .context("starting TurnServer")?,
            );
            let offer = match daemon_cfg.turn_service.service_offer(&turn_server) {
                Ok(offer) => offer,
                Err(err) => {
                    let _ = turn_server.close().await;
                    return Err(err.into());
                }
            };

            let builder = builder.replay_storage(Box::new(rings_core::storage::MemStorage::new()));
            let processor = match announce_turn_service(builder, offer).build() {
                Ok(p) => Arc::new(p),
                Err(err) => {
                    let _ = turn_server.close().await;
                    return Err(err.into());
                }
            };

            let provider = Arc::new(Provider::from_processor(Arc::clone(&processor)));
            if daemon_cfg.turn_service.issue_tickets {
                let pipeline = provider.onion_pipeline()?;
                register_turn_ticket_invoke_backend(
                    &pipeline,
                    daemon_cfg.turn_service.ticket_invoke_policy,
                    Arc::clone(&turn_server),
                )?;
            }
            install_turn_client_rpc(&provider)?;
            provider.set_backend()?;

            tracing::info!(
                did = %processor.did(),
                turn_listen = %turn_server.local_addr(),
                turn_advertised = %turn_server.advertised_addr(),
                issue_tickets = daemon_cfg.turn_service.issue_tickets,
                "rings-turn-service started"
            );

            let stop = StopSource::new();
            let listen_provider = Arc::clone(&provider);
            let listen_stop = stop.token();
            let mut tasks = tokio::task::JoinSet::new();
            tasks.spawn(async move {
                listen_provider
                    .listen_with(listen_stop)
                    .await
                    .context("provider listen_with failed")
            });

            tokio::signal::ctrl_c()
                .await
                .context("waiting for Ctrl-C signal")?;
            tracing::info!("shutting down rings-turn-service");
            stop.request_stop();
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                while tasks.join_next().await.is_some() {}
            })
            .await;
            let _ = turn_server.close().await;
            let _ = processor.close().await;
            Ok(())
        }
    }
}
