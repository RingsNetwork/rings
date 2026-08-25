//! Owns launchd manager effects; pure definition and status policy live in sibling modules.
#![cfg(any(target_os = "macos", all(test, unix)))]

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;

use thiserror::Error;

use super::command_failure;
use super::ensure_parent_directory;
use super::finish_with_recovery;
use super::path_text;
use super::poll_until;
use super::wait_for_running;
use super::write_atomic;
use super::AutostartState;
use super::CommandRunner;
use super::DaemonError;
use super::DaemonObservation;
use super::DaemonStatus;
use super::PollSchedule;
use super::ProcessCommandRunner;
use super::RecoveryFailure;
use super::ServiceManager;
use super::ServiceSpec;
#[cfg(target_os = "macos")]
use super::MANAGER_OBSERVATION_SCHEDULE;

pub(super) mod model;
mod status;

use model::is_service_not_found;
use model::may_be_disabled_bootstrap;
use model::render_launchd_plist;
use model::LaunchdDefinitionError;
#[cfg(test)]
use model::LAUNCHD_BOOTSTRAP_DISABLED;
use model::LAUNCHD_LABEL;
#[cfg(test)]
use model::LAUNCHD_SERVICE_NOT_FOUND;
use status::attribution_after_restart;
use status::parse_launchd_autostart;
use status::parse_launchd_observation;
use status::LaunchdAttribution;
use status::LaunchdRecord;

// macOS provides launchctl at this SIP-protected path. PATH lookup would add a hijack surface.
const LAUNCHCTL: &str = "/bin/launchctl";
const LAUNCHD_MANAGER: &str = "launchd";

#[derive(Debug, Error)]
pub(super) enum LaunchdError {
    #[error(transparent)]
    Definition(#[from] LaunchdDefinitionError),
    #[cfg(target_os = "macos")]
    #[error("could not read the current user id from `{output}`")]
    InvalidUserId { output: String },
    #[error("launchd did not unload the daemon service within the observation budget")]
    ServiceDidNotUnload,
    #[error("could not bootstrap the disabled service while restoring its autostart setting")]
    DisabledBootstrap {
        #[source]
        failure: RecoveryFailure<Box<DaemonError>>,
    },
    #[error("cannot set launchd autostart to non-concrete state {state}")]
    InvalidAutostartMutation { state: AutostartState },
}

impl From<LaunchdDefinitionError> for DaemonError {
    fn from(error: LaunchdDefinitionError) -> Self {
        LaunchdError::from(error).into()
    }
}

pub(super) struct LaunchdManager<R = ProcessCommandRunner> {
    definition_path: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
    domain: String,
    poll_schedule: PollSchedule,
    runner: R,
}

#[cfg(target_os = "macos")]
impl LaunchdManager<ProcessCommandRunner> {
    pub(super) fn discover() -> Result<Self, DaemonError> {
        let home = home::home_dir().ok_or(DaemonError::HomeDirectoryUnavailable)?;
        let runner = ProcessCommandRunner;
        let output = runner.run_checked("/usr/bin/id", &["-u"])?;
        let user_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if user_id.is_empty() || !user_id.chars().all(|character| character.is_ascii_digit()) {
            return Err(LaunchdError::InvalidUserId { output: user_id }.into());
        }
        let domain = format!("gui/{user_id}");
        let logs = home.join(".rings").join("logs");
        Ok(Self {
            definition_path: launchd_definition_path(&home),
            stdout_log: logs.join("daemon.log"),
            stderr_log: logs.join("daemon.error.log"),
            domain,
            poll_schedule: MANAGER_OBSERVATION_SCHEDULE,
            runner,
        })
    }
}

fn launchd_target(domain: &str) -> String {
    format!("{domain}/{LAUNCHD_LABEL}")
}

fn launchd_definition_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

impl<R> LaunchdManager<R>
where R: CommandRunner
{
    fn target(&self) -> String {
        launchd_target(&self.domain)
    }

    fn service_record(&self) -> Result<LaunchdRecord<Output>, DaemonError> {
        let target = self.target();
        let arguments = ["print", target.as_str()];
        let output = self.runner.run(LAUNCHCTL, &arguments)?;
        if output.status.success() {
            return Ok(LaunchdRecord::Loaded(output));
        }
        if is_service_not_found(output.status.code()) {
            return Ok(LaunchdRecord::Missing);
        }
        Err(command_failure(LAUNCHCTL, &arguments, output).into())
    }

    fn is_loaded(&self) -> Result<bool, DaemonError> {
        self.service_record()
            .map(|record| matches!(record, LaunchdRecord::Loaded(_)))
    }

    fn unload_if_loaded(&self) -> Result<(), DaemonError> {
        if !self.is_loaded()? {
            return Ok(());
        }
        let target = self.target();
        self.runner.run_checked(LAUNCHCTL, &["bootout", &target])?;
        let still_loaded = poll_until(self.poll_schedule, || self.is_loaded(), |loaded| !*loaded)?;
        if still_loaded {
            Err(LaunchdError::ServiceDidNotUnload.into())
        } else {
            Ok(())
        }
    }

    fn bootstrap(&self) -> Result<(), DaemonError> {
        let definition = path_text(&self.definition_path)?;
        let arguments = ["bootstrap", self.domain.as_str(), definition.as_str()];
        let output = self.runner.run(LAUNCHCTL, &arguments)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(command_failure(LAUNCHCTL, &arguments, output).into())
        }
    }

