#![cfg(any(target_os = "linux", all(test, unix)))]

#[cfg(target_os = "linux")]
use std::env;
use std::path::Path;
use std::path::PathBuf;

use super::run_checked;
use super::write_atomic;
use super::AutostartState;
use super::CommandRunner;
use super::DaemonError;
use super::DaemonState;
use super::DaemonStatus;
use super::ProcessCommandRunner;
use super::ServiceManager;
use super::ServiceSpec;

const SYSTEMD_UNIT: &str = "rings-node.service";
// Resolve systemctl through PATH for non-FHS systems such as NixOS.
const SYSTEMCTL: &str = "systemctl";
const SYSTEMD_STATUS_ARGS: [&str; 7] = [
    "--user",
    "show",
    "--all",
    "--property=LoadState",
    "--property=ActiveState",
    "--property=UnitFileState",
    SYSTEMD_UNIT,
];

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

    fn start(&self, spec: &ServiceSpec) -> Result<(), DaemonError> {
        write_atomic(&self.unit_path, &render_systemd_unit(spec))?;
        reload_enable_restart(&self.runner)
    }

    fn stop(&self) -> Result<(), DaemonError> {
        if self.state()?.is_installed() {
            run_checked(&self.runner, SYSTEMCTL, &["--user", "stop", SYSTEMD_UNIT])?;
        }
        Ok(())
    }

    fn restart(&self) -> Result<(), DaemonError> {
        if self.unit_path.is_file() {
            return reload_enable_restart(&self.runner);
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
        ])
        .map(|_| ())
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

fn render_systemd_unit(spec: &ServiceSpec) -> String {
    let command = spec
        .arguments()
        .into_iter()
        .map(systemd_exec_quote)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
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
        systemd_working_directory(&spec.working_directory)
    )
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

fn systemd_working_directory(value: &str) -> String {
    value.replace('%', "%%")
}

fn parse_systemd_state(output: &str) -> DaemonState {
    match output.trim() {
        "active" => DaemonState::Running,
        "inactive" => DaemonState::Stopped,
        "failed" => DaemonState::Failed,
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
    for line in output.lines() {
        let Some((property, value)) = line.split_once('=') else {
            continue;
        };
        match property {
            "LoadState" => load_state = Some(value),
            "ActiveState" => active_state = Some(value),
            "UnitFileState" => unit_file_state = Some(value),
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
    let state = parse_systemd_status_state(load_state, active_state);
    let autostart = if matches!(state, DaemonState::NotInstalled) {
        AutostartState::Disabled
    } else {
        parse_systemd_autostart(unit_file_state.unwrap_or_default())
    };
    Ok(DaemonStatus { state, autostart })
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
        let unit = render_systemd_unit(&spec);

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
        assert_eq!(parse_systemd_state("failed\n"), DaemonState::Failed);
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

        let result = manager.start(&spec);

        assert!(manager.unit_path.is_file());
        manager.runner.assert_exhausted();
        assert!(fs::remove_dir_all(&root).is_ok());
        result
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

        manager.restart()?;

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

        let result = manager.restart();

        manager.runner.assert_exhausted();
        assert!(fs::remove_dir_all(&root).is_ok());
        result
    }

    #[test]
    fn quote_escapes_service_manager_expansion_characters() {
        assert_eq!(
            systemd_exec_quote("/tmp/a $HOME/%n/\"rings\""),
            "\"/tmp/a $$HOME/%%n/\\\"rings\\\"\""
        );
        assert_eq!(
            systemd_working_directory("/tmp/a $HOME/%n/rings"),
            "/tmp/a $HOME/%%n/rings"
        );
    }
}
