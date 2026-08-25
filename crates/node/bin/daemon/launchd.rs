//! Executes launchd user-domain commands while rendering and lifecycle reduction remain in
//! effect-free sibling modules. Stop reuses the observation that proved the job was unloaded.
#![cfg(any(target_os = "macos", all(test, unix)))]

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;

use thiserror::Error;

use super::command_failure;
use super::ensure_parent_directory;
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
#[cfg(test)]
pub(super) use status::OBSERVED_THROTTLE_FLOOR;

// Verified on macOS 15.6.1 (24G90): launchctl is provided at /bin on the SIP-protected signed
// system volume. Using the fixed path avoids substituting a different manager through PATH.
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
    #[error("could not determine whether launchd exit 5 came from a disabled label after bootstrap failed: {bootstrap}")]
    BootstrapStateProbe {
        bootstrap: Box<DaemonError>,
        #[source]
        probe: Box<DaemonError>,
    },
    #[error("launchd bootstrap failed, but disabled-label recovery is not applicable because login autostart is {observed}")]
    BootstrapStateMismatch {
        observed: AutostartState,
        #[source]
        bootstrap: Box<DaemonError>,
    },
    #[error(
        "could not temporarily enable a disabled launchd label after bootstrap failed: {bootstrap}"
    )]
    BootstrapEnable {
        bootstrap: Box<DaemonError>,
        #[source]
        enable: Box<DaemonError>,
    },
    #[error("could not bootstrap the disabled launchd service after temporarily enabling it")]
    BootstrapRetry {
        #[source]
        source: Box<DaemonError>,
    },
    #[error("the service was bootstrapped, but restoring disabled login autostart failed; the running service may remain enabled at login")]
    AutostartRestore {
        #[source]
        source: Box<DaemonError>,
    },
    #[error("the bootstrap retry and disabled-autostart restore both failed; login autostart may remain enabled")]
    BootstrapRetryAndRestore {
        #[source]
        failure: RecoveryFailure<Box<DaemonError>>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConcreteAutostart {
    Enabled,
    Disabled,
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
    target: String,
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
        let target = launchd_target(&domain);
        let logs = home.join(".rings").join("logs");
        Ok(Self {
            definition_path: launchd_definition_path(&home),
            stdout_log: logs.join("daemon.log"),
            stderr_log: logs.join("daemon.error.log"),
            domain,
            target,
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
    fn target(&self) -> &str {
        &self.target
    }

    fn service_record(&self) -> Result<LaunchdRecord<Output>, DaemonError> {
        let arguments = ["print", self.target()];
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

    fn unloaded_observation(&self) -> DaemonObservation {
        if self.has_definition() {
            DaemonObservation::installed(
                super::DaemonState::Stopped,
                super::StartPollDisposition::Settled,
                (),
            )
        } else {
            DaemonObservation::NotInstalled
        }
    }

    fn unload_if_loaded(&self) -> Result<DaemonObservation, DaemonError> {
        if !self.is_loaded()? {
            return Ok(self.unloaded_observation());
        }
        self.runner
            .run_checked(LAUNCHCTL, &["bootout", self.target()])?;
        let still_loaded = poll_until(self.poll_schedule, || self.is_loaded(), |loaded| !*loaded)?;
        if still_loaded {
            Err(LaunchdError::ServiceDidNotUnload.into())
        } else {
            Ok(self.unloaded_observation())
        }
    }

    fn bootstrap(&self) -> Result<(), DaemonError> {
        let definition = path_text(&self.definition_path)?;
        self.runner
            .run_checked(LAUNCHCTL, &[
                "bootstrap",
                self.domain.as_str(),
                definition.as_str(),
            ])
            .map(|_| ())
    }

    fn set_autostart(&self, state: ConcreteAutostart) -> Result<(), DaemonError> {
        let action = match state {
            ConcreteAutostart::Enabled => "enable",
            ConcreteAutostart::Disabled => "disable",
        };
        self.runner
            .run_checked(LAUNCHCTL, &[action, self.target()])?;
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
        let observed = match self.autostart_state() {
            Ok(state) => state,
            Err(probe) => {
                return Err(LaunchdError::BootstrapStateProbe {
                    bootstrap: Box::new(error),
                    probe: Box::new(probe),
                }
                .into());
            }
        };
        let AutostartState::Disabled = observed else {
            return Err(LaunchdError::BootstrapStateMismatch {
                observed,
                bootstrap: Box::new(error),
            }
            .into());
        };
        if let Err(enable) = self.set_autostart(ConcreteAutostart::Enabled) {
            return Err(LaunchdError::BootstrapEnable {
                bootstrap: Box::new(error),
                enable: Box::new(enable),
            }
            .into());
        }
        let bootstrap = self.bootstrap().map_err(Box::new);
        // Observed on macOS 15.6.1 (24G90): disabling a live job does not unload or stop it. Restore
        // Disabled was corroborated above, so restore that prior value after either retry result.
        let restore = self
            .set_autostart(ConcreteAutostart::Disabled)
            .map_err(Box::new);
        match (bootstrap, restore) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(source), Ok(())) => Err(LaunchdError::BootstrapRetry { source }.into()),
            (Ok(()), Err(source)) => Err(LaunchdError::AutostartRestore { source }.into()),
            (Err(primary), Err(recovery)) => Err(LaunchdError::BootstrapRetryAndRestore {
                failure: RecoveryFailure::Both { primary, recovery },
            }
            .into()),
        }
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
        let borrowed = match &record {
            LaunchdRecord::Missing => LaunchdRecord::Missing,
            LaunchdRecord::Loaded(output) => {
                LaunchdRecord::Loaded(String::from_utf8_lossy(&output.stdout))
            }
        };
        Ok(parse_launchd_observation(
            borrowed,
            definition_present,
            attribution,
        ))
    }

    fn complete_observation(
        &self,
        observation: DaemonObservation,
    ) -> Result<DaemonStatus, DaemonError> {
        match observation {
            DaemonObservation::NotInstalled => Ok(DaemonStatus::NotInstalled),
            DaemonObservation::Installed { state, .. } => {
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

    fn settle(&self, attribution: LaunchdAttribution) -> Result<DaemonStatus, DaemonError> {
        let observation = wait_for_running(self.poll_schedule, || {
            self.observe_lifecycle_with_attribution(attribution)
        })?;
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
        // Observed on macOS 15.6.1 (24G90): bootstrap rejects an already-loaded label and a
        // disabled unloaded label. Therefore start unloads first and explicitly enables before
        // bootstrap. The log parents are created before bootstrap because launchd will not spawn a
        // job whose StandardOutPath or StandardErrorPath parent is absent.
        self.set_autostart(ConcreteAutostart::Enabled)?;
        self.bootstrap()?;
        self.settle(LaunchdAttribution::Unfiltered)
    }

    fn stop(&self) -> Result<DaemonStatus, DaemonError> {
        let observation = self.unload_if_loaded()?;
        self.complete_observation(observation)
    }

    fn restart(&self) -> Result<DaemonStatus, DaemonError> {
        if let LaunchdRecord::Loaded(output) = self.service_record()? {
            let attribution = attribution_after_restart(&String::from_utf8_lossy(&output.stdout));
            self.runner
                .run_checked(LAUNCHCTL, &["kickstart", "-k", self.target()])?;
            self.settle(attribution)
        } else if self.has_definition() {
            self.bootstrap_preserving_autostart()?;
            self.settle(LaunchdAttribution::Unfiltered)
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
    //! Scripts launchd command sequences and verifies manager mutations preserve autostart state.

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
        let target = launchd_target(&domain);
        LaunchdManager {
            definition_path: launchd_definition_path(root),
            stdout_log: root.join(".rings/logs/daemon.log"),
            stderr_log: root.join(".rings/logs/daemon.error.log"),
            domain,
            target,
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

    fn missing_service(target: &str) -> CommandStep {
        CommandStep::failure(
            LAUNCHCTL,
            &["print", target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        )
    }

    fn disabled_bootstrap(domain: &str, definition: &str) -> CommandStep {
        CommandStep::failure(
            LAUNCHCTL,
            &["bootstrap", domain, definition],
            LAUNCHD_BOOTSTRAP_DISABLED,
            "Input/output error",
        )
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
            missing_service(&target),
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
    fn healthy_restart_waits_past_an_action_translated_exit_code() -> Result<(), DaemonError> {
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
        steps.push(CommandStep::success(
            LAUNCHCTL,
            &["print", &target],
            "state = running\nruns = 5\n",
        ));
        steps.push(enabled_autostart(TEST_DOMAIN));
        let runner = ScriptedCommandRunner::new(steps);
        let manager = test_manager(&root, runner);
        install_test_definition(&root)?;

        let status = manager.restart()?;
        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
        );
        super::super::report_started(&manager, status)
    }

    #[test]
    fn healthy_restart_waits_past_an_exited_action_record() -> Result<(), DaemonError> {
        let root = test_root("restart-exited-action-record");
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
                "state = exited\nruns = 4\nlast exit code = 1\n",
            ),
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = running\nruns = 4\n",
            ),
            enabled_autostart(TEST_DOMAIN),
        ]);
        let manager = test_manager(&root, runner);
        install_test_definition(&root)?;

        let status = manager.restart()?;

        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
        );
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
        steps.push(CommandStep::success(
            LAUNCHCTL,
            &["print", &target],
            "state = throttled\nruns = 4\nlast terminating signal = Segmentation fault: 11\n",
        ));
        steps.push(enabled_autostart(TEST_DOMAIN));
        let runner = ScriptedCommandRunner::new(steps);
        let manager = test_manager(&root, runner);
        install_test_definition(&root)?;

        let status = manager.restart()?;
        let error = super::super::report_started(&manager, status);
        let expected = DaemonStatus::installed(
            DaemonState::Restarting(Some(DaemonFailure::described(
                "signal Segmentation fault: 11",
            ))),
            AutostartState::Enabled,
        );

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart { status }) if status == expected
        ));
        Ok(())
    }

    #[test]
    fn restart_rejects_an_unloaded_service_without_a_definition() {
        let root = test_root("restart-not-installed");
        let target = test_target();
        let runner = ScriptedCommandRunner::new([missing_service(&target)]);
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
                    state: DaemonState::Transitioning(super::super::DaemonTransition::Starting),
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
        let definition = launchd_definition_path(&root);
        let definition_text = path_text(&definition)?;
        let runner = ScriptedCommandRunner::new([
            missing_service(&target),
            CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
            CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = waiting\n"),
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            enabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

        let status = manager.start(&spec)?;
        super::super::report_started(&manager, status)
    }

    #[test]
    fn start_reporting_settles_immediately_for_a_throttled_retry() -> Result<(), DaemonError> {
        let root = test_root("start-reporting-failure");
        let domain = TEST_DOMAIN;
        let target = test_target();
        let definition = launchd_definition_path(&root);
        let definition_text = path_text(&definition)?;
        let runner = ScriptedCommandRunner::new([
            missing_service(&target),
            CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
            CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = throttled\nlast exit code = 78\n",
            ),
            enabled_autostart(domain),
        ]);
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
    fn start_waits_past_a_scheduled_retry_until_running() -> Result<(), DaemonError> {
        let root = test_root("start-reporting-signal-failure");
        let domain = TEST_DOMAIN;
        let target = test_target();
        let definition = launchd_definition_path(&root);
        let definition_text = path_text(&definition)?;
        let runner = ScriptedCommandRunner::new([
            missing_service(&target),
            CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
            CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nruns = 1\nlast terminating signal = Segmentation fault: 11\n",
            ),
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\nruns = 1\n"),
            enabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

        let status = manager.start(&spec)?;
        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
        );
        super::super::report_started(&manager, status)
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
                DaemonState::Restarting(Some(DaemonFailure::described("signal Bus error: 10"))),
                AutostartState::Enabled,
            )
        );
        Ok(())
    }

    #[test]
    fn status_maps_only_launchd_service_not_found_to_not_installed() -> Result<(), DaemonError> {
        let root = test_root("status-not-found");
        let target = test_target();
        let runner = ScriptedCommandRunner::new([missing_service(&target)]);
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
