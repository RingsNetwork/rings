//! systemd user-manager adapter for the Linux daemon.
#![cfg(any(target_os = "linux", all(test, unix)))]

#[cfg(target_os = "linux")]
use std::env;
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

use super::command_output_value;
use super::wait_for_running;
use super::write_atomic;
use super::AutostartState;
use super::CommandRunner;
use super::DaemonError;
use super::DaemonFailure;
use super::DaemonManagerFailure;
use super::DaemonState;
use super::DaemonStatus;
use super::ProcessCommandRunner;
use super::ServiceManager;
use super::ServiceSpec;

const SYSTEMD_UNIT: &str = "rings-node.service";
// Resolve systemctl through PATH for non-FHS systems such as NixOS.
const SYSTEMCTL: &str = "systemctl";
const SYSTEMD_USER_ARG: &str = "--user";
const SYSTEMD_MANAGER: &str = "systemd --user";
const SYSTEMD_STATUS_ARGS: [&str; 11] = [
    SYSTEMD_USER_ARG,
    "show",
    "--all",
    "--property=LoadState",
    "--property=ActiveState",
    "--property=SubState",
    "--property=UnitFileState",
    "--property=ExecMainCode",
    "--property=ExecMainStatus",
    "--property=Result",
    SYSTEMD_UNIT,
];
// Verified with systemd 257.13: ExecMainCode publishes Linux `siginfo_t::si_code`.
const SYSTEMD_EXEC_CODE_EXITED: i32 = 1;
const SYSTEMD_EXEC_CODE_KILLED: i32 = 2;
const SYSTEMD_EXEC_CODE_DUMPED: i32 = 3;

#[derive(Debug, Error)]
pub(super) enum SystemdDefinitionError {
    // Debug formatting keeps the rejected line break escaped in diagnostics.
    #[error("working directory contains a line break and cannot be written safely to a systemd unit: {value:?}")]
    ContainsLineBreak { value: String },
    #[error("working directory has leading or trailing whitespace that systemd would discard: {value:?}")]
    HasBoundaryWhitespace { value: String },
    #[error(
        "working directory ends in a backslash that would continue the systemd unit line: {value:?}"
    )]
    EndsWithBackslash { value: String },
}

pub(super) struct SystemdManager<R = ProcessCommandRunner> {
    unit_path: PathBuf,
    runner: R,
}