    fn set_autostart(&self, state: AutostartState) -> Result<(), DaemonError> {
        let action = match state {
            AutostartState::Enabled => "enable",
            AutostartState::Disabled => "disable",
            AutostartState::Unavailable | AutostartState::Unknown => {
                return Err(LaunchdError::InvalidAutostartMutation { state }.into());
            }
        };
        let target = self.target();
        self.runner.run_checked(LAUNCHCTL, &[action, &target])?;
        Ok(())
    }

    fn bootstrap_preserving_autostart(&self) -> Result<(), DaemonError> {
        let initial = self.bootstrap();
        let Err(error) = initial else {
            return Ok(());
        };
        if !matches!(
            &error,
            DaemonError::CommandFailed(failure)
                if may_be_disabled_bootstrap(failure.status.code())
        ) {
            return Err(error);
        }
        let previous = self.autostart_state()?;
        if previous != AutostartState::Disabled {
            return Err(error);
        }
        self.set_autostart(AutostartState::Enabled)?;
        let bootstrap = self.bootstrap().map_err(Box::new);
        // Observed on macOS 15.6.1 (24G90): disabling a live job does not unload or stop it. Restore
        // the corroborated prior value after either result of the retry.
        let restore = self.set_autostart(previous).map_err(Box::new);
        finish_with_recovery(bootstrap, restore)
            .map_err(|failure| LaunchdError::DisabledBootstrap { failure }.into())
    }

    fn has_definition(&self) -> bool {
        // An unloaded launchd job has no manager record, so the plist is the installation evidence.
        self.definition_path.is_file()
    }

    fn autostart_state(&self) -> Result<AutostartState, DaemonError> {
        let output = self
            .runner
            .run_checked(LAUNCHCTL, &["print-disabled", &self.domain])?;
        Ok(parse_launchd_autostart(
            &String::from_utf8_lossy(&output.stdout),
            LAUNCHD_LABEL,
        ))
    }

