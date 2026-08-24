#![cfg(any(target_os = "linux", all(test, unix)))]

#[cfg(target_os = "linux")]
use std::env;
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

use super::run_checked;
use super::write_atomic;
use super::AutostartState;
use super::CommandRunner;
use super::DaemonError;
use super::DaemonFailure;
use super::DaemonState;
use super::DaemonStatus;
use super::FailureBoundary;
use super::ProcessCommandRunner;
use super::ServiceManager;
use super::ServiceSpec;

const SYSTEMD_UNIT: &str = "rings-node.service";
// Resolve systemctl through PATH for non-FHS systems such as NixOS.
const SYSTEMCTL: &str = "systemctl";
const SYSTEMD_STATUS_ARGS: [&str; 10] = [
    "--user",
    "show",
    "--all",
    "--property=LoadState",
    "--property=ActiveState",
    "--property=UnitFileState",
    "--property=ExecMainCode",
    "--property=ExecMainStatus",
    "--property=Result",
    SYSTEMD_UNIT,
];
// Linux `siginfo_t::si_code` values published by systemd's ExecMainCode property.
const SYSTEMD_EXEC_CODE_EXITED: i32 = 1;
const SYSTEMD_EXEC_CODE_KILLED: i32 = 2;
const SYSTEMD_EXEC_CODE_DUMPED: i32 = 3;

#[derive(Debug, Error)]
pub(super) enum SystemdDefinitionError {
    // Debug formatting keeps the rejected control character escaped in diagnostics.
    #[error(
        "working directory contains an ASCII control character and cannot be written safely to a systemd unit: {value:?}"
    )]
    WorkingDirectoryContainsControlCharacter { value: String },
    #[error(
        "working directory ends in a backslash that would continue the systemd unit line: {value:?}"
    )]
    WorkingDirectoryEndsWithBackslash { value: String },
}

pub(super) struct SystemdManager<R = ProcessCommandRunner> {
    unit_path: PathBuf,
    runner: R,
}

#[cfg(target_os = "linux")]
impl SystemdManager<ProcessCommandRunner> {
    pub(super) fn discover() -> Result<Self, DaemonError> {
        let home = home::home_dir().ok_or(DaemonError::HomeDirectoryUnavailable)?;
        let xdg_config_home = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let config_home = systemd_config_home(&home, xdg_config_home.as_deref());
        Ok(Self {
            unit_path: config_home.join("systemd").join("user").join(SYSTEMD_UNIT),
            runner: ProcessCommandRunner,
        })
    }
}

impl<R> ServiceManager for SystemdManager<R>
where R: CommandRunner
{
    fn name(&self) -> &'static str {
        "systemd --user"
    }

    fn definition_path(&self) -> &Path {
        &self.unit_path
    }

    fn start(&self, spec: &ServiceSpec) -> Result<FailureBoundary, DaemonError> {
        write_atomic(&self.unit_path, &render_systemd_unit(spec)?)?;
        reload_enable_restart(&self.runner)?;
        Ok(FailureBoundary::Unambiguous)
    }

    fn stop(&self) -> Result<(), DaemonError> {
        if self.state()?.is_installed() {
            run_checked(&self.runner, SYSTEMCTL, &["--user", "stop", SYSTEMD_UNIT])?;
        }
        Ok(())
    }

    fn restart(&self) -> Result<FailureBoundary, DaemonError> {
        if self.unit_path.is_file() {
            reload_enable_restart(&self.runner)?;
            return Ok(FailureBoundary::Unambiguous);
        }
        if !self.state()?.is_installed() {
            return Err(DaemonError::ServiceNotInstalled {
                path: self.unit_path.clone(),
            });
        }
        run_checked(&self.runner, SYSTEMCTL, &[
            "--user",
            "restart",
            SYSTEMD_UNIT,
        ])?;
        Ok(FailureBoundary::Unambiguous)
    }

    fn state(&self) -> Result<DaemonState, DaemonError> {
        systemd_status(&self.runner).map(|status| status.state)
    }

    fn autostart(&self) -> Result<AutostartState, DaemonError> {
        systemd_status(&self.runner).map(|status| status.autostart)
    }

    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        systemd_status(&self.runner)
    }
}

