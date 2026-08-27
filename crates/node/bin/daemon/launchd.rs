//! Confines launchd subprocess and filesystem effects to one adapter boundary.
#![cfg(any(target_os = "macos", all(test, unix)))]

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;

use thiserror::Error;

use super::command_failure;
use super::ensure_parent_directory;
use super::path_text;
use super::poll_until;
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
use status::MissingRecordEvidence;

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
    #[error("launchd bootstrap failed; probing disabled-label state also failed: {probe}")]
    BootstrapStateProbe {
        #[source]
        bootstrap: Box<DaemonError>,
        probe: Box<DaemonError>,
    },
    #[error("launchd bootstrap failed, but disabled-label recovery is not applicable because login autostart is {observed}")]
    BootstrapStateMismatch {
        observed: AutostartState,
        #[source]
        bootstrap: Box<DaemonError>,
    },
    #[error(
        "launchd bootstrap failed; temporarily enabling the disabled label also failed: {enable}"
    )]
    BootstrapEnable {
        #[source]
        bootstrap: Box<DaemonError>,
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
    #[error(
        "the bootstrap retry failed and disabled-autostart restoration also failed ({restore}); login autostart may remain enabled"
    )]
    BootstrapRetryAndRestore {
        #[source]
        retry: Box<DaemonError>,
        restore: Box<DaemonError>,
    },
}

/// A concrete launchd disabled-label override.
///
/// Law: `Enabled` denotes the same manager fact as `AutostartState::Enabled`; `Disabled` denotes
/// the same manager fact as `AutostartState::Disabled`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConcreteAutostart {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnloadOutcome {
    AlreadyAbsent,
    Unloaded,
}

