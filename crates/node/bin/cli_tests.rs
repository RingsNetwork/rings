use clap::CommandFactory;
use clap::Parser;
use clap::ValueEnum;

use super::apply_storage_overrides;
use super::config;
use super::daemon::DaemonCommand;
use super::Cli;
use super::Command;

#[test]
fn daemon_command_tree_accepts_the_four_supported_actions() {
    let cases = [
        ["rings", "daemon", "start"].as_slice(),
        ["rings", "daemon", "stop"].as_slice(),
        ["rings", "daemon", "status"].as_slice(),
        ["rings", "daemon", "restart"].as_slice(),
    ];

    for arguments in cases {
        assert!(
            Cli::try_parse_from(arguments).is_ok(),
            "failed to parse {arguments:?}"
        );
    }
}

#[test]
fn daemon_start_accepts_a_config_path() {
    let parsed = Cli::try_parse_from(["rings", "daemon", "start", "-c", "custom.yaml"]);
    let config_path = parsed.ok().and_then(|cli| match cli.command {
        Command::Daemon(DaemonCommand::Start(command)) => Some(command.config_path().to_owned()),
        _ => None,
    });

    assert_eq!(config_path.as_deref(), Some("custom.yaml"));
}

#[test]
fn daemon_start_preserves_the_default_config_path() {
    let parsed = Cli::try_parse_from(["rings", "daemon", "start"]);
    let config_path = parsed.ok().and_then(|cli| match cli.command {
        Command::Daemon(DaemonCommand::Start(command)) => Some(command.config_path().to_owned()),
        _ => None,
    });

    assert_eq!(config_path.as_deref(), Some("~/.rings/config.yaml"));
}

#[test]
fn daemon_command_tree_rejects_unknown_actions() {
    let parsed = Cli::try_parse_from(["rings", "daemon", "install"]);

    assert!(parsed.is_err());
}

#[test]
fn clap_command_tree_is_internally_consistent() {
    Cli::command().debug_assert();
}

#[test]
fn command_order_is_stable() {
    let command = Cli::command();
    let subcommands = command
        .get_subcommands()
        .map(|subcommand| subcommand.get_name())
        .filter(|name| *name != "help")
        .collect::<Vec<_>>();

    assert_eq!(subcommands, [
        "init",
        "new-session",
        "run",
        "pubsub",
        "connect",
        "peer",
        "send",
        "service",
        "inspect",
        "daemon",
    ]);
}

#[test]
fn every_preexisting_command_shape_remains_parseable() {
    let cases: &[&[&str]] = &[
        &["rings", "init"],
        &["rings", "new-session"],
        &["rings", "run"],
        &["rings", "pubsub", "topic"],
        &["rings", "connect", "node", "http://127.0.0.1:50001"],
        &["rings", "connect", "did", "did:ring:peer"],
        &["rings", "connect", "seed", "https://example.com/seed"],
        &["rings", "peer", "list"],
        &["rings", "peer", "disconnect", "did:ring:peer"],
        &["rings", "send", "message", "did:ring:peer", "chat", "hello"],
        &["rings", "service", "register", "web"],
        &["rings", "service", "lookup", "web"],
        &["rings", "inspect"],
    ];

    for &arguments in cases {
        assert!(
            Cli::try_parse_from(arguments).is_ok(),
            "failed to parse {arguments:?}"
        );
    }
}

#[test]
fn global_runtime_and_log_options_remain_compatible() {
    let parsed = Cli::try_parse_from([
        "rings",
        "--log-level",
        "warn",
        "--runtime",
        "current-thread",
        "inspect",
    ]);

    let values = parsed.ok().and_then(|parsed| {
        Some((
            parsed.log_level.to_possible_value()?.get_name().to_owned(),
            parsed.runtime.to_possible_value()?.get_name().to_owned(),
        ))
    });
    assert_eq!(
        values,
        Some(("warn".to_owned(), "current-thread".to_owned()))
    );
}

#[test]
fn storage_path_preserves_each_configured_capacity_without_an_override() {
    let mut config = config::Config::new("session");
    config.data_storage = config::StorageConfig::new("old-data", 5_000);
    config.measure_storage = config::StorageConfig::new("old-measure", 7_000);

    apply_storage_overrides(&mut config, Some("/new-root".to_owned()), None);

    assert_eq!(
        (
            config.data_storage.path.as_str(),
            config.data_storage.capacity,
        ),
        ("/new-root/data", 5_000)
    );
    assert_eq!(
        (
            config.measure_storage.path.as_str(),
            config.measure_storage.capacity,
        ),
        ("/new-root/measure", 7_000)
    );
}

#[test]
fn storage_capacity_overrides_both_configured_stores_without_replacing_paths() {
    let mut config = config::Config::new("session");
    config.data_storage = config::StorageConfig::new("old-data", 5_000);
    config.measure_storage = config::StorageConfig::new("old-measure", 7_000);

    apply_storage_overrides(&mut config, None, Some(9_000));

    assert_eq!(
        (
            config.data_storage.path.as_str(),
            config.data_storage.capacity,
        ),
        ("old-data", 9_000)
    );
    assert_eq!(
        (
            config.measure_storage.path.as_str(),
            config.measure_storage.capacity,
        ),
        ("old-measure", 9_000)
    );
}

#[test]
fn storage_capacity_is_absent_when_the_cli_flag_is_omitted() {
    let command = Cli::command();
    let storage_capacity = command
        .get_subcommands()
        .find(|subcommand| subcommand.get_name() == "run")
        .and_then(|run| {
            run.get_arguments()
                .find(|argument| argument.get_id() == "storage_capacity")
        });

    assert!(storage_capacity.is_some_and(|argument| argument.get_default_values().is_empty()));
}
