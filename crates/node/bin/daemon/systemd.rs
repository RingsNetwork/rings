//! Executes systemd user-manager commands and delegates unit rendering and snapshot reduction to
//! effect-free sibling modules. One `systemctl show` call supplies both reported status axes.
#![cfg(any(target_os = "linux", all(test, unix)))]

#[cfg(target_os = "linux")]
use std::env;
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

use super::wait_for_running;
use super::write_atomic;
use super::AutostartState;
use super::CommandRunner;
use super::DaemonError;
use super::DaemonObservation;
use super::DaemonStatus;
use super::PollSchedule;
use super::ProcessCommandRunner;
use super::ServiceManager;
use super::ServiceSpec;
#[cfg(target_os = "linux")]
use super::MANAGER_OBSERVATION_SCHEDULE;

pub(super) mod model;
mod status;
use model::render_systemd_unit;
use model::SystemdDefinitionError;
#[cfg(test)]
pub(super) use model::SYSTEMD_RESTART_DELAY;
use status::parse_systemd_observation;
use status::SystemdStatusError;

#[derive(Debug, Error)]
pub(super) enum SystemdError {
    #[error(transparent)]
    Definition(#[from] SystemdDefinitionError),
    #[error(transparent)]
    Status(#[from] SystemdStatusError),
}

impl From<SystemdDefinitionError> for DaemonError {
    fn from(error: SystemdDefinitionError) -> Self {
        SystemdError::from(error).into()
    }
}

impl From<SystemdStatusError> for DaemonError {
    fn from(error: SystemdStatusError) -> Self {
        SystemdError::from(error).into()
    }
}

const SYSTEMD_UNIT: &str = "rings-node.service";
// Verified against systemd 257.13 packaging conventions: Linux has no distribution-independent
// fixed systemctl path. PATH lookup is required for NixOS and other non-FHS systems, so the
// portability requirement outweighs the lookup surface here.
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
pub(super) struct SystemdManager<R = ProcessCommandRunner> {
    unit_path: PathBuf,
    poll_schedule: PollSchedule,
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
            poll_schedule: MANAGER_OBSERVATION_SCHEDULE,
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
        let definition = render_systemd_unit(spec)?;
        write_atomic(&self.unit_path, &definition)?;
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"])?;
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT])?;
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT])?;
        self.settle_status()
    }

    fn stop(&self) -> Result<DaemonStatus, DaemonError> {
        let status = self.observe()?;
        if matches!(status, DaemonStatus::NotInstalled) {
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
            return self.settle_status();
        }
        if matches!(self.observe()?, DaemonStatus::NotInstalled) {
            return Err(DaemonError::ServiceNotInstalled {
                path: self.unit_path.clone(),
            });
        }
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT])?;
        self.settle_status()
    }

    fn observe(&self) -> Result<DaemonStatus, DaemonError> {
        Ok(self.observe_snapshot()?.into_status())
    }
}

impl<R> SystemdManager<R>
where R: CommandRunner
{
    fn observe_snapshot(&self) -> Result<DaemonObservation<AutostartState>, DaemonError> {
        let output = self.runner.run_checked(SYSTEMCTL, &SYSTEMD_STATUS_ARGS)?;
        Ok(parse_systemd_observation(&String::from_utf8_lossy(
            &output.stdout,
        ))?)
    }

    fn settle_status(&self) -> Result<DaemonStatus, DaemonError> {
        let snapshot = wait_for_running(self.poll_schedule, || self.observe_snapshot())?;
        Ok(snapshot.into_status())
    }
}

fn systemd_config_home(home: &Path, candidate: Option<&Path>) -> PathBuf {
    candidate
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"))
}

#[cfg(test)]
mod tests {
    //! Scripts the systemd command boundary and verifies lifecycle polling consumes every step.

    use rings_node::logging::LogLevel;

    use super::super::super::RuntimeFlavor;
    use super::super::tests::command_runner::CommandStep;
    use super::super::tests::command_runner::ScriptedCommandRunner;
    use super::super::tests::service_spec;
    use super::super::tests::TestRoot;
    use super::super::AutostartState;
    use super::super::DaemonFailure;
    use super::super::DaemonState;
    use super::super::TEST_OBSERVATION_SCHEDULE;
    use super::*;

    const DETACHED_RUNNING_STATUS: &str =
        "LoadState=not-found\nActiveState=active\nSubState=running\nUnitFileState=\n";
    const NOT_INSTALLED_STATUS: &str =
        "LoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\n";

    fn test_root(name: &str) -> TestRoot {
        TestRoot::new("systemd", name)
    }

