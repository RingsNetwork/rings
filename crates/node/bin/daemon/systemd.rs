//! Confines systemd user-manager subprocess and filesystem effects to one adapter boundary.
#![cfg(any(target_os = "linux", all(test, unix)))]

#[cfg(target_os = "linux")]
use std::env;
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

use super::remove_service_definition;
use super::wait_for_start_settlement;
use super::write_atomic;
use super::AutostartState;
use super::CommandRunner;
use super::DaemonError;
use super::DaemonObservation;
use super::DaemonState;
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
use status::parse_systemd_record;
use status::SystemdRecord;
use status::SystemdStatusError;

#[derive(Debug, Error)]
pub(super) enum SystemdError {
    #[error(transparent)]
    Definition(#[from] SystemdDefinitionError),
    #[error(transparent)]
    Status(#[from] SystemdStatusError),
    #[error("the systemd user unit is unavailable; inspect its load state and repair or unmask it before changing it")]
    UnitUnavailable,
}

impl_daemon_error_from_adapter!(SystemdDefinitionError => SystemdError);
impl_daemon_error_from_adapter!(SystemdStatusError => SystemdError);

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

    fn install(&self, spec: &ServiceSpec) -> Result<DaemonStatus, DaemonError> {
        let definition = render_systemd_unit(spec)?;
        write_atomic(&self.unit_path, &definition)?;
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"])?;
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT])?;
        Ok(self.observe_snapshot()?.into_status())
    }

    fn uninstall(&self) -> Result<DaemonStatus, DaemonError> {
        let record = self.observe_record()?;
        let inactive_without_process = record.is_inactive_without_process();
        if record.is_unavailable() && !inactive_without_process {
            return Err(SystemdError::UnitUnavailable.into());
        }
        let has_definition = self.has_definition();
        let has_enabled_registration = record.has_enabled_registration();
        if !has_definition && !has_enabled_registration && inactive_without_process {
            return Ok(DaemonStatus::NotInstalled);
        }
        if has_definition || has_enabled_registration {
            if inactive_without_process {
                self.runner
                    .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "disable", SYSTEMD_UNIT])?;
            } else {
                self.runner.run_checked(SYSTEMCTL, &[
                    SYSTEMD_USER_ARG,
                    "disable",
                    "--now",
                    SYSTEMD_UNIT,
                ])?;
            }
            remove_service_definition(&self.unit_path)?;
        } else {
            self.runner
                .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "stop", SYSTEMD_UNIT])?;
        }
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"])?;
        Ok(DaemonStatus::NotInstalled)
    }

    fn start(&self) -> Result<DaemonStatus, DaemonError> {
        let record = self.observe_record()?;
        if record.is_running() {
            return Ok(record.into_observation(self.has_definition()).into_status());
        }
        self.validate_lifecycle_target(&record)?;
        if self.has_definition() {
            self.runner
                .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"])?;
        }
        self.lifecycle_and_settle("start")
    }

    fn stop(&self) -> Result<DaemonStatus, DaemonError> {
        let record = self.observe_record()?;
        if record.is_inactive_without_process() {
            // `systemctl stop` turns an already missing or inactive unavailable unit from a benign
            // no-op into a command error, so preserve the manager verdict without issuing it.
            return Ok(record.into_observation(self.has_definition()).into_status());
        }
        if record.is_unavailable() {
            return Err(SystemdError::UnitUnavailable.into());
        }
        let autostart = record.autostart();
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "stop", SYSTEMD_UNIT])?;
        // Post: command success proves stop acted on the loaded record sampled above even if a
        // detached unit disappears from the manager immediately afterward.
        Ok(DaemonStatus::installed(DaemonState::Stopped, autostart))
    }

    fn restart(&self) -> Result<DaemonStatus, DaemonError> {
        let record = self.observe_record()?;
        self.validate_lifecycle_target(&record)?;
        if self.has_definition() {
            self.runner
                .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"])?;
        }
        self.lifecycle_and_settle("restart")
    }

    fn observe(&self) -> Result<DaemonStatus, DaemonError> {
        Ok(self.observe_snapshot()?.into_status())
    }
}

impl<R> SystemdManager<R>
where R: CommandRunner
{
    fn observe_snapshot(&self) -> Result<DaemonObservation<AutostartState>, DaemonError> {
        // Verified with systemd 257.13: LoadState=not-found describes both an absent unit and a
        // running unit whose file was removed. The configured unit path is the local installation
        // evidence when no process lifecycle remains.
        Ok(self
            .observe_record()?
            .into_observation(self.has_definition()))
    }

    fn observe_record(&self) -> Result<SystemdRecord, DaemonError> {
        let output = self.runner.run_checked(SYSTEMCTL, &SYSTEMD_STATUS_ARGS)?;
        Ok(parse_systemd_record(&String::from_utf8_lossy(
            &output.stdout,
        ))?)
    }

    fn validate_lifecycle_target(&self, record: &SystemdRecord) -> Result<(), DaemonError> {
        if record.is_unavailable() {
            return Err(SystemdError::UnitUnavailable.into());
        }
        if record.is_missing() && !self.has_definition() {
            return Err(DaemonError::ServiceNotInstalled {
                path: self.unit_path.clone(),
            });
        }
        Ok(())
    }

    fn lifecycle_and_settle(&self, action: &'static str) -> Result<DaemonStatus, DaemonError> {
        self.runner
            .run_checked(SYSTEMCTL, &[SYSTEMD_USER_ARG, action, SYSTEMD_UNIT])?;
        let snapshot = wait_for_start_settlement(self.poll_schedule, || self.observe_snapshot())?;
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
    //! Ensures lifecycle tests use complete snapshots and a zero-delay observation schedule.

    mod manager;

    use rings_node::logging::LogLevel;

    use super::super::super::RuntimeFlavor;
    use super::super::tests::command_runner::CommandStep;
    use super::super::tests::command_runner::ScriptedCommandRunner;
    use super::super::tests::fill_poll_budget;
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
}