fn reload_enable_restart<R>(runner: &R) -> Result<(), DaemonError>
where R: CommandRunner + ?Sized {
    run_checked(runner, SYSTEMCTL, &["--user", "daemon-reload"])?;
    run_checked(runner, SYSTEMCTL, &["--user", "enable", SYSTEMD_UNIT])?;
    run_checked(runner, SYSTEMCTL, &["--user", "restart", SYSTEMD_UNIT])?;
    Ok(())
}

fn systemd_status<R>(runner: &R) -> Result<DaemonStatus, DaemonError>
where R: CommandRunner + ?Sized {
    let output = run_checked(runner, SYSTEMCTL, &SYSTEMD_STATUS_ARGS)?;
    parse_systemd_status(&String::from_utf8_lossy(&output.stdout))
}

fn systemd_config_home(home: &Path, candidate: Option<&Path>) -> PathBuf {
    candidate
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"))
}

fn render_systemd_unit(spec: &ServiceSpec) -> Result<String, DaemonError> {
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
    if value.chars().any(|character| character.is_ascii_control()) {
        return Err(
            SystemdDefinitionError::WorkingDirectoryContainsControlCharacter {
                value: value.to_owned(),
            },
        );
    }
    if value.ends_with('\\') {
        return Err(SystemdDefinitionError::WorkingDirectoryEndsWithBackslash {
            value: value.to_owned(),
        });
    }
    Ok(value.replace('%', "%%"))
}

fn parse_systemd_state(output: &str) -> DaemonState {
    match output.trim() {
        "active" => DaemonState::Running,
        "inactive" => DaemonState::Stopped,
        "failed" => DaemonState::Failed(None),
        "activating" | "reloading" => DaemonState::Starting,
        "deactivating" => DaemonState::Stopping,
        "" => DaemonState::Unknown("empty systemctl response".to_owned()),
        other => DaemonState::Unknown(other.to_owned()),
    }
}

fn parse_systemd_status(output: &str) -> Result<DaemonStatus, DaemonError> {
    let mut load_state = None;
    let mut active_state = None;
    let mut unit_file_state = None;
    let mut exec_main_code = None;
    let mut exec_main_status = None;
    let mut service_result = None;
    for line in output.lines() {
        let Some((property, value)) = line.split_once('=') else {
            continue;
        };
        match property {
            "LoadState" => load_state = Some(value),
            "ActiveState" => active_state = Some(value),
            "UnitFileState" => unit_file_state = Some(value),
            "ExecMainCode" => exec_main_code = Some(value),
            "ExecMainStatus" => exec_main_status = Some(value),
            "Result" => service_result = Some(value),
            _ => {}
        }
    }
    let load_state = load_state.ok_or(DaemonError::MalformedServiceStatus {
        manager: "systemd --user",
        detail: "missing LoadState property",
    })?;
    let active_state = active_state.ok_or(DaemonError::MalformedServiceStatus {
        manager: "systemd --user",
        detail: "missing ActiveState property",
    })?;
    let state = attribute_systemd_failure(
        parse_systemd_status_state(load_state, active_state),
        active_state,
        service_result,
        parse_systemd_failure(exec_main_code, exec_main_status),
    );
    let autostart = if matches!(state, DaemonState::NotInstalled) {
        AutostartState::Disabled
    } else {
        parse_systemd_autostart(unit_file_state.unwrap_or_default())
    };
    Ok(DaemonStatus { state, autostart })
}

