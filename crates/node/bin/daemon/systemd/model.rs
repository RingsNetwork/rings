//! Pure systemd unit rendering and status reduction with no process or filesystem effects.

use thiserror::Error;

use super::super::command_output_value;
use super::super::AutostartState;
use super::super::DaemonFailure;
use super::super::DaemonManagerFailure;
use super::super::DaemonState;
use super::super::DaemonStatus;
use super::super::ServiceSpec;
use super::SYSTEMD_MANAGER;

// Verified with systemd 257.13: ExecMainCode publishes Linux `siginfo_t::si_code`.
const SYSTEMD_EXEC_CODE_EXITED: i32 = 1;
const SYSTEMD_EXEC_CODE_KILLED: i32 = 2;
const SYSTEMD_EXEC_CODE_DUMPED: i32 = 3;

#[derive(Debug, Error)]
pub(crate) enum SystemdError {
    #[error(transparent)]
    Definition(#[from] SystemdDefinitionError),
    #[error("{SYSTEMD_MANAGER} status is missing required property {property}")]
    MissingProperty { property: &'static str },
}

#[derive(Debug, Error)]
pub(crate) enum SystemdDefinitionError {
    #[error("working directory contains a line break and cannot be written safely to a systemd unit: {value:?}")]
    ContainsLineBreak { value: String },
    #[error("working directory has leading or trailing ASCII whitespace that systemd would discard: {value:?}")]
    HasBoundaryWhitespace { value: String },
}

pub(super) fn render_systemd_unit(spec: &ServiceSpec) -> Result<String, SystemdError> {
    let command = spec
        .arguments()
        .into_iter()
        .map(systemd_exec_quote)
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "[Unit]\n\
Description=Rings Network node\n\
Wants=network-online.target\n\
After=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
WorkingDirectory={}\n\
ExecStart={command}\n\
Restart=on-failure\n\
RestartSec=5\n\
TimeoutStopSec=30\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        systemd_working_directory(&spec.working_directory)?
    ))
}

fn systemd_exec_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("$$"),
            '%' => quoted.push_str("%%"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn systemd_working_directory(value: &str) -> Result<String, SystemdDefinitionError> {
    if value
        .chars()
        .any(|character| matches!(character, '\n' | '\r'))
    {
        return Err(SystemdDefinitionError::ContainsLineBreak {
            value: value.to_owned(),
        });
    }
    if has_boundary_ascii_whitespace(value) {
        return Err(SystemdDefinitionError::HasBoundaryWhitespace {
            value: value.to_owned(),
        });
    }
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\'' => escaped.push_str("\\'"),
            '\u{7}' => escaped.push_str("\\a"),
            '\u{8}' => escaped.push_str("\\b"),
            '\t' => escaped.push_str("\\t"),
            '\u{b}' => escaped.push_str("\\v"),
            '\u{c}' => escaped.push_str("\\f"),
            '%' => escaped.push_str("%%"),
            other if other.is_ascii_control() => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let byte = other as u8;
                let high = HEX.get(usize::from(byte >> 4)).copied().unwrap_or(b'0');
                let low = HEX.get(usize::from(byte & 0x0f)).copied().unwrap_or(b'0');
                escaped.push_str("\\x");
                escaped.push(char::from(high));
                escaped.push(char::from(low));
            }
            other => escaped.push(other),
        }
    }
    Ok(escaped)
}

fn has_boundary_ascii_whitespace(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_whitespace())
        || value
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_whitespace())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SystemdLifecycle<'a> {
    NotInstalled,
    Running,
    Stopped,
    Failed,
    Starting,
    AutoRestarting,
    Stopping,
    Other {
        load: &'a str,
        active: &'a str,
        sub: &'a str,
    },
}

