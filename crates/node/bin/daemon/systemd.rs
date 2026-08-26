//! Owns the systemd adapter effect boundary.
#![cfg(any(target_os = "linux", all(test, unix)))]

#[cfg(target_os = "linux")]
use std::env;
use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

use super::wait_for_start_settlement;
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
use status::parse_systemd_record;
use status::SystemdRecord;
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
        // This local evidence is folded with systemd's manager record by every lifecycle command.
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
        let record = self.observe_record()?;
        if record.is_missing() {
            return Ok(record.into_observation(self.has_definition()).into_status());
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
        if self.observe_record()?.is_missing() {
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

    fn settle_status(&self) -> Result<DaemonStatus, DaemonError> {
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
    //! Exercises the systemd user-manager boundary.

    mod manager;

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
}
