use clap::CommandFactory;
use clap::Parser;

use super::Cli;

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

    assert!(parsed.is_ok());
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

    assert!(parsed.is_ok());
}