impl SystemdLifecycle<'_> {
    fn into_installed_state(self, output: &str) -> Option<DaemonState> {
        Some(match self {
            Self::NotInstalled => return None,
            Self::Running => DaemonState::Running,
            Self::Stopped => DaemonState::Stopped,
            Self::Failed => DaemonState::Failed(parse_systemd_failure(output)),
            Self::Starting => DaemonState::Starting,
            Self::AutoRestarting => DaemonState::Restarting(parse_systemd_failure(output)),
            Self::Stopping => DaemonState::Stopping,
            Self::Other { load, active, sub } => DaemonState::Unknown(format!(
                "load state: {load}, active state: {active}, substate: {sub}"
            )),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SystemdResult<'a> {
    Success,
    ExitCode,
    Signal,
    CoreDump,
    Timeout,
    Watchdog,
    OutOfMemory,
    StartLimit,
    Protocol,
    Resources,
    Other(&'a str),
    Missing,
}

impl SystemdResult<'_> {
    fn failure(self, output: &str) -> Option<DaemonFailure> {
        match self {
            Self::ExitCode => SystemdExecOutcome::Exited.failure_from(output),
            Self::Signal => SystemdExecOutcome::Killed.failure_from(output),
            Self::CoreDump => SystemdExecOutcome::Dumped.failure_from(output),
            Self::Timeout => Some(DaemonFailure::Manager(DaemonManagerFailure::Timeout)),
            Self::Watchdog => Some(DaemonFailure::Manager(DaemonManagerFailure::Watchdog)),
            Self::OutOfMemory => Some(DaemonFailure::Manager(DaemonManagerFailure::OutOfMemory)),
            Self::StartLimit => Some(DaemonFailure::Manager(DaemonManagerFailure::StartLimit)),
            Self::Protocol => Some(DaemonFailure::Manager(DaemonManagerFailure::Protocol)),
            Self::Resources => Some(DaemonFailure::Manager(DaemonManagerFailure::Resources)),
            Self::Other(result) => Some(DaemonFailure::Manager(DaemonManagerFailure::Other(
                result.to_owned(),
            ))),
            Self::Success | Self::Missing => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SystemdExecOutcome {
    Exited,
    Killed,
    Dumped,
}

impl SystemdExecOutcome {
    fn si_code(self) -> i32 {
        match self {
            Self::Exited => SYSTEMD_EXEC_CODE_EXITED,
            Self::Killed => SYSTEMD_EXEC_CODE_KILLED,
            Self::Dumped => SYSTEMD_EXEC_CODE_DUMPED,
        }
    }

    fn failure_from(self, output: &str) -> Option<DaemonFailure> {
        let code = property(output, "ExecMainCode")?.parse::<i32>().ok()?;
        let status = property(output, "ExecMainStatus")?.parse::<i32>().ok()?;
        if code != self.si_code() || status <= 0 {
            return None;
        }
        match self {
            Self::Exited => Some(DaemonFailure::ExitCode(status)),
            Self::Killed => Some(DaemonFailure::Signal {
                name: None,
                number: status,
                core_dumped: false,
            }),
            Self::Dumped => Some(DaemonFailure::Signal {
                name: None,
                number: status,
                core_dumped: true,
            }),
        }
    }
}

pub(super) fn parse_systemd_status(output: &str) -> Result<DaemonStatus, SystemdError> {
    let load = required_property(output, "LoadState")?;
    let active = required_property(output, "ActiveState")?;
    let sub = required_property(output, "SubState")?;
    let lifecycle = parse_systemd_lifecycle(load, active, sub);
    let Some(state) = lifecycle.into_installed_state(output) else {
        return Ok(DaemonStatus::NotInstalled);
    };
    let unit_file_state = required_property(output, "UnitFileState")?;
    Ok(DaemonStatus::installed(
        state,
        parse_systemd_autostart(unit_file_state),
    ))
}

fn required_property<'a>(
    output: &'a str,
    property_name: &'static str,
) -> Result<&'a str, SystemdError> {
    property(output, property_name).ok_or(SystemdError::MissingProperty {
        property: property_name,
    })
}

fn property<'a>(output: &'a str, property_name: &str) -> Option<&'a str> {
    command_output_value(output, property_name)
}

fn parse_systemd_lifecycle<'a>(
    load: &'a str,
    active: &'a str,
    sub: &'a str,
) -> SystemdLifecycle<'a> {
    // Verified with systemd 257.13, including auto-restart-queued and an active unlinked unit whose
    // LoadState is not-found. Unknown tuples remain visible instead of being guessed.
    match (load, active, sub) {
        ("not-found", "inactive", _) => SystemdLifecycle::NotInstalled,
        ("loaded" | "not-found", "active" | "reloading", _) => SystemdLifecycle::Running,
        ("loaded", "inactive", _) => SystemdLifecycle::Stopped,
        ("loaded" | "not-found", "failed", _) => SystemdLifecycle::Failed,
        ("loaded" | "not-found", "activating", "auto-restart" | "auto-restart-queued") => {
            SystemdLifecycle::AutoRestarting
        }
        ("loaded" | "not-found", "activating", _) => SystemdLifecycle::Starting,
        ("loaded" | "not-found", "deactivating", _) => SystemdLifecycle::Stopping,
        _ => SystemdLifecycle::Other { load, active, sub },
    }
}