    fn observe_lifecycle_with_attribution(
        &self,
        attribution: LaunchdAttribution,
    ) -> Result<DaemonObservation, DaemonError> {
        let definition_present = self.has_definition();
        let record = self.service_record()?;
        let observation = match &record {
            LaunchdRecord::Missing => {
                parse_launchd_observation(LaunchdRecord::Missing, definition_present, attribution)
            }
            LaunchdRecord::Loaded(output) => {
                let text = String::from_utf8_lossy(&output.stdout);
                parse_launchd_observation(
                    LaunchdRecord::Loaded(text.as_ref()),
                    definition_present,
                    attribution,
                )
            }
        };
        Ok(observation)
    }

    fn complete_observation(
        &self,
        observation: DaemonObservation,
    ) -> Result<DaemonStatus, DaemonError> {
        match observation {
            DaemonObservation::NotInstalled => Ok(DaemonStatus::NotInstalled),
            DaemonObservation::Installed(state) => {
                let autostart = self.autostart_state()?;
                Ok(DaemonStatus::installed(state, autostart))
            }
        }
    }

    fn observe_with_attribution(
        &self,
        attribution: LaunchdAttribution,
    ) -> Result<DaemonStatus, DaemonError> {
        let observation = self.observe_lifecycle_with_attribution(attribution)?;
        self.complete_observation(observation)
    }
}

impl<R> ServiceManager for LaunchdManager<R>
where R: CommandRunner
{
    fn name(&self) -> &'static str {
        LAUNCHD_MANAGER
    }

    fn definition_path(&self) -> &Path {
        &self.definition_path
    }

    fn start(&self, spec: &ServiceSpec) -> Result<DaemonStatus, DaemonError> {
        let stdout_log = path_text(&self.stdout_log)?;
        let stderr_log = path_text(&self.stderr_log)?;
        let definition = render_launchd_plist(spec, &stdout_log, &stderr_log)?;
        ensure_parent_directory(&self.stdout_log)?;
        ensure_parent_directory(&self.stderr_log)?;
        write_atomic(&self.definition_path, &definition)?;
        self.unload_if_loaded()?;
        self.set_autostart(AutostartState::Enabled)?;
        self.bootstrap()?;
        let observation = wait_for_running(self.poll_schedule, || {
            self.observe_lifecycle_with_attribution(LaunchdAttribution::Unfiltered)
        })?;
        self.complete_observation(observation)
    }

    fn stop(&self) -> Result<DaemonStatus, DaemonError> {
        self.unload_if_loaded()?;
        self.observe()
    }

    fn restart(&self) -> Result<DaemonStatus, DaemonError> {
        if let LaunchdRecord::Loaded(output) = self.service_record()? {
            let attribution = attribution_after_restart(&String::from_utf8_lossy(&output.stdout));
            let target = self.target();
            self.runner
                .run_checked(LAUNCHCTL, &["kickstart", "-k", &target])?;
            let observation = wait_for_running(self.poll_schedule, || {
                self.observe_lifecycle_with_attribution(attribution)
            })?;
            self.complete_observation(observation)
        } else if self.has_definition() {
            self.bootstrap_preserving_autostart()?;
            let observation = wait_for_running(self.poll_schedule, || {
                self.observe_lifecycle_with_attribution(LaunchdAttribution::Unfiltered)
            })?;
            self.complete_observation(observation)
        } else {
            Err(DaemonError::ServiceNotInstalled {
                path: self.definition_path.clone(),
            })
        }
    }

    fn observe(&self) -> Result<DaemonStatus, DaemonError> {
        self.observe_with_attribution(LaunchdAttribution::Unfiltered)
    }
}

#[cfg(test)]
mod tests {
    mod bootstrap;
    mod lifecycle;

    use rings_node::logging::LogLevel;

    use super::super::super::RuntimeFlavor;
    use super::super::tests::command_runner::CommandStep;
    use super::super::tests::command_runner::ScriptedCommandRunner;
    use super::super::tests::service_spec;
    use super::super::tests::TestRoot;
    use super::super::DaemonFailure;
    use super::super::DaemonState;
    use super::super::DaemonStatus;
    use super::super::TEST_OBSERVATION_SCHEDULE;
    use super::*;

