//! Standalone native daemon hosting `rings-turn` + `rings-node` overlay discovery and
//! `turn.ticket` Sphinx `invoke` issuance without embedding WebRTC/TURN primitives inside
//! `rings-node`.

use std::fs;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use rings_node::native::api_auth::load_or_create_api_token;
use rings_node::native::config::Config as NodeConfig;
use rings_node::native::daemon::NativeNodeRunOptions;
use rings_node::seed::Seed;
use rings_node::util::loader::ResourceLoader;
use rings_turn_service::run_turn_service_daemon;
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
    /// Start the TURN server and full Rings foreground node (persistent `NodeStores`, bootstrap
    /// supervisor, JSON-RPC endpoints, delegation horizon, `TurnServiceDescriptor` DHT publisher,
    /// and `turn.ticket` onion `invoke` handler).
    Run {
        /// Path to the `TurnServiceDaemonConfig` YAML file.
        #[arg(short, long)]
        config: PathBuf,
        /// Optional root directory overriding `node.data_storage` and `node.measure_storage`.
        #[arg(long)]
        storage_path: Option<String>,
        /// Optional storage capacity override applied when `--storage-path` is set.
        #[arg(long)]
        storage_capacity: Option<u32>,
        /// Optional seed file or URL whose peers are appended to `node.bootstrap.peers`.
        #[arg(long)]
        bootstrap_seed: Option<String>,
        /// Optional API authentication token file path override.
        #[arg(long)]
        api_token_path: Option<String>,
        /// Optional allowed CORS origin(s) for the HTTP JSON-RPC endpoints.
        #[arg(long = "api-allowed-origin")]
        api_allowed_origins: Vec<String>,
        /// Allow binding the external HTTP JSON-RPC API on non-loopback addresses.
        #[arg(long)]
        allow_remote_external_api: bool,
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
            load_or_create_api_token(&output, daemon_cfg.node.api_token_path.as_deref())?;
            tracing::info!(
                path = %output.display(),
                "wrote sample rings-turn-service configuration and initialized API token file"
            );
            Ok(())
        }
        Commands::Run {
            config,
            storage_path,
            storage_capacity,
            bootstrap_seed,
            api_token_path,
            api_allowed_origins,
            allow_remote_external_api,
        } => {
            let config_path_str = config.to_string_lossy().to_string();
            let raw = fs::read_to_string(&config)
                .with_context(|| format!("reading config from {}", config.display()))?;
            let mut daemon_cfg: TurnServiceDaemonConfig = serde_saphyr::from_str(&raw)
                .with_context(|| format!("parsing YAML config {}", config.display()))?;
            if let Some(source) = bootstrap_seed {
                let seed = Seed::load(source.as_str())
                    .await
                    .with_context(|| format!("loading bootstrap seed {source}"))?;
                daemon_cfg.node.bootstrap.peers.extend(seed.peers);
            }
            let mut options = NativeNodeRunOptions::new(config_path_str, daemon_cfg.node);
            options.storage_path = storage_path;
            options.storage_capacity = storage_capacity;
            options.api_token_path_override = api_token_path;
            options.api_allowed_origins_override = api_allowed_origins;
            options.allow_remote_external_api_override = allow_remote_external_api;
            run_turn_service_daemon(options, daemon_cfg.turn_service).await
        }
    }
}
