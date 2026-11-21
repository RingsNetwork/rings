//! Rings native node command-line entrypoint.

// The `dummy` feature selects rings-core's in-memory test transport, over which a node cannot
// reach any peer; it exists for the node's own test suite only.
#[cfg(all(feature = "dummy", not(test)))]
compile_error!("the `dummy` test transport cannot run a node; build `rings` without it");

use anyhow::Context;
use clap::Parser;
use cli::payload_arg_or_stdin;
use cli::Cli;
use cli::ClientArgs;
use cli::Command;
use cli::ConnectCommand;
use cli::PeerCommand;
use cli::RunCommand;
use cli::SendCommand;
use cli::ServiceCommand;
use futures::pin_mut;
use futures::StreamExt;
use rings_node::logging::init_logging;
use rings_node::native::api_auth::load_api_token_file;
use rings_node::native::api_auth::load_or_create_api_token;
use rings_node::native::config;
use rings_node::native::daemon::run_foreground_node;
use rings_node::native::daemon::NativeNodeRunOptions;
use rings_node::onion::OnionExitTarget;
use rings_node::seed::Seed;
use rings_node::util::loader::ResourceLoader;
use tokio::io;
use tokio::io::AsyncBufReadExt;

/// The command line: the top-level parser, its subcommands and their arguments.
mod cli;

/// Run the node in the foreground until a signal or a service stops it.
async fn foreground_run(mut args: RunCommand) -> anyhow::Result<()> {
    let config_path = args.config_args.config.clone();
    let mut c = config::Config::read_fs(&config_path)?;
    apply_run_overrides(&mut c, &mut args, &config_path).await?;
    let mut options = NativeNodeRunOptions::new(config_path, c);
    options.storage_path = args.storage_path;
    options.storage_capacity = args.storage_capacity;
    options.reassembly_limits = args.reassembly_profile.limits();
    options.api_token_path_override = args.api_token_path;
    options.api_allowed_origins_override = args.api_allowed_origins;
    options.allow_remote_external_api_override = args.allow_remote_external_api;
    run_foreground_node(options, ()).await
}

/// Apply the run's command-line overrides to the configuration read from `config_path`.
async fn apply_run_overrides(
    c: &mut config::Config,
    args: &mut RunCommand,
    config_path: &str,
) -> anyhow::Result<()> {
    if let Some(ice_servers) = args.ice_servers.take() {
        c.ice_servers = ice_servers;
    }
    if let Some(external_ip) = args.external_ip.take() {
        c.external_ip = Some(external_ip);
    }
    if args.udp_port_min.is_some() {
        c.udp_port_min = args.udp_port_min;
    }
    if args.udp_port_max.is_some() {
        c.udp_port_max = args.udp_port_max;
    }
    if let Some(stabilize_interval) = args.stabilize_interval.take() {
        c.stabilize_interval = stabilize_interval;
    }
    if let Some(source) = args.bootstrap_seed.take() {
        let seed = Seed::load(source.as_str())
            .await
            .with_context(|| format!("loading bootstrap seed {source}"))?;
        c.bootstrap.peers.extend(seed.peers);
    }
    if let Some(proxy_url) = args.remote_rpc_proxy_url.take() {
        c.remote_rpc_proxy_url = Some(proxy_url);
    }
    if let Some(external_api_addr) = args.external_api_addr.take() {
        c.external_api_addr = external_api_addr;
    }
    if let Some(internal_api_port) = args.internal_api_port.take() {
        c.internal_api_port = internal_api_port;
    }
    if args.advertise_onion_relay {
        c.advertise_onion_relay = true;
    }
    if args.advertise_onion_exit {
        c.advertise_onion_exit = true;
    }
    if !args.onion_exit_service.is_empty() {
        c.onion_exit_services = std::mem::take(&mut args.onion_exit_service);
    }
    if !args.onion_exit_allow_target.is_empty() {
        c.onion_exit_policy.allowed_targets =
            parse_onion_exit_targets(std::mem::take(&mut args.onion_exit_allow_target))?;
    }
    if !args.onion_exit_deny_target.is_empty() {
        c.onion_exit_policy.denied_targets =
            parse_onion_exit_targets(std::mem::take(&mut args.onion_exit_deny_target))?;
    }
    if let Some(max_sessions) = args.onion_exit_max_sessions.take() {
        c.onion_exit_policy.max_sessions = max_sessions;
    }
    if let Some(max_bytes_per_minute) = args.onion_exit_max_bytes_per_minute.take() {
        c.onion_exit_policy.max_bytes_per_minute = max_bytes_per_minute;
    }
    if let Some(interval_secs) = args.onion_exit_heartbeat_interval_secs.take() {
        c.onion_exit_heartbeat_interval_secs = interval_secs;
    }
    if let Some(ttl_secs) = args.onion_exit_ttl_secs.take() {
        c.onion_exit_ttl_secs = ttl_secs;
    }
    if let Some(addr) = args.onion_http_proxy_addr.take() {
        c.onion_http_proxy_addr = Some(addr);
    }
    if let Some(timeout_secs) = args.onion_http_proxy_header_timeout_secs.take() {
        c.onion_http_proxy_header_timeout_secs = timeout_secs;
    }
    if let Some(max_connections) = args.onion_http_proxy_max_connections.take() {
        c.onion_http_proxy_max_connections = max_connections;
    }
    if args.gateway {
        let Some(gateway) = c.gateway.as_mut() else {
            anyhow::bail!(
                "--gateway requires a gateway section in {config_path}; `rings init` writes one \
                 into a new config file, or append this to the existing file:\n{}",
                config::NativeGatewayConfig::disabled_default().to_yaml_section()?
            );
        };
        gateway.enabled = true;
    }
    Ok(())
}