fn attribute_systemd_failure(
    state: DaemonState,
    active_state: &str,
    service_result: Option<&str>,
    failure: Option<DaemonFailure>,
) -> DaemonState {
    match (state, failure) {
        (DaemonState::Failed(None), failure) => DaemonState::Failed(failure),
        // The generated Type=simple unit cannot remain in ordinary startup. A terminated main
        // process plus a failed Result and `activating` means Restart=on-failure is waiting to
        // spawn the next attempt.
        (DaemonState::Starting, Some(failure))
            if active_state == "activating"
                && service_result.is_some_and(is_failed_systemd_result) =>
        {
            DaemonState::Failed(Some(failure))
        }
        (state, _) => state,
    }
}

fn is_failed_systemd_result(result: &str) -> bool {
    !result.is_empty() && result != "success"
}

fn parse_systemd_failure(code: Option<&str>, status: Option<&str>) -> Option<DaemonFailure> {
    let code = code?.parse::<i32>().ok()?;
    let status = status?.parse::<i32>().ok()?;
    match code {
        SYSTEMD_EXEC_CODE_EXITED if status > 0 => Some(DaemonFailure::ExitCode(status)),
        SYSTEMD_EXEC_CODE_KILLED | SYSTEMD_EXEC_CODE_DUMPED if status > 0 => {
            Some(DaemonFailure::Signal {
                name: None,
                number: status,
            })
        }
        _ => None,
    }
}

fn parse_systemd_status_state(load_state: &str, active_state: &str) -> DaemonState {
    match (load_state, active_state) {
        ("not-found", "inactive") => DaemonState::NotInstalled,
        ("loaded" | "not-found", state) => parse_systemd_state(state),
        (load_state, _) => DaemonState::Unknown(format!("load state: {load_state}")),
    }
}

