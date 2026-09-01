//! Foreground Unix privilege boundary for Rings gateway interface and route configuration.

#![deny(missing_docs)]
#![cfg_attr(test, allow(clippy::expect_used))]

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::num::ParseIntError;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::PathBuf;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use rings_gateway::bindings::unix::helper::serve;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rings_gateway::bindings::unix::helper::UnixHelperOptions;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rings_gateway::bindings::NativeTunnelOptions;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rings_gateway::GatewayError;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use thiserror::Error;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() {
    match parse_args(std::env::args().skip(1)) {
        Ok(ParseResult::Help) => print_help(),
        Ok(ParseResult::Run(arguments)) => {
            if let Err(error) = run(arguments) {
                eprintln!("gateway-config-unix: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("gateway-config-unix: {error}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {
    eprintln!("gateway-config-unix is available only on Linux and macOS");
    std::process::exit(1);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run(arguments: Arguments) -> Result<(), GatewayError> {
    let mut tunnel = NativeTunnelOptions::new(arguments.ledger);
    if let Some(interface) = arguments.interface {
        tunnel = tunnel.with_interface_name(interface);
    }
    let mut helper = UnixHelperOptions::new(arguments.socket, tunnel);
    if let Some(uid) = arguments.socket_owner {
        helper = helper.with_socket_owner(uid);
    }
    serve(helper)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Eq, PartialEq)]
struct Arguments {
    socket: PathBuf,
    ledger: PathBuf,
    interface: Option<String>,
    socket_owner: Option<u32>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Eq, PartialEq)]
enum ParseResult {
    Help,
    Run(Arguments),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Error)]
enum ArgumentError {
    #[error("unknown argument {argument:?}")]
    UnknownArgument { argument: String },
    #[error("{flag} requires a value")]
    MissingValue { flag: &'static str },
    #[error("{flag} may be provided only once")]
    DuplicateArgument { flag: &'static str },
    #[error("invalid --socket-owner {value:?}: {source}")]
    InvalidSocketOwner {
        value: String,
        #[source]
        source: ParseIntError,
    },
    #[error("missing required {flag} {value_name}")]
    MissingRequired {
        flag: &'static str,
        value_name: &'static str,
    },
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn parse_args(arguments: impl Iterator<Item = String>) -> Result<ParseResult, ArgumentError> {
    let mut arguments = arguments;
    let mut socket = None;
    let mut ledger = None;
    let mut interface = None;
    let mut socket_owner = None;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(ParseResult::Help),
            "--socket" => set_once(
                &mut socket,
                PathBuf::from(next_value(&mut arguments, "--socket")?),
                "--socket",
            )?,
            "--ledger" => set_once(
                &mut ledger,
                PathBuf::from(next_value(&mut arguments, "--ledger")?),
                "--ledger",
            )?,
            "--interface" => set_once(
                &mut interface,
                next_value(&mut arguments, "--interface")?,
                "--interface",
            )?,
            "--socket-owner" => {
                let value = next_value(&mut arguments, "--socket-owner")?;
                let uid = value
                    .parse::<u32>()
                    .map_err(|source| ArgumentError::InvalidSocketOwner { value, source })?;
                set_once(&mut socket_owner, uid, "--socket-owner")?;
            }
            _ => return Err(ArgumentError::UnknownArgument { argument }),
        }
    }

    let socket = socket.ok_or(ArgumentError::MissingRequired {
        flag: "--socket",
        value_name: "PATH",
    })?;
    let ledger = ledger.ok_or(ArgumentError::MissingRequired {
        flag: "--ledger",
        value_name: "PATH",
    })?;
    Ok(ParseResult::Run(Arguments {
        socket,
        ledger,
        interface,
        socket_owner,
    }))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn next_value(
    arguments: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<String, ArgumentError> {
    arguments.next().ok_or(ArgumentError::MissingValue { flag })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_once<T>(slot: &mut Option<T>, value: T, flag: &'static str) -> Result<(), ArgumentError> {
    if slot.is_some() {
        return Err(ArgumentError::DuplicateArgument { flag });
    }
    *slot = Some(value);
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn print_help() {
    println!(
        "Usage: gateway-config-unix --socket PATH --ledger PATH [OPTIONS]\n\
         \n\
         Foreground privilege boundary for one Rings gateway lifecycle.\n\
         It does not install, invoke, or wrap systemd, launchd, or another service manager.\n\
         Socket and ledger parents must belong to this process. Their canonical ancestor chains\n\
         may be owned only by this process or root and must not be group/other writable.\n\
         \n\
         Options:\n\
           --socket PATH       Unix control socket created with mode 0600\n\
           --ledger PATH       Durable route cleanup journal\n\
           --interface NAME    Requested TUN name (Linux) or utun name (macOS)\n\
           --socket-owner UID  Chown the socket for an unprivileged node process\n\
           -h, --help          Print this help"
    );
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::path::PathBuf;

    use super::parse_args;
    use super::Arguments;
    use super::ParseResult;

    #[test]
    fn parses_required_and_privilege_boundary_options() {
        let parsed = parse_args(
            [
                "--socket",
                "/tmp/rings.sock",
                "--ledger",
                "/tmp/rings-routes.json",
                "--interface",
                "rings0",
                "--socket-owner",
                "501",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("parse arguments");
        assert_eq!(
            parsed,
            ParseResult::Run(Arguments {
                socket: PathBuf::from("/tmp/rings.sock"),
                ledger: PathBuf::from("/tmp/rings-routes.json"),
                interface: Some("rings0".to_string()),
                socket_owner: Some(501),
            })
        );
    }
}