fn parse_systemd_failure(output: &str) -> Option<DaemonFailure> {
    parse_systemd_result(property(output, "Result")).failure(output)
}

fn parse_systemd_result(result: Option<&str>) -> SystemdResult<'_> {
    match result {
        Some("success") => SystemdResult::Success,
        Some("exit-code") => SystemdResult::ExitCode,
        Some("signal") => SystemdResult::Signal,
        Some("core-dump") => SystemdResult::CoreDump,
        Some("timeout") => SystemdResult::Timeout,
        Some("watchdog") => SystemdResult::Watchdog,
        Some("oom-kill") => SystemdResult::OutOfMemory,
        Some("start-limit-hit") => SystemdResult::StartLimit,
        Some("protocol") => SystemdResult::Protocol,
        Some("resources") => SystemdResult::Resources,
        Some("") | None => SystemdResult::Missing,
        Some(other) => SystemdResult::Other(other),
    }
}

fn parse_systemd_autostart(output: &str) -> AutostartState {
    match output.trim() {
        "enabled" | "enabled-runtime" => AutostartState::Enabled,
        "disabled" => AutostartState::Disabled,
        "masked" | "masked-runtime" => AutostartState::Unavailable,
        // Linked and alias describe unit-file discovery, not default-target installation.
        "linked" | "linked-runtime" | "alias" => AutostartState::Unknown,
        _ => AutostartState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use rings_node::logging::LogLevel;

    use super::super::super::super::RuntimeFlavor;
    use super::super::super::tests::service_spec;
    use super::super::super::DaemonError;
    use super::*;

    #[test]
    fn definition_quotes_arguments_and_sets_working_directory() -> Result<(), DaemonError> {
        let spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
        let unit = render_systemd_unit(&spec)?;

        assert!(unit.contains("ExecStart=\"/Users/test user/bin/rings\""));
        assert!(unit.contains("\"/Users/test user/.rings/config&prod.yaml\""));
        assert!(unit.contains("WorkingDirectory=/Users/test user/work"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        Ok(())
    }

    #[test]
    fn lifecycle_reduction_preserves_manager_states() {
        assert_eq!(
            parse_systemd_lifecycle("loaded", "active", "running"),
            SystemdLifecycle::Running
        );
        assert_eq!(
            parse_systemd_lifecycle("loaded", "reloading", "reload"),
            SystemdLifecycle::Running
        );
        assert_eq!(
            parse_systemd_lifecycle("loaded", "activating", "auto-restart-queued"),
            SystemdLifecycle::AutoRestarting
        );
        assert!(matches!(
            parse_systemd_lifecycle("error", "inactive", "dead"),
            SystemdLifecycle::Other { .. }
        ));
    }

    #[test]
    fn status_requires_every_structural_property() {
        for property in ["LoadState", "ActiveState", "SubState", "UnitFileState"] {
            let output =
                "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\n"
                    .lines()
                    .filter(|line| !line.starts_with(property))
                    .collect::<Vec<_>>()
                    .join("\n");
            assert!(matches!(
                parse_systemd_status(&output),
                Err(SystemdError::MissingProperty { property: missing }) if missing == property
            ));
        }
    }

    #[test]
    fn not_installed_has_no_synthetic_autostart_state() -> Result<(), SystemdError> {
        let status =
            parse_systemd_status("LoadState=not-found\nActiveState=inactive\nSubState=dead\n")?;

        assert_eq!(status, DaemonStatus::NotInstalled);
        Ok(())
    }

    #[test]
    fn detached_and_reloading_units_remain_running() -> Result<(), SystemdError> {
        for (load, active, sub) in [
            ("not-found", "active", "running"),
            ("loaded", "reloading", "reload"),
        ] {
            let status = parse_systemd_status(&format!(
                "LoadState={load}\nActiveState={active}\nSubState={sub}\nUnitFileState=linked\n"
            ))?;
            assert_eq!(
                status,
                DaemonStatus::installed(DaemonState::Running, AutostartState::Unknown)
            );
        }
        Ok(())
    }

    #[test]
    fn unknown_manager_vocabulary_remains_user_visible() -> Result<(), SystemdError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=future-active\nSubState=future-sub\nUnitFileState=enabled\n",
        )?;

        assert_eq!(
            status.to_string(),
            "unknown (load state: loaded, active state: future-active, substate: future-sub)"
        );
        Ok(())
    }

    #[test]
    fn auto_restart_preserves_failure_without_becoming_terminal() -> Result<(), SystemdError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n",
        )?;

        assert_eq!(
            status,
            DaemonStatus::installed(
                DaemonState::Restarting(Some(DaemonFailure::ExitCode(78))),
                AutostartState::Enabled,
            )
        );
        Ok(())
    }

    #[test]
    fn post_stop_success_does_not_attribute_systemd_sigterm() -> Result<(), SystemdError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=15\nResult=success\n",
        )?;

        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Stopped, AutostartState::Enabled)
        );
        Ok(())
    }

    #[test]
    fn timeout_suppresses_the_physical_sigkill_record() -> Result<(), SystemdError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=9\nResult=timeout\n",
        )?;

        assert_eq!(
            status,
            DaemonStatus::installed(
                DaemonState::Failed(Some(DaemonFailure::Manager(DaemonManagerFailure::Timeout,))),
                AutostartState::Enabled,
            )
        );
        Ok(())
    }

    #[test]
    fn exit_signal_core_and_unknown_results_remain_distinct() -> Result<(), SystemdError> {
        let cases = [
            ("exit-code", "1", "78", DaemonFailure::ExitCode(78)),
            ("signal", "2", "15", DaemonFailure::Signal {
                name: None,
                number: 15,
                core_dumped: false,
            }),
            ("core-dump", "3", "6", DaemonFailure::Signal {
                name: None,
                number: 6,
                core_dumped: true,
            }),
            (
                "future-result",
                "2",
                "9",
                DaemonFailure::Manager(DaemonManagerFailure::Other("future-result".to_owned())),
            ),
        ];
        for (result, code, process_status, failure) in cases {
            let status = parse_systemd_status(&format!(
                "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode={code}\nExecMainStatus={process_status}\nResult={result}\n"
            ))?;
            assert_eq!(
                status,
                DaemonStatus::installed(
                    DaemonState::Failed(Some(failure)),
                    AutostartState::Enabled,
                )
            );
        }
        Ok(())
    }

    #[test]
    fn process_failure_requires_a_matching_positive_siginfo_record() -> Result<(), SystemdError> {
        for (result, code, process_status) in [
            ("exit-code", "2", "78"),
            ("exit-code", "1", "0"),
            ("signal", "3", "9"),
            ("core-dump", "2", "6"),
            ("signal", "invalid", "9"),
            ("signal", "2", "invalid"),
        ] {
            let status = parse_systemd_status(&format!(
                "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode={code}\nExecMainStatus={process_status}\nResult={result}\n"
            ))?;
            assert_eq!(
                status,
                DaemonStatus::installed(DaemonState::Failed(None), AutostartState::Enabled)
            );
        }
        Ok(())
    }

    #[test]
    fn autostart_does_not_conflate_discovery_or_masking_with_enabled() {
        assert_eq!(parse_systemd_autostart("enabled"), AutostartState::Enabled);
        assert_eq!(
            parse_systemd_autostart("disabled"),
            AutostartState::Disabled
        );
        assert_eq!(parse_systemd_autostart("linked"), AutostartState::Unknown);
        assert_eq!(parse_systemd_autostart("alias"), AutostartState::Unknown);
        assert_eq!(
            parse_systemd_autostart("masked"),
            AutostartState::Unavailable
        );
    }

    #[test]
    fn working_directory_escapes_unit_syntax_and_accepts_non_ascii_boundary_space() {
        assert!(matches!(
            systemd_working_directory("/tmp/a\t$HOME/%n/\\rings/\"node\"/'worker'/\u{7}"),
            Ok(path) if path == "/tmp/a\\t$HOME/%%n/\\\\rings/\\\"node\\\"/\\'worker\\'/\\a"
        ));
        assert!(matches!(
            systemd_working_directory("\u{a0}/tmp/rings\u{a0}"),
            Ok(path) if path == "\u{a0}/tmp/rings\u{a0}"
        ));
        assert!(matches!(
            systemd_working_directory("/tmp/rings\\"),
            Ok(path) if path == "/tmp/rings\\\\"
        ));
    }

    #[test]
    fn working_directory_rejects_line_reader_mutations() {
        for path in [
            "/tmp/rings\nnode",
            "/tmp/rings\rnode",
            " /tmp/rings",
            "/tmp/rings ",
            "\t/tmp/rings",
            "/tmp/rings\t",
        ] {
            assert!(systemd_working_directory(path).is_err());
        }
    }

    #[test]
    fn exec_quote_escapes_service_manager_expansion_characters() {
        assert_eq!(
            systemd_exec_quote("/tmp/a $HOME/%n/\"rings\""),
            "\"/tmp/a $$HOME/%%n/\\\"rings\\\"\""
        );
    }
}