    const TEST_DOMAIN: &str = "gui/501";

    fn test_target() -> String {
        launchd_target(TEST_DOMAIN)
    }

    fn test_root(name: &str) -> TestRoot {
        TestRoot::new("launchd", name)
    }

    fn install_test_definition(root: &Path) -> Result<PathBuf, DaemonError> {
        let definition = launchd_definition_path(root);
        write_atomic(&definition, "installed")?;
        Ok(definition)
    }

    fn test_manager(
        root: &Path,
        runner: ScriptedCommandRunner,
    ) -> LaunchdManager<ScriptedCommandRunner> {
        let domain = TEST_DOMAIN.to_owned();
        LaunchdManager {
            definition_path: launchd_definition_path(root),
            stdout_log: root.join(".rings/logs/daemon.log"),
            stderr_log: root.join(".rings/logs/daemon.error.log"),
            domain,
            poll_schedule: TEST_OBSERVATION_SCHEDULE,
            runner,
        }
    }

    fn enabled_autostart(domain: &str) -> CommandStep {
        let output = format!("\"{LAUNCHD_LABEL}\" => false\n");
        CommandStep::success(LAUNCHCTL, &["print-disabled", domain], &output)
    }

    fn disabled_autostart(domain: &str) -> CommandStep {
        let output = format!("\"{LAUNCHD_LABEL}\" => true\n");
        CommandStep::success(LAUNCHCTL, &["print-disabled", domain], &output)
    }

    #[test]
    fn start_rejects_invalid_definition_before_creating_directories() -> Result<(), DaemonError> {
        let root = test_root("start-invalid-definition");
        let manager = test_manager(
            &root,
            ScriptedCommandRunner::new(std::iter::empty::<CommandStep>()),
        );
        let mut spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
        spec.working_directory = "/tmp/rings\u{b}daemon".to_owned();

        let result = manager.start(&spec);

        assert!(matches!(
            result,
            Err(DaemonError::Launchd(LaunchdError::Definition(
                LaunchdDefinitionError::XmlIncompatibleValue { .. }
            )))
        ));
        assert!(!root.exists());
        Ok(())
    }