    fn test_manager(
        root: &Path,
        runner: ScriptedCommandRunner,
    ) -> SystemdManager<ScriptedCommandRunner> {
        SystemdManager {
            unit_path: root.join("systemd/user").join(SYSTEMD_UNIT),
            poll_schedule: TEST_OBSERVATION_SCHEDULE,
            runner,
        }
    }

    fn detached_manager(runner: ScriptedCommandRunner) -> SystemdManager<ScriptedCommandRunner> {
        SystemdManager {
            unit_path: PathBuf::from("/definition/does/not/exist"),
            poll_schedule: TEST_OBSERVATION_SCHEDULE,
            runner,
        }
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
    fn start_reporting_includes_the_systemd_exit_cause() -> Result<(), DaemonError> {
        let failed_status = "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n";
        let root = test_root("start-reporting-failure");
        let mut steps = vec![
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
        ];
        for _ in 0..=TEST_OBSERVATION_SCHEDULE.retries {
            steps.push(CommandStep::success(
                SYSTEMCTL,
                &SYSTEMD_STATUS_ARGS,
                failed_status,
            ));
        }
        let runner = ScriptedCommandRunner::new(steps);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;
        let status = manager.start(&spec)?;

        let error = super::super::report_started(&manager, status);
        let expected = DaemonStatus::installed(
            DaemonState::Restarting(Some(DaemonFailure::described("exit code 78"))),
            AutostartState::Enabled,
        );

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart { status }) if status == expected
        ));
        Ok(())
    }

    #[test]
    fn start_waits_through_auto_restart_until_the_unit_runs() -> Result<(), DaemonError> {
        let root = test_root("start-auto-restart-pending");
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
            CommandStep::success(
                SYSTEMCTL,
                &SYSTEMD_STATUS_ARGS,
                "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n",
            ),
            CommandStep::success(
                SYSTEMCTL,
                &SYSTEMD_STATUS_ARGS,
                "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nResult=success\n",
            ),
        ]);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

        let status = manager.start(&spec)?;

        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
        );
        Ok(())
    }

    #[test]
    fn stop_targets_an_active_unit_when_the_local_definition_is_missing() -> Result<(), DaemonError>
    {
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, DETACHED_RUNNING_STATUS),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "stop", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, NOT_INSTALLED_STATUS),
        ]);
        let manager = detached_manager(runner);

        manager.stop()?;

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
        let manager = detached_manager(runner);

        let result = manager.observe();

        assert!(matches!(result, Err(DaemonError::CommandFailed(_))));
    }

    #[test]
    fn start_installs_definition_then_reload_enables_and_restarts() -> Result<(), DaemonError> {
        let root = test_root("start-sequence");
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
            CommandStep::success(
                SYSTEMCTL,
                &SYSTEMD_STATUS_ARGS,
                "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nExecMainCode=0\nExecMainStatus=0\nResult=success\n",
            ),
        ]);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

        let status = manager.start(&spec)?;

        assert!(manager.unit_path.is_file());
        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
        );
        Ok(())
    }

    #[test]
    fn restart_targets_an_active_unit_when_the_local_definition_is_missing(
    ) -> Result<(), DaemonError> {
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, DETACHED_RUNNING_STATUS),
            CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
            CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, DETACHED_RUNNING_STATUS),
        ]);
        let manager = detached_manager(runner);

        let status = manager.restart()?;

        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Unknown)
        );
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

        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Disabled)
        );
        Ok(())
    }

    #[test]
    fn stop_returns_not_installed_without_issuing_stop() -> Result<(), DaemonError> {
        for status_output in [
            NOT_INSTALLED_STATUS,
            "LoadState=masked\nActiveState=inactive\nSubState=dead\nUnitFileState=masked\n",
        ] {
            let runner = ScriptedCommandRunner::new([CommandStep::success(
                SYSTEMCTL,
                &SYSTEMD_STATUS_ARGS,
                status_output,
            )]);
            let manager = detached_manager(runner);

            let status = manager.stop()?;

            assert_eq!(status, DaemonStatus::NotInstalled);
        }
        Ok(())
    }

    #[test]
    fn restart_rejects_a_not_installed_unit() {
        for status_output in [
            NOT_INSTALLED_STATUS,
            "LoadState=masked\nActiveState=inactive\nSubState=dead\nUnitFileState=masked\n",
        ] {
            let runner = ScriptedCommandRunner::new([CommandStep::success(
                SYSTEMCTL,
                &SYSTEMD_STATUS_ARGS,
                status_output,
            )]);
            let manager = detached_manager(runner);

            let result = manager.restart();

            assert!(matches!(
                result,
                Err(DaemonError::ServiceNotInstalled { .. })
            ));
        }
    }
}