fn parse_systemd_autostart(output: &str) -> AutostartState {
    match output.trim() {
        "enabled" | "enabled-runtime" | "linked" | "linked-runtime" | "alias" => {
            AutostartState::Enabled
        }
        "disabled" | "masked" | "masked-runtime" => AutostartState::Disabled,
        _ => AutostartState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;

    use rings_node::logging::LogLevel;

    use super::super::super::RuntimeFlavor;
    use super::super::tests::command_runner::CommandStep;
    use super::super::tests::command_runner::ScriptedCommandRunner;
    use super::super::tests::service_spec;
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "rings-daemon-systemd-{name}-{}",
            std::process::id()
        ))
    }

    fn test_manager(
        root: &Path,
        runner: ScriptedCommandRunner,
    ) -> SystemdManager<ScriptedCommandRunner> {
        SystemdManager {
            unit_path: root.join("systemd/user").join(SYSTEMD_UNIT),
            runner,
        }
    }

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
    fn config_home_accepts_only_absolute_xdg_paths() {
        let home = Path::new("/home/test");

        assert_eq!(
            systemd_config_home(home, Some(Path::new("/srv/test-config"))),
            Path::new("/srv/test-config")
        );
        assert_eq!(
            systemd_config_home(home, None),
            Path::new("/home/test/.config")
        );
        assert_eq!(
            systemd_config_home(home, Some(Path::new("relative-config"))),
            Path::new("/home/test/.config")
        );
    }

    #[test]
    fn state_parser_preserves_lifecycle_states() {
        assert_eq!(parse_systemd_state("active\n"), DaemonState::Running);
        assert_eq!(parse_systemd_state("inactive\n"), DaemonState::Stopped);
        assert_eq!(parse_systemd_state("failed\n"), DaemonState::Failed(None));
        assert_eq!(parse_systemd_state("activating\n"), DaemonState::Starting);
        assert_eq!(parse_systemd_state("deactivating\n"), DaemonState::Stopping);
        assert_eq!(
            parse_systemd_autostart("enabled\n"),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_systemd_autostart("indirect\n"),
            AutostartState::Unknown
        );
    }

    #[test]
    fn status_parser_preserves_systemd_exit_and_signal_causes() -> Result<(), DaemonError> {
        let exited = parse_systemd_status(
            "LoadState=loaded\nActiveState=activating\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n",
        )?;
        let killed = parse_systemd_status(
            "LoadState=loaded\nActiveState=failed\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=15\n",
        )?;
        let dumped = parse_systemd_status(
            "LoadState=loaded\nActiveState=failed\nUnitFileState=enabled\nExecMainCode=3\nExecMainStatus=6\n",
        )?;

        assert_eq!(
            exited.state,
            DaemonState::Failed(Some(DaemonFailure::ExitCode(78)))
        );
        assert_eq!(
            killed.state,
            DaemonState::Failed(Some(DaemonFailure::Signal {
                name: None,
                number: 15,
            }))
        );
        assert_eq!(
            dumped.state,
            DaemonState::Failed(Some(DaemonFailure::Signal {
                name: None,
                number: 6,
            }))
        );
        Ok(())
    }

    #[test]
    fn failure_parser_rejects_nonterminal_or_malformed_process_status() {
        for (code, status) in [
            (Some("0"), Some("78")),
            (Some("1"), Some("0")),
            (Some("1"), Some("-1")),
            (Some("2"), Some("0")),
            (Some("4"), Some("9")),
            (Some("invalid"), Some("78")),
            (Some("1"), Some("invalid")),
            (None, Some("78")),
            (Some("1"), None),
        ] {
            assert_eq!(parse_systemd_failure(code, status), None);
        }
    }

    #[test]
    fn status_parser_keeps_an_unfailed_activating_unit_starting() -> Result<(), DaemonError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=activating\nUnitFileState=enabled\nExecMainCode=0\nExecMainStatus=0\nResult=success\n",
        )?;

        assert_eq!(status.state, DaemonState::Starting);
        Ok(())
    }

    #[test]
    fn status_parser_does_not_attribute_a_healthy_restart_signal() -> Result<(), DaemonError> {
        for result in ["success", ""] {
            let status = parse_systemd_status(&format!(
                "LoadState=loaded\nActiveState=activating\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=15\nResult={result}\n"
            ))?;

            assert_eq!(status.state, DaemonState::Starting);
        }
        Ok(())
    }

    #[test]
    fn start_reporting_includes_the_systemd_exit_cause() -> anyhow::Result<()> {
        let failed_status = "LoadState=loaded\nActiveState=activating\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n";
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, failed_status),
            CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, failed_status),
        ]);
        let manager = SystemdManager {
            unit_path: PathBuf::from("/definition/rings-node.service"),
            runner,
        };

        let error = super::super::report_started(&manager, FailureBoundary::Unambiguous)
            .err()
            .map(|error| error.to_string());

        assert_eq!(
            error.as_deref(),
            Some(
                "the daemon did not reach the running state; current state: failed (exit code 78)"
            )
        );
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn status_parser_uses_manager_load_state_instead_of_definition_path() -> Result<(), DaemonError>
    {
        let running =
            parse_systemd_status("LoadState=loaded\nActiveState=active\nUnitFileState=enabled\n")?;
        let detached_running =
            parse_systemd_status("LoadState=not-found\nActiveState=active\nUnitFileState=\n")?;
        let missing =
            parse_systemd_status("LoadState=not-found\nActiveState=inactive\nUnitFileState=\n")?;

        assert_eq!(running, DaemonStatus {
            state: DaemonState::Running,
            autostart: AutostartState::Enabled,
        });
        assert_eq!(detached_running, DaemonStatus {
            state: DaemonState::Running,
            autostart: AutostartState::Unknown,
        });
        assert_eq!(missing, DaemonStatus {
            state: DaemonState::NotInstalled,
            autostart: AutostartState::Disabled,
        });
        Ok(())
    }

    #[test]
    fn stop_targets_an_active_unit_when_the_local_definition_is_missing() -> Result<(), DaemonError>
    {
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                "systemctl",
                &SYSTEMD_STATUS_ARGS,
                "LoadState=not-found\nActiveState=active\nUnitFileState=\n",
            ),
            CommandStep::success(SYSTEMCTL, &["--user", "stop", SYSTEMD_UNIT], ""),
        ]);
        let manager = SystemdManager {
            unit_path: PathBuf::from("/definition/does/not/exist"),
            runner,
        };

        manager.stop()?;

        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn status_preserves_systemctl_connection_failures() {
        let runner = ScriptedCommandRunner::new([CommandStep::failure(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            1,
            "Failed to connect to bus",
        )]);
        let manager = SystemdManager {
            unit_path: PathBuf::from("/definition/does/not/exist"),
            runner,
        };

        let result = manager.status();

        assert!(matches!(result, Err(DaemonError::CommandFailed(_))));
        manager.runner.assert_exhausted();
    }

    #[test]
    fn start_installs_definition_then_reload_enables_and_restarts() -> Result<(), DaemonError> {
        let root = test_root("start-sequence");
        let _ = fs::remove_dir_all(&root);
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &["--user", "daemon-reload"], ""),
            CommandStep::success(SYSTEMCTL, &["--user", "enable", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &["--user", "restart", SYSTEMD_UNIT], ""),
        ]);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

        let boundary = manager.start(&spec)?;

        assert!(manager.unit_path.is_file());
        assert_eq!(boundary, FailureBoundary::Unambiguous);
        manager.runner.assert_exhausted();
        assert!(fs::remove_dir_all(&root).is_ok());
        Ok(())
    }

    #[test]
    fn restart_targets_an_active_unit_when_the_local_definition_is_missing(
    ) -> Result<(), DaemonError> {
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                "systemctl",
                &SYSTEMD_STATUS_ARGS,
                "LoadState=not-found\nActiveState=active\nUnitFileState=\n",
            ),
            CommandStep::success(SYSTEMCTL, &["--user", "restart", SYSTEMD_UNIT], ""),
        ]);
        let manager = SystemdManager {
            unit_path: PathBuf::from("/definition/does/not/exist"),
            runner,
        };

        let boundary = manager.restart()?;

        assert_eq!(boundary, FailureBoundary::Unambiguous);
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn restart_of_installed_unit_reloads_enables_and_restarts() -> Result<(), DaemonError> {
        let root = test_root("restart-sequence");
        let _ = fs::remove_dir_all(&root);
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &["--user", "daemon-reload"], ""),
            CommandStep::success(SYSTEMCTL, &["--user", "enable", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &["--user", "restart", SYSTEMD_UNIT], ""),
        ]);
        let manager = test_manager(&root, runner);
        write_atomic(&manager.unit_path, "installed")?;

        let boundary = manager.restart()?;

        assert_eq!(boundary, FailureBoundary::Unambiguous);
        manager.runner.assert_exhausted();
        assert!(fs::remove_dir_all(&root).is_ok());
        Ok(())
    }

    #[test]
    fn exec_quote_escapes_service_manager_expansion_characters() {
        assert_eq!(
            systemd_exec_quote("/tmp/a $HOME/%n/\"rings\""),
            "\"/tmp/a $$HOME/%%n/\\\"rings\\\"\""
        );
    }

    #[test]
    fn working_directory_preserves_raw_characters_and_escapes_only_specifiers() {
        assert!(matches!(
            systemd_working_directory("/tmp/a $HOME/%n/\\rings/\"node\"/'worker'"),
            Ok(path) if path == "/tmp/a $HOME/%%n/\\rings/\"node\"/'worker'"
        ));
    }

    #[test]
    fn working_directory_rejects_ascii_control_characters() {
        for character in [
            '\0', '\u{7}', '\u{8}', '\t', '\n', '\u{b}', '\u{c}', '\r', '\u{1f}', '\u{7f}',
        ] {
            let path = format!("/tmp/rings{character}daemon");
            assert!(matches!(
                systemd_working_directory(&path),
                Err(SystemdDefinitionError::WorkingDirectoryContainsControlCharacter { .. })
            ));
        }
    }

    #[test]
    fn working_directory_rejects_a_trailing_line_continuation() {
        assert!(matches!(
            systemd_working_directory("/tmp/rings\\"),
            Err(SystemdDefinitionError::WorkingDirectoryEndsWithBackslash { .. })
        ));
    }
}