impl<R> SystemdManager<R> {
    fn has_definition(&self) -> bool {
        // This decides whether restart must reload our local unit. Status uses systemd's LoadState
        // as the authoritative installation evidence, including active units without this file.
        self.unit_path.is_file()
    }
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
        SYSTEMD_MANAGER
    }

    fn definition_path(&self) -> &Path {
        &self.unit_path
    }

    fn start(&self, spec: &ServiceSpec) -> Result<DaemonStatus, DaemonError> {
        write_atomic(&self.unit_path, &render_systemd_unit(spec)?)?;
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"])?;
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT])?;
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT])?;
        wait_for_running(|| self.observe())
    }

    fn stop(&self) -> Result<DaemonStatus, DaemonError> {
        let status = self.observe()?;
        if !status.state.is_installed() {
            return Ok(status);
        }
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "stop", SYSTEMD_UNIT])?;
        self.observe()
    }

    fn restart(&self) -> Result<DaemonStatus, DaemonError> {
        if self.has_definition() {
            self.runner
                .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"])?;
            self.runner
                .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT])?;
            return wait_for_running(|| self.observe());
        }
        if !self.observe()?.state.is_installed() {
            return Err(DaemonError::ServiceNotInstalled {
                path: self.unit_path.clone(),
            });
        }
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT])?;
        wait_for_running(|| self.observe())
    }

    fn observe(&self) -> Result<DaemonStatus, DaemonError> {
        let output = self.runner.run_checked(SYSTEMCTL, &SYSTEMD_STATUS_ARGS)?;
        parse_systemd_status(&String::from_utf8_lossy(&output.stdout))
    }
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
    if value
        .chars()
        .any(|character| matches!(character, '\n' | '\r'))
    {
        return Err(SystemdDefinitionError::ContainsLineBreak {
            value: value.to_owned(),
        });
    }
    if value != value.trim() {
        return Err(SystemdDefinitionError::HasBoundaryWhitespace {
            value: value.to_owned(),
        });
    }
    if value.ends_with('\\') {
        return Err(SystemdDefinitionError::EndsWithBackslash {
            value: value.to_owned(),
        });
    }
    Ok(value.replace('%', "%%"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SystemdLifecycle<'a> {
    NotInstalled,
    Running,
    Stopped,
    Failed,
    Starting,
    AutoRestarting,
    Reloading,
    Stopping,
    Other {
        load: &'a str,
        active: &'a str,
        sub: &'a str,
    },
}

impl SystemdLifecycle<'_> {
    fn is_installed(self) -> bool {
        !matches!(self, Self::NotInstalled)
    }

    fn into_state(self, failure: Option<DaemonFailure>) -> DaemonState {
        match self {
            Self::NotInstalled => DaemonState::NotInstalled,
            Self::Running => DaemonState::Running,
            Self::Stopped => DaemonState::Stopped,
            Self::Failed | Self::AutoRestarting => DaemonState::Failed(failure),
            Self::Starting | Self::Reloading => DaemonState::Starting,
            Self::Stopping => DaemonState::Stopping,
            Self::Other { load, active, sub } => DaemonState::Unknown(format!(
                "load state: {load}, active state: {active}, substate: {sub}"
            )),
        }
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
    fn failure(self, code: Option<&str>, status: Option<&str>) -> Option<DaemonFailure> {
        match self {
            Self::ExitCode => {
                parse_systemd_process_failure(code, status, SYSTEMD_EXEC_CODE_EXITED, false)
            }
            Self::Signal => {
                parse_systemd_process_failure(code, status, SYSTEMD_EXEC_CODE_KILLED, false)
            }
            Self::CoreDump => {
                parse_systemd_process_failure(code, status, SYSTEMD_EXEC_CODE_DUMPED, true)
            }
            Self::Timeout => Some(DaemonFailure::Manager(DaemonManagerFailure::Timeout)),
            Self::Watchdog => Some(DaemonFailure::Manager(DaemonManagerFailure::Watchdog)),
            Self::OutOfMemory => Some(DaemonFailure::Manager(DaemonManagerFailure::OutOfMemory)),
            Self::StartLimit => Some(DaemonFailure::Manager(DaemonManagerFailure::StartLimit)),
            Self::Protocol => Some(DaemonFailure::Manager(DaemonManagerFailure::Protocol)),
            Self::Resources => Some(DaemonFailure::Manager(DaemonManagerFailure::Resources)),
            Self::Success | Self::Other(_) | Self::Missing => None,
        }
    }
}

fn parse_systemd_status(output: &str) -> Result<DaemonStatus, DaemonError> {
    let load_state = command_output_value(output, "LoadState", "=").ok_or(
        DaemonError::MalformedServiceStatus {
            manager: SYSTEMD_MANAGER,
            detail: "missing LoadState property",
        },
    )?;
    let active_state = command_output_value(output, "ActiveState", "=").ok_or(
        DaemonError::MalformedServiceStatus {
            manager: SYSTEMD_MANAGER,
            detail: "missing ActiveState property",
        },
    )?;
    let sub_state = command_output_value(output, "SubState", "=").unwrap_or_default();
    let lifecycle = parse_systemd_lifecycle(load_state, active_state, sub_state);
    let result = parse_systemd_result(command_output_value(output, "Result", "="));
    let failure = result.failure(
        command_output_value(output, "ExecMainCode", "="),
        command_output_value(output, "ExecMainStatus", "="),
    );
    let autostart = if lifecycle.is_installed() {
        parse_systemd_autostart(
            command_output_value(output, "UnitFileState", "=").unwrap_or_default(),
        )
    } else {
        AutostartState::Disabled
    };
    let state = lifecycle.into_state(failure);
    Ok(DaemonStatus { state, autostart })
}

fn parse_systemd_lifecycle<'a>(
    load: &'a str,
    active: &'a str,
    sub: &'a str,
) -> SystemdLifecycle<'a> {
    match (load, active, sub) {
        ("not-found", "inactive", _) => SystemdLifecycle::NotInstalled,
        ("loaded" | "not-found", "active", _) => SystemdLifecycle::Running,
        ("loaded", "inactive", _) => SystemdLifecycle::Stopped,
        ("loaded" | "not-found", "failed", _) => SystemdLifecycle::Failed,
        ("loaded" | "not-found", "activating", "auto-restart" | "auto-restart-queued") => {
            SystemdLifecycle::AutoRestarting
        }
        ("loaded" | "not-found", "activating", _) => SystemdLifecycle::Starting,
        ("loaded" | "not-found", "reloading", _) => SystemdLifecycle::Reloading,
        ("loaded" | "not-found", "deactivating", _) => SystemdLifecycle::Stopping,
        _ => SystemdLifecycle::Other { load, active, sub },
    }
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