impl_daemon_error_from_adapter!(LaunchdDefinitionError => LaunchdError);

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

    /// Uses the command observation schedule to bound confirmation that `bootout` removed the
    /// manager record. Exhaustion is terminal: stop cannot claim success, and start cannot safely
    /// bootstrap a label that launchd may still consider loaded.
    fn unload_if_loaded(&self) -> Result<UnloadOutcome, DaemonError> {
        let initial = self.service_record()?;
        if matches!(initial, LaunchdRecord::Missing) {
            return Ok(UnloadOutcome::AlreadyAbsent);
        }
        self.unload_loaded()
    }

    /// Pre: the manager has just reported this label as loaded.
    ///
    /// Post: success proves that launchd no longer retains the loaded job definition.
    fn unload_loaded(&self) -> Result<UnloadOutcome, DaemonError> {
        self.runner
            .run_checked(LAUNCHCTL, &["bootout", self.target()])?;
        let terminal_record = poll_until(
            self.poll_schedule,
            || self.service_record(),
            |record| matches!(record, LaunchdRecord::Missing),
        )?;
        if matches!(terminal_record, LaunchdRecord::Loaded(_)) {
            Err(LaunchdError::ServiceDidNotUnload.into())
        } else {
            Ok(UnloadOutcome::Unloaded)
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

    /// Invariant: once temporary enablement succeeds, every bootstrap retry outcome attempts one
    /// disable. Only a failed disable can leave the previously observed disabled override changed.
    ///
    /// Post: direct bootstrap success changes no override. Retry success restores an observed
    /// disabled override after temporarily enabling it.
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
        // Observed on macOS 15.6.1 (24G90): disabling a live job does not unload or stop it. The
        // Disabled state was corroborated above, so restore that prior value after either result.
        let restore = self
            .set_autostart(ConcreteAutostart::Disabled)
            .map_err(Box::new);
        match (bootstrap, restore) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(source), Ok(())) => Err(LaunchdError::BootstrapRetry { source }.into()),
            (Ok(()), Err(source)) => Err(LaunchdError::AutostartRestore { source }.into()),
            (Err(retry), Err(restore)) => {
                Err(LaunchdError::BootstrapRetryAndRestore { retry, restore }.into())
            }
        }
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

    fn missing_record_evidence(&self) -> MissingRecordEvidence {
        // An unloaded launchd job has no manager record, so the plist is the only local
        // installation evidence available to passive status observation.
        if self.has_definition() {
            MissingRecordEvidence::DefinitionPresent
        } else {
            MissingRecordEvidence::None
        }
    }

    fn observe_lifecycle_with_attribution(
        &self,
        attribution: LaunchdAttribution,
    ) -> Result<DaemonObservation, DaemonError> {
        let record = self.service_record()?;
        Ok(self.lifecycle_from_record(&record, attribution))
    }

    fn lifecycle_from_record(
        &self,
        record: &LaunchdRecord<Output>,
        attribution: LaunchdAttribution,
    ) -> DaemonObservation {
        let text_record = record
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stdout));
        parse_launchd_observation(text_record, self.missing_record_evidence(), attribution)
    }

    fn complete_observation(
        &self,
        observation: DaemonObservation,
    ) -> Result<DaemonStatus, DaemonError> {
        let complete = match observation {
            DaemonObservation::NotInstalled => DaemonObservation::NotInstalled,
            DaemonObservation::Installed { state, .. } => {
                let autostart = self.autostart_state()?;
                DaemonObservation::installed(state, autostart)
            }
        };
        Ok(complete.into_status())
    }

    fn observe_with_attribution(
        &self,
        attribution: LaunchdAttribution,
    ) -> Result<DaemonStatus, DaemonError> {
        let observation = self.observe_lifecycle_with_attribution(attribution)?;
        self.complete_observation(observation)
    }

    fn settle(&self, attribution: LaunchdAttribution) -> Result<DaemonStatus, DaemonError> {
        let observation = wait_for_start_settlement(self.poll_schedule, || {
            self.observe_lifecycle_with_attribution(attribution)
        })?;
        self.complete_observation(observation)
    }

    fn stopped_status(&self, installed: bool) -> Result<DaemonStatus, DaemonError> {
        if !installed {
            return Ok(DaemonStatus::NotInstalled);
        }
        Ok(DaemonStatus::installed(
            DaemonState::Stopped,
            self.autostart_state()?,
        ))
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

    fn install(&self, spec: &ServiceSpec) -> Result<DaemonStatus, DaemonError> {
        let stdout_log = path_text(&self.stdout_log)?;
        let stderr_log = path_text(&self.stderr_log)?;
        let definition = render_launchd_plist(spec, &stdout_log, &stderr_log)?;
        // Observed on macOS 15.6.1 (24G90): launchd will not spawn a job whose StandardOutPath or
        // StandardErrorPath parent is absent.
        ensure_parent_directory(&self.stdout_log)?;
        ensure_parent_directory(&self.stderr_log)?;
        write_atomic(&self.definition_path, &definition)?;
        self.set_autostart(ConcreteAutostart::Enabled)?;
        self.observe()
    }

    fn uninstall(&self) -> Result<DaemonStatus, DaemonError> {
        self.unload_if_loaded()?;
        remove_service_definition(&self.definition_path)?;
        Ok(DaemonStatus::NotInstalled)
    }

    fn start(&self) -> Result<DaemonStatus, DaemonError> {
        let record = self.service_record()?;
        match &record {
            LaunchdRecord::Missing if self.has_definition() => {
                self.bootstrap_preserving_autostart()?;
                self.settle(LaunchdAttribution::Unfiltered)
            }
            LaunchdRecord::Missing => Err(DaemonError::ServiceNotInstalled {
                path: self.definition_path.clone(),
            }),
            LaunchdRecord::Loaded(_) => {
                let observation =
                    self.lifecycle_from_record(&record, LaunchdAttribution::Unfiltered);
                if observation.is_running() {
                    return self.complete_observation(observation);
                }
                if self.has_definition() {
                    // launchd retains the definition loaded before a later `install`. Reload the
                    // plist before starting a stopped job so the installed definition takes effect.
                    self.unload_loaded()?;
                    self.bootstrap_preserving_autostart()?;
                    return self.settle(LaunchdAttribution::Unfiltered);
                }
                self.runner
                    .run_checked(LAUNCHCTL, &["kickstart", self.target()])?;
                self.settle(LaunchdAttribution::Unfiltered)
            }
        }
    }

    fn stop(&self) -> Result<DaemonStatus, DaemonError> {
        match self.unload_if_loaded()? {
            UnloadOutcome::AlreadyAbsent => self.stopped_status(self.has_definition()),
            // Post: a successful bootout proves that stop acted on a loaded job even when no plist
            // remains after the action.
            UnloadOutcome::Unloaded => self.stopped_status(true),
        }
    }

    fn restart(&self) -> Result<DaemonStatus, DaemonError> {
        if let LaunchdRecord::Loaded(output) = self.service_record()? {
            if self.has_definition() {
                // launchd caches the loaded job definition. Bootout plus bootstrap is the reload
                // boundary that makes a definition replaced by `install` effective.
                self.unload_loaded()?;
                self.bootstrap_preserving_autostart()?;
                return self.settle(LaunchdAttribution::Unfiltered);
            }
            // Pre: sample the run counter strictly before issuing kickstart;
            // attribution_after_restart uses that baseline to exclude termination history caused
            // by this action.
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
    //! Ensures scripted loaded records include the rendered unsuccessful-exit policy.

    mod bootstrap;
    mod lifecycle;
    mod manager;

    use rings_node::logging::LogLevel;

    use super::super::super::RuntimeFlavor;
    use super::super::tests::command_runner::CommandStep;
    use super::super::tests::command_runner::ScriptedCommandRunner;
    use super::super::tests::fill_poll_budget;
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

    fn scripted_manager(
        root: &Path,
        steps: impl IntoIterator<Item = CommandStep>,
    ) -> LaunchdManager<ScriptedCommandRunner> {
        test_manager(root, ScriptedCommandRunner::new(steps))
    }

    fn loaded_service(target: &str, properties: &str) -> CommandStep {
        let output = format!("semaphores = {{\n    successful exit => 0\n}}\n{properties}");
        CommandStep::success(LAUNCHCTL, &["print", target], &output)
    }

    fn loaded_service_without_respawn_policy(target: &str, properties: &str) -> CommandStep {
        CommandStep::success(LAUNCHCTL, &["print", target], properties)
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
}