fn parse_onion_exit_targets(targets: Vec<String>) -> anyhow::Result<Vec<OnionExitTarget>> {
    let mut parsed = Vec::with_capacity(targets.len());
    for target in targets {
        parsed.push(OnionExitTarget::parse(target)?);
    }
    Ok(parsed)
}

async fn pubsub_run(client_args: ClientArgs, topic: String) -> anyhow::Result<()> {
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    let client = client_args.new_client().await?;
    let stream = client.subscribe_topic(topic.clone()).await;
    pin_mut!(stream);

    loop {
        tokio::select! {
            line = stdin.next_line() => {
                match line? {
                    Some(line) => {
                        client.publish_message_to_topic(&topic, &line).await?;
                    }
                    None => return Ok(()),
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(msg) => println!("{msg}"),
                    None => return Ok(()),
                }
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    init_logging(cli.log_level);
    let runtime = cli.runtime.build()?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Run(args) => foreground_run(*args).await,
        Command::Pubsub(args) => pubsub_run(args.client_args, args.topic).await,
        Command::Connect(ConnectCommand::Node(args)) => {
            let remote_api_token = args
                .remote_api_token_file
                .as_deref()
                .map(load_api_token_file)
                .transpose()?
                .map(|token| token.into_secret());
            args.client_args
                .new_client()
                .await?
                .connect_peer_via_http_with_token(args.node_url.as_str(), remote_api_token)
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Did(args)) => {
            args.client_args
                .new_client()
                .await?
                .connect_with_did(args.did.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Seed(args)) => {
            args.client_args
                .new_client()
                .await?
                .connect_with_seed(args.source.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Offer(args)) => {
            args.client_args
                .new_client()
                .await?
                .create_offer(args.did.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Answer(args)) => {
            let offer = payload_arg_or_stdin(args.offer.as_str())?;
            args.client_args
                .new_client()
                .await?
                .answer_offer(offer.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Connect(ConnectCommand::Accept(args)) => {
            let answer = payload_arg_or_stdin(args.answer.as_str())?;
            args.client_args
                .new_client()
                .await?
                .accept_answer(answer.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Peer(PeerCommand::List(args)) => {
            args.client_args
                .new_client()
                .await?
                .list_peers()
                .await?
                .display();
            Ok(())
        }
        Command::Peer(PeerCommand::Disconnect(args)) => {
            args.client_args
                .new_client()
                .await?
                .disconnect(args.address.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Send(SendCommand::Message(args)) => {
            args.client_args
                .new_client()
                .await?
                .send_message(
                    args.to_did.as_str(),
                    args.namespace.as_str(),
                    args.data.as_str(),
                )
                .await?
                .display();
            Ok(())
        }
        Command::Service(ServiceCommand::Register(args)) => {
            args.client_args
                .new_client()
                .await?
                .register_service(args.name.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Service(ServiceCommand::Lookup(args)) => {
            args.client_args
                .new_client()
                .await?
                .lookup_service(args.name.as_str())
                .await?
                .display();
            Ok(())
        }
        Command::Init(args) => {
            let delegatee_key_path = args.delegation_args.new_delegation_then_write_to_fs()?;
            let config = config::Config::new(delegatee_key_path);
            let p = config.write_fs(&args.location)?;
            load_or_create_api_token(&p, config.api_token_path.as_deref())?;
            println!("Your config file has saved to: {p}");
            println!("API authentication token file initialized.");
            Ok(())
        }
        Command::NewDelegation(args) => {
            args.delegation_args.new_delegation_then_write_to_fs()?;
            Ok(())
        }
        Command::Inspect(args) => {
            args.client_args
                .new_client()
                .await?
                .inspect()
                .await?
                .display();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use clap::CommandFactory;
    use clap::FromArgMatches;
    use rings_node::logging::LogLevel;
    use rings_node::native::daemon::await_foreground_exit_with_signal;
    use rings_node::native::daemon::await_gateway_startup;
    use rings_node::native::daemon::await_task_cleanup;
    use rings_node::native::daemon::spawn_processor;
    use rings_node::native::daemon::start_gateway;
    use rings_node::native::daemon::OwnedTask;
    use rings_node::processor::ProcessorBuilder;
    use rings_node::processor::ProcessorConfig;
    use tokio::task::JoinSet;

    use super::Cli;

    fn parse_without_log_level_env<const N: usize>(args: [&str; N]) -> Result<Cli, clap::Error> {
        let matches = Cli::command()
            .mut_arg("log_level", |arg| arg.env(None::<&'static str>))
            .try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    #[test]
    fn test_cli_default_log_level_is_error() {
        let parsed =
            parse_without_log_level_env(["rings", "--runtime", "current-thread", "new-delegation"]);

        assert!(matches!(
            parsed,
            Ok(Cli {
                log_level: LogLevel::Error,
                ..
            })
        ));
    }

    #[test]
    fn test_cli_explicit_log_level_overrides_default() {
        let parsed = parse_without_log_level_env([
            "rings",
            "--log-level",
            "debug",
            "--runtime",
            "current-thread",
            "new-delegation",
        ]);

        assert!(matches!(
            parsed,
            Ok(Cli {
                log_level: LogLevel::Debug,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn processor_cleanup_gets_a_cooperative_stop_window() {
        let stop = rings_node::prelude::StopSource::new();
        let token = stop.token();
        let flushed = Arc::new(AtomicBool::new(false));
        let task_flushed = Arc::clone(&flushed);
        let mut task = OwnedTask::spawn(async move {
            token.stopped().await;
            task_flushed.store(true, Ordering::Release);
            Ok(())
        });
        stop.request_stop();

        let cleanup = await_task_cleanup(&mut task, "test processor cleanup").await;

        assert!(cleanup.is_ok());
        assert!(flushed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn foreground_shutdown_closes_processor_network() -> anyhow::Result<()> {
        let key = rings_node::prelude::rings_core::ecc::SecretKey::random();
        let delegatee_key =
            rings_node::prelude::rings_core::delegation::DelegateeKey::new_with_seckey(&key)?;
        let processor_config =
            ProcessorConfig::new(0, "stun://127.0.0.1:3478".to_string(), delegatee_key, 200)
                .dht_virtual_nodes(0);
        let replay = Box::new(rings_node::prelude::rings_core::storage::MemStorage::new());
        let processor = Arc::new(
            ProcessorBuilder::from_config(&processor_config)?
                .replay_storage(replay)
                .build()?,
        );
        let stop = rings_node::prelude::StopSource::new();
        let mut tasks = JoinSet::new();
        let task_stop = stop.token();
        tasks.spawn(async move {
            task_stop.stopped().await;
            Ok(())
        });
        let gateway_task = start_gateway(None, stop.token()).await?;
        let processor_task = spawn_processor(Arc::clone(&processor), stop.token());

        await_foreground_exit_with_signal(
            async { Ok(()) },
            stop,
            tasks,
            gateway_task,
            processor_task,
            Arc::clone(&processor),
            false,
            &mut (),
        )
        .await?;

        let peer = rings_node::prelude::rings_core::ecc::SecretKey::random()
            .address()
            .into();
        assert!(matches!(
            processor.swarm.create_offer(peer).await,
            Err(rings_node::prelude::rings_core::error::Error::SwarmClosed)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn gateway_startup_barrier_reports_early_failure() {
        let (started, startup) = tokio::sync::oneshot::channel();
        drop(started);
        let mut task = OwnedTask::spawn(async {
            Err(anyhow::anyhow!("gateway activation failed before startup"))
        });

        let result = await_gateway_startup(true, startup, &mut task).await;

        assert!(matches!(
            result,
            Err(error) if error.to_string().contains("activation failed before startup")
        ));
    }

    #[tokio::test]
    async fn test_run_remote_rpc_proxy_url_override_applies_to_config() {
        let parsed = parse_without_log_level_env([
            "rings",
            "run",
            "--remote-rpc-proxy-url",
            "http://127.0.0.1:18080",
        ]);
        let mut cfg = super::config::Config::new("delegatee_key");
        assert_eq!(cfg.remote_rpc_proxy_url, None);

        if let Ok(Cli {
            command: super::Command::Run(mut run_args),
            ..
        }) = parsed
        {
            assert!(
                super::apply_run_overrides(&mut cfg, &mut run_args, "config.yaml")
                    .await
                    .is_ok()
            );
        }
        assert_eq!(
            cfg.remote_rpc_proxy_url.as_deref(),
            Some("http://127.0.0.1:18080")
        );
    }

    #[test]
    fn test_connect_answer_and_accept_help_describes_counter_offer_flow() {
        let mut cmd = Cli::command();
        let connect = cmd.find_subcommand_mut("connect");
        assert!(connect.is_some());
        if let Some(connect) = connect {
            let answer_help = connect
                .find_subcommand_mut("answer")
                .map(|sub| sub.render_long_help().to_string())
                .unwrap_or_default();
            assert!(
                answer_help.contains("counter-offer"),
                "connect answer help should explain counter-offer: {answer_help}"
            );
            assert!(
                answer_help.contains("connect accept"),
                "connect answer help should mention connect accept: {answer_help}"
            );

            let accept_help = connect
                .find_subcommand_mut("accept")
                .map(|sub| sub.render_long_help().to_string())
                .unwrap_or_default();
            assert!(
                accept_help.contains("counter-offer"),
                "connect accept help should explain counter-offer: {accept_help}"
            );
            assert!(
                accept_help.contains("connect answer"),
                "connect accept help should point to connect answer: {accept_help}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_install_script_and_contrib_service_definitions() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir.parent().and_then(std::path::Path::parent);
        assert!(repo_root.is_some());
        if let Some(repo_root) = repo_root {
            let script = repo_root.join("scripts/test-install.sh");
            let output = std::process::Command::new("sh")
                .arg(script.as_os_str())
                .output();
            assert!(output.is_ok());
            if let Ok(output) = output {
                assert!(
                    output.status.success(),
                    "scripts/test-install.sh failed:\nstdout: {}\nstderr: {}",
                    String::from_utf8_lossy(output.stdout.as_slice()),
                    String::from_utf8_lossy(output.stderr.as_slice())
                );
            }
        }
    }
}