fn parse_systemd_process_failure(
    code: Option<&str>,
    status: Option<&str>,
    expected_code: i32,
    core_dumped: bool,
) -> Option<DaemonFailure> {
    let code = code?.parse::<i32>().ok()?;
    let status = status?.parse::<i32>().ok()?;
    if code != expected_code || status <= 0 {
        return None;
    }
    if code == SYSTEMD_EXEC_CODE_EXITED {
        Some(DaemonFailure::ExitCode(status))
    } else {
        Some(DaemonFailure::Signal {
            name: None,
            number: status,
            core_dumped,
        })
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
    use rings_node::logging::LogLevel;

    use super::super::super::RuntimeFlavor;
    use super::super::tests::command_runner::CommandStep;
    use super::super::tests::command_runner::ScriptedCommandRunner;
    use super::super::tests::service_spec;
    use super::super::tests::TestRoot;
    use super::*;

    const RUNNING_STATUS: &str = "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nExecMainCode=0\nExecMainStatus=0\nResult=success\n";

    fn test_root(name: &str) -> TestRoot {
        TestRoot::new("systemd", name)
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
        assert_eq!(
            parse_systemd_lifecycle("loaded", "active", "running"),
            SystemdLifecycle::Running
        );
        assert_eq!(
            parse_systemd_lifecycle("loaded", "inactive", "dead"),
            SystemdLifecycle::Stopped
        );
        assert_eq!(
            parse_systemd_lifecycle("loaded", "failed", "failed"),
            SystemdLifecycle::Failed
        );
        assert_eq!(
            parse_systemd_lifecycle("loaded", "activating", "start"),
            SystemdLifecycle::Starting
        );
        assert_eq!(
            parse_systemd_lifecycle("loaded", "activating", "auto-restart"),
            SystemdLifecycle::AutoRestarting
        );
        assert_eq!(
            parse_systemd_lifecycle("loaded", "deactivating", "stop-sigterm"),
            SystemdLifecycle::Stopping
        );
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
            "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n",
        )?;
        let killed = parse_systemd_status(
            "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=15\nResult=signal\n",
        )?;
        let dumped = parse_systemd_status(
            "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode=3\nExecMainStatus=6\nResult=core-dump\n",
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
                core_dumped: false,
            }))
        );
        assert_eq!(
            dumped.state,
            DaemonState::Failed(Some(DaemonFailure::Signal {
                name: None,
                number: 6,
                core_dumped: true,
            }))
        );
        Ok(())
    }

    #[test]
    fn failure_parser_rejects_nonterminal_or_malformed_process_status() {
        for (result, code, status) in [
            (SystemdResult::ExitCode, Some("0"), Some("78")),
            (SystemdResult::ExitCode, Some("1"), Some("0")),
            (SystemdResult::ExitCode, Some("1"), Some("-1")),
            (SystemdResult::Signal, Some("2"), Some("0")),
            (SystemdResult::Signal, Some("4"), Some("9")),
            (SystemdResult::ExitCode, Some("invalid"), Some("78")),
            (SystemdResult::ExitCode, Some("1"), Some("invalid")),
            (SystemdResult::ExitCode, None, Some("78")),
            (SystemdResult::ExitCode, Some("1"), None),
        ] {
            assert_eq!(result.failure(code, status), None);
        }
    }

    #[test]
    fn manager_results_are_not_misattributed_to_process_signals() -> Result<(), DaemonError> {
        let cases = [
            ("timeout", DaemonManagerFailure::Timeout),
            ("watchdog", DaemonManagerFailure::Watchdog),
            ("oom-kill", DaemonManagerFailure::OutOfMemory),
            ("start-limit-hit", DaemonManagerFailure::StartLimit),
            ("protocol", DaemonManagerFailure::Protocol),
            ("resources", DaemonManagerFailure::Resources),
        ];
        for (result, expected) in cases {
            let status = parse_systemd_status(&format!(
                "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=9\nResult={result}\n"
            ))?;
            assert_eq!(
                status.state,
                DaemonState::Failed(Some(DaemonFailure::Manager(expected)))
            );
        }
        Ok(())
    }

    #[test]
    fn normal_start_ignores_a_previous_exit_record() -> Result<(), DaemonError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=activating\nSubState=start\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n",
        )?;

        assert_eq!(status.state, DaemonState::Starting);
        Ok(())
    }

    #[test]
    fn status_parser_does_not_attribute_a_healthy_restart_signal() -> Result<(), DaemonError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=activating\nSubState=start\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=15\nResult=success\n",
        )?;

        assert_eq!(status.state, DaemonState::Starting);
        Ok(())
    }

    #[test]
    fn unknown_result_does_not_invent_a_process_cause() -> Result<(), DaemonError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=9\nResult=future-manager-result\n",
        )?;

        assert_eq!(status.state, DaemonState::Failed(None));
        Ok(())
    }

    #[test]
    fn start_reporting_includes_the_systemd_exit_cause() -> Result<(), DaemonError> {
        let failed_status = "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n";
        let root = test_root("start-reporting-failure");
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, failed_status),
        ]);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;
        let status = manager.start(&spec)?;

        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                state: DaemonState::Failed(Some(DaemonFailure::ExitCode(78)))
            })
        ));
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn status_parser_uses_manager_load_state_instead_of_definition_path() -> Result<(), DaemonError>
    {
        let running = parse_systemd_status(
            "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\n",
        )?;
        let detached_running = parse_systemd_status(
            "LoadState=not-found\nActiveState=active\nSubState=running\nUnitFileState=\n",
        )?;
        let missing = parse_systemd_status(
            "LoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\n",
        )?;

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
                "LoadState=not-found\nActiveState=active\nSubState=running\nUnitFileState=\n",
            ),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "stop", SYSTEMD_UNIT], ""),
            CommandStep::success(
                SYSTEMCTL,
                &SYSTEMD_STATUS_ARGS,
                "LoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\n",
            ),
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

        let result = manager.observe();

        assert!(matches!(result, Err(DaemonError::CommandFailed(_))));
        manager.runner.assert_exhausted();
    }

    #[test]
    fn start_installs_definition_then_reload_enables_and_restarts() -> Result<(), DaemonError> {
        let root = test_root("start-sequence");
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, RUNNING_STATUS),
        ]);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

        let status = manager.start(&spec)?;

        assert!(manager.unit_path.is_file());
        assert_eq!(status.state, DaemonState::Running);
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn restart_targets_an_active_unit_when_the_local_definition_is_missing(
    ) -> Result<(), DaemonError> {
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                "systemctl",
                &SYSTEMD_STATUS_ARGS,
                "LoadState=not-found\nActiveState=active\nSubState=running\nUnitFileState=\n",
            ),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
            CommandStep::success(
                SYSTEMCTL,
                &SYSTEMD_STATUS_ARGS,
                "LoadState=not-found\nActiveState=active\nSubState=running\nUnitFileState=\n",
            ),
        ]);
        let manager = SystemdManager {
            unit_path: PathBuf::from("/definition/does/not/exist"),
            runner,
        };

        let status = manager.restart()?;

        assert_eq!(status.state, DaemonState::Running);
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn restart_of_installed_unit_preserves_autostart() -> Result<(), DaemonError> {
        let root = test_root("restart-sequence");
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
            CommandStep::success(
                SYSTEMCTL,
                &SYSTEMD_STATUS_ARGS,
                "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=disabled\nResult=success\n",
            ),
        ]);
        let manager = test_manager(&root, runner);
        write_atomic(&manager.unit_path, "installed")?;

        let status = manager.restart()?;

        assert_eq!(status.autostart, AutostartState::Disabled);
        manager.runner.assert_exhausted();
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
            systemd_working_directory("/tmp/a\t$HOME/%n/\\rings/\"node\"/'worker'/\u{7}"),
            Ok(path) if path == "/tmp/a\t$HOME/%%n/\\rings/\"node\"/'worker'/\u{7}"
        ));
    }

    #[test]
    fn working_directory_rejects_line_breaks_and_boundary_whitespace() {
        for character in ['\n', '\r'] {
            let path = format!("/tmp/rings{character}daemon");
            assert!(matches!(
                systemd_working_directory(&path),
                Err(SystemdDefinitionError::ContainsLineBreak { .. })
            ));
        }
        for path in [" /tmp/rings", "/tmp/rings ", "\t/tmp/rings", "/tmp/rings\t"] {
            assert!(matches!(
                systemd_working_directory(path),
                Err(SystemdDefinitionError::HasBoundaryWhitespace { .. })
            ));
        }
    }

    #[test]
    fn working_directory_rejects_a_trailing_line_continuation() {
        assert!(matches!(
            systemd_working_directory("/tmp/rings\\"),
            Err(SystemdDefinitionError::EndsWithBackslash { .. })
        ));
    }
}