    #[test]
    fn start_waits_for_bootout_then_bootstraps_without_kickstart() -> Result<(), DaemonError> {
        let root = test_root("start-sequence");
        let domain = TEST_DOMAIN;
        let target = test_target();
        let definition = launchd_definition_path(&root);
        let definition_text = path_text(&definition)?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            CommandStep::success(LAUNCHCTL, &["bootout", &target], ""),
            CommandStep::failure(
                LAUNCHCTL,
                &["print", &target],
                LAUNCHD_SERVICE_NOT_FOUND,
                "Could not find specified service",
            ),
            CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
            CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            enabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

        let status = manager.start(&spec)?;

        assert!(manager.definition_path.is_file());
        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
        );
        Ok(())
    }

    #[test]
    fn restart_reads_disabled_autostart_once_and_suppresses_action_signal(
    ) -> Result<(), DaemonError> {
        let root = test_root("restart-sequence");
        let domain = TEST_DOMAIN;
        let target = test_target();
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = running\nruns = 3\n",
            ),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = throttled\nruns = 4\nlast terminating signal = Terminated: 15\n",
            ),
            disabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);
        install_test_definition(&root)?;

        let status = manager.restart()?;
        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Restarting(None), AutostartState::Disabled)
        );
        assert!(matches!(
            super::super::report_started(&manager, status),
            Err(DaemonError::ServiceDidNotStart { .. })
        ));
        Ok(())
    }

    #[test]
    fn restart_does_not_attribute_an_action_translated_exit_code() -> Result<(), DaemonError> {
        let root = test_root("restart-current-failure");
        let target = test_target();
        let mut steps = vec![
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = running\nruns = 4\n",
            ),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        ];
        steps.push(CommandStep::success(
            LAUNCHCTL,
            &["print", &target],
            "state = spawn scheduled\nruns = 5\nlast exit code = 1\n",
        ));
        steps.push(enabled_autostart(TEST_DOMAIN));
        let runner = ScriptedCommandRunner::new(steps);
        let manager = test_manager(&root, runner);
        install_test_definition(&root)?;

        let status = manager.restart()?;
        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                status: DaemonStatus::Installed {
                    state: DaemonState::Restarting(None),
                    autostart: AutostartState::Enabled,
                }
            })
        ));
        Ok(())
    }

    #[test]
    fn restart_reports_signal_crash_after_sequence_advances() -> Result<(), DaemonError> {
        let root = test_root("restart-signal-failure");
        let target = test_target();
        let mut steps = vec![
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = running\nruns = 3\n",
            ),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        ];
        steps.push(CommandStep::success(
            LAUNCHCTL,
            &["print", &target],
            "state = spawn scheduled\nruns = 4\nlast terminating signal = Segmentation fault: 11\n",
        ));
        steps.push(enabled_autostart(TEST_DOMAIN));
        let runner = ScriptedCommandRunner::new(steps);
        let manager = test_manager(&root, runner);
        install_test_definition(&root)?;

        let status = manager.restart()?;
        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                status: DaemonStatus::Installed {
                    state: DaemonState::Restarting(Some(DaemonFailure::Signal {
                        name: Some(name),
                        number: 11,
                        core_dumped: false,
                    })),
                    autostart: AutostartState::Enabled,
                }
            }) if name == "Segmentation fault"
        ));
        Ok(())
    }

    #[test]
    fn restart_rejects_an_unloaded_service_without_a_definition() {
        let root = test_root("restart-not-installed");
        let target = test_target();
        let runner = ScriptedCommandRunner::new([CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        )]);
        let manager = test_manager(&root, runner);

        let result = manager.restart();

        assert!(matches!(
            result,
            Err(DaemonError::ServiceNotInstalled { .. })
        ));
    }

    #[test]
    fn restart_without_a_sequence_baseline_uses_state_only() -> Result<(), DaemonError> {
        let root = test_root("restart-missing-runs");
        let target = test_target();
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nlast exit code = 9\n",
            ),
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            enabled_autostart(TEST_DOMAIN),
        ]);
        let manager = test_manager(&root, runner);
        install_test_definition(&root)?;

        let status = manager.restart()?;
        super::super::report_started(&manager, status)
    }

    #[test]
    fn restart_without_a_sequence_baseline_reports_crash_loop_as_starting(
    ) -> Result<(), DaemonError> {
        let root = test_root("restart-missing-runs-crash-loop");
        let target = test_target();
        let crash_loop = "state = spawn scheduled\nlast exit code = 1\n";
        let mut steps = vec![
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = waiting\n"),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        ];
        for _ in 0..=TEST_OBSERVATION_SCHEDULE.retries {
            steps.push(CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                crash_loop,
            ));
        }
        steps.push(enabled_autostart(TEST_DOMAIN));
        let manager = test_manager(&root, ScriptedCommandRunner::new(steps));
        install_test_definition(&root)?;

        let status = manager.restart()?;
        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                status: DaemonStatus::Installed {
                    state: DaemonState::Starting,
                    autostart: AutostartState::Enabled,
                }
            })
        ));
        Ok(())
    }

    #[test]
    fn start_reporting_observes_autostart_once_after_lifecycle_settles() -> Result<(), DaemonError>
    {
        let root = test_root("start-reporting");
        let domain = TEST_DOMAIN;
        let target = test_target();
        install_test_definition(&root)?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = waiting\n"),
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            enabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);

        let observation = super::super::wait_for_running(TEST_OBSERVATION_SCHEDULE, || {
            manager.observe_lifecycle_with_attribution(LaunchdAttribution::Unfiltered)
        })?;
        let status = manager.complete_observation(observation)?;
        super::super::report_started(&manager, status)
    }

    #[test]
    fn start_reporting_settles_immediately_for_a_throttled_retry() -> Result<(), DaemonError> {
        let root = test_root("start-reporting-failure");
        let domain = TEST_DOMAIN;
        let target = test_target();
        install_test_definition(&root)?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = throttled\nlast exit code = 78\n",
            ),
            enabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);

        let observation = super::super::wait_for_running(TEST_OBSERVATION_SCHEDULE, || {
            manager.observe_lifecycle_with_attribution(LaunchdAttribution::Unfiltered)
        })?;
        let status = manager.complete_observation(observation)?;
        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                status: DaemonStatus::Installed {
                    state: DaemonState::Restarting(Some(DaemonFailure::ExitCode(78))),
                    autostart: AutostartState::Enabled,
                }
            })
        ));
        Ok(())
    }

    #[test]
    fn start_reporting_settles_immediately_for_a_scheduled_retry() -> Result<(), DaemonError> {
        let root = test_root("start-reporting-signal-failure");
        let domain = TEST_DOMAIN;
        let target = test_target();
        install_test_definition(&root)?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nruns = 1\nlast terminating signal = Segmentation fault: 11\n",
            ),
            enabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);

        let observation = super::super::wait_for_running(TEST_OBSERVATION_SCHEDULE, || {
            manager.observe_lifecycle_with_attribution(LaunchdAttribution::Unfiltered)
        })?;
        let status = manager.complete_observation(observation)?;
        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                status: DaemonStatus::Installed {
                    state: DaemonState::Restarting(Some(DaemonFailure::Signal {
                        name: Some(name),
                        number: 11,
                        core_dumped: false,
                    })),
                    autostart: AutostartState::Enabled,
                }
            }) if name == "Segmentation fault"
        ));
        Ok(())
    }

    #[test]
    fn status_preserves_unexpected_launchctl_failures() {
        let root = test_root("status-failure");
        let target = test_target();
        let runner = ScriptedCommandRunner::new([CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            112,
            "Could not find specified domain",
        )]);
        let manager = test_manager(&root, runner);

        let result = manager.observe();

        let failure_detail = match result {
            Err(DaemonError::CommandFailed(failure)) => failure.detail,
            _ => None,
        };
        assert_eq!(
            failure_detail.as_deref(),
            Some("Could not find specified domain")
        );
    }

    #[test]
    fn status_reports_an_external_signal_crash() -> Result<(), DaemonError> {
        let root = test_root("status-signal-failure");
        let domain = TEST_DOMAIN;
        let target = test_target();
        install_test_definition(&root)?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nruns = 7\nlast terminating signal = Bus error: 10\n",
            ),
            enabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);

        let status = manager.observe()?;

        assert_eq!(
            status,
            DaemonStatus::installed(
                DaemonState::Restarting(Some(DaemonFailure::Signal {
                    name: Some("Bus error".to_owned()),
                    number: 10,
                    core_dumped: false,
                })),
                AutostartState::Enabled,
            )
        );
        Ok(())
    }

    #[test]
    fn status_maps_only_launchd_service_not_found_to_not_installed() -> Result<(), DaemonError> {
        let root = test_root("status-not-found");
        let target = test_target();
        let runner = ScriptedCommandRunner::new([CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        )]);
        let manager = test_manager(&root, runner);

        let status = manager.observe()?;

        assert_eq!(status, DaemonStatus::NotInstalled);
        Ok(())
    }

    #[test]
    fn loaded_job_without_a_plist_still_queries_manager_autostart() -> Result<(), DaemonError> {
        let root = test_root("status-loaded-without-plist");
        let domain = TEST_DOMAIN;
        let target = test_target();
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            enabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);

        let status = manager.observe()?;

        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
        );
        Ok(())
    }
}
