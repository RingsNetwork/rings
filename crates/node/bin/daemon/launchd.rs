//! Owns all launchd effects and funnels every process invocation through `CommandRunner`.
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
use super::DaemonStatus;
use super::ProcessCommandRunner;
use super::RecoveryFailure;
use super::ServiceManager;
use super::ServiceSpec;

mod status;

use status::attribution_after_restart;
use status::parse_launchd_autostart;
use status::parse_launchd_observation;
use status::LaunchdAttribution;
use status::LaunchdObservation;

const LAUNCHD_LABEL: &str = "io.ringsnetwork.node";
// macOS provides launchctl at this SIP-protected path. PATH lookup would add a hijack surface.
const LAUNCHCTL: &str = "/bin/launchctl";
const LAUNCHD_MANAGER: &str = "launchd";
// Observed on macOS 15.6.1 (24G90): `launchctl error 113` is service-not-found.
const LAUNCHD_SERVICE_NOT_FOUND: i32 = 113;
// Observed on macOS 15.6.1 (24G90): bootstrap returns 5 for a disabled unloaded label.
const LAUNCHD_BOOTSTRAP_DISABLED: i32 = 5;

#[derive(Debug, Error)]
pub(super) enum LaunchdError {
    #[error(transparent)]
    Definition(#[from] LaunchdDefinitionError),
    #[cfg(target_os = "macos")]
    #[error("could not read the current user id from `{output}")]
    InvalidUserId { output: String },
    #[error("launchd did not unload the daemon service within the observation budget")]
    ServiceDidNotUnload,
    #[error("could not bootstrap the disabled service while restoring its autostart setting")]
    DisabledBootstrap {
        #[source]
        failure: RecoveryFailure<Box<DaemonError>>,
    },
}

#[derive(Debug, Error)]
pub(super) enum LaunchdDefinitionError {
    // Debug formatting keeps the rejected control character escaped in diagnostics.
    #[error("value contains a character forbidden by XML 1.0 launchd plists: {value:?}")]
    XmlIncompatibleValue { value: String },
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
    fn service_output(&self) -> Result<Option<Output>, DaemonError> {
        let arguments = ["print", self.target.as_str()];
        let output = self.runner.run(LAUNCHCTL, &arguments)?;
        if output.status.success() {
            return Ok(Some(output));
        }
        if output.status.code() == Some(LAUNCHD_SERVICE_NOT_FOUND) {
            return Ok(None);
        }
        Err(command_failure(LAUNCHCTL, &arguments, output).into())
    }

    fn is_loaded(&self) -> Result<bool, DaemonError> {
        self.service_output().map(|output| output.is_some())
    }

    fn unload_if_loaded(&self) -> Result<(), DaemonError> {
        if !self.is_loaded()? {
            return Ok(());
        }
        self.runner
            .run_checked(LAUNCHCTL, &["bootout", &self.target])?;
        let still_loaded = poll_until(&mut || self.is_loaded(), |loaded| !*loaded)?;
        if still_loaded {
            Err(LaunchdError::ServiceDidNotUnload.into())
        } else {
            Ok(())
        }
    }

    fn bootstrap(&self) -> Result<(), DaemonError> {
        let definition = path_text(&self.definition_path)?;
        self.runner
            .run_checked(LAUNCHCTL, &["bootstrap", &self.domain, &definition])?;
        Ok(())
    }

    fn set_autostart(&self, enabled: bool) -> Result<(), DaemonError> {
        let action = if enabled { "enable" } else { "disable" };
        self.runner
            .run_checked(LAUNCHCTL, &[action, &self.target])?;
        Ok(())
    }

    fn bootstrap_preserving_autostart(&self) -> Result<(), DaemonError> {
        let initial = self.bootstrap();
        let Err(error) = initial else {
            return Ok(());
        };
        if !is_disabled_bootstrap_error(&error) {
            return Err(error);
        }
        // Exit 5 makes the macOS premise executable: mutate only after direct bootstrap proves the
        // label disabled, and restore disabled state after either outcome of the second bootstrap.
        self.set_autostart(true)?;
        let bootstrap = self.bootstrap().map_err(Box::new);
        let restore = self.set_autostart(false).map_err(Box::new);
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
        Ok(parse_launchd_autostart(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    fn observe_with_attribution(
        &self,
        attribution: LaunchdAttribution,
    ) -> Result<DaemonStatus, DaemonError> {
        let definition_present = self.has_definition();
        let output = self.service_output()?;
        let observation = parse_launchd_observation(
            output
                .as_ref()
                .map(|output| String::from_utf8_lossy(&output.stdout))
                .as_deref(),
            definition_present,
            attribution,
        );
        match observation {
            LaunchdObservation::NotInstalled => Ok(DaemonStatus::NotInstalled),
            LaunchdObservation::Installed(state) => {
                let autostart = self.autostart_state()?;
                Ok(DaemonStatus::installed(state, autostart))
            }
        }
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
        let definition = render_launchd_plist(spec, &self.stdout_log, &self.stderr_log)?;
        ensure_parent_directory(&self.stdout_log)?;
        ensure_parent_directory(&self.stderr_log)?;
        write_atomic(&self.definition_path, &definition)?;
        self.unload_if_loaded()?;
        self.set_autostart(true)?;
        self.bootstrap()?;
        wait_for_running(|| self.observe_with_attribution(LaunchdAttribution::FreshInstance))
    }

    fn stop(&self) -> Result<DaemonStatus, DaemonError> {
        self.unload_if_loaded()?;
        self.observe()
    }

    fn restart(&self) -> Result<DaemonStatus, DaemonError> {
        if let Some(output) = self.service_output()? {
            let attribution = attribution_after_restart(&String::from_utf8_lossy(&output.stdout));
            self.runner
                .run_checked(LAUNCHCTL, &["kickstart", "-k", &self.target])?;
            wait_for_running(|| self.observe_with_attribution(attribution))
        } else if self.has_definition() {
            self.bootstrap_preserving_autostart()?;
            wait_for_running(|| self.observe_with_attribution(LaunchdAttribution::FreshInstance))
        } else {
            Err(DaemonError::ServiceNotInstalled {
                path: self.definition_path.clone(),
            })
        }
    }

    fn observe(&self) -> Result<DaemonStatus, DaemonError> {
        self.observe_with_attribution(LaunchdAttribution::CurrentStatus)
    }
}

fn is_disabled_bootstrap_error(error: &DaemonError) -> bool {
    matches!(
        error,
        DaemonError::CommandFailed(failure)
            if failure.status.code() == Some(LAUNCHD_BOOTSTRAP_DISABLED)
    )
}

fn render_launchd_plist(
    spec: &ServiceSpec,
    stdout_log: &Path,
    stderr_log: &Path,
) -> Result<String, DaemonError> {
    let label = xml_string(LAUNCHD_LABEL)?;
    let arguments = spec
        .arguments()
        .into_iter()
        .map(|argument| {
            xml_string(argument).map(|argument| format!("    <string>{argument}</string>\n"))
        })
        .collect::<Result<String, LaunchdDefinitionError>>()?;
    let working_directory = xml_string(&spec.working_directory)?;
    let stdout_log = xml_string(&path_text(stdout_log)?)?;
    let stderr_log = xml_string(&path_text(stderr_log)?)?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
  <key>WorkingDirectory</key>
  <string>{}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
</dict>
</plist>
"#,
        working_directory, stdout_log, stderr_log,
    ))
}

fn xml_string(value: &str) -> Result<String, LaunchdDefinitionError> {
    if !value.chars().all(is_xml_1_0_character) {
        Err(LaunchdDefinitionError::XmlIncompatibleValue {
            value: value.to_owned(),
        })
    } else {
        Ok(value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;"))
    }
}

fn is_xml_1_0_character(character: char) -> bool {
    matches!(
        character,
        '\u{9}'
            | '\u{a}'
            | '\u{d}'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}'
    )
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
    use super::*;

    const TEST_DOMAIN: &str = "gui/501";
    const TEST_TARGET: &str = "gui/501/io.ringsnetwork.node";

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
            target: launchd_target(&domain),
            domain,
            runner,
        }
    }

    fn enabled_autostart(domain: &str) -> CommandStep {
        CommandStep::success(
            LAUNCHCTL,
            &["print-disabled", domain],
            "\"io.ringsnetwork.node\" => false\n",
        )
    }

    fn disabled_autostart(domain: &str) -> CommandStep {
        CommandStep::success(
            LAUNCHCTL,
            &["print-disabled", domain],
            "\"io.ringsnetwork.node\" => true\n",
        )
    }

    fn repeated_enabled_observations(domain: &str, target: &str, output: &str) -> Vec<CommandStep> {
        let mut steps = Vec::new();
        for _ in 0..=super::super::OBSERVATION_RETRIES {
            steps.push(CommandStep::success(LAUNCHCTL, &["print", target], output));
            steps.push(enabled_autostart(domain));
        }
        steps
    }

    #[test]
    fn definition_preserves_arguments_working_directory_and_xml() -> Result<(), DaemonError> {
        let spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
        let plist = render_launchd_plist(
            &spec,
            Path::new("/Users/test user/.rings/logs/daemon.log"),
            Path::new("/Users/test user/.rings/logs/daemon.error.log"),
        )?;

        assert!(plist.contains("<string>/Users/test user/bin/rings</string>"));
        assert!(plist.contains("<string>/Users/test user/.rings/config&amp;prod.yaml</string>"));
        assert!(plist.contains("<key>WorkingDirectory</key>"));
        assert!(plist.contains("<string>/Users/test user/work</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        Ok(())
    }

    #[test]
    fn definition_rejects_values_forbidden_by_xml_1_0() -> Result<(), DaemonError> {
        for character in [
            '\u{0}', '\u{1}', '\u{8}', '\u{b}', '\u{c}', '\u{e}', '\u{1f}', '\u{fffe}', '\u{ffff}',
        ] {
            let mut spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
            spec.working_directory = format!("/tmp/rings{character}daemon");
            assert!(matches!(
                render_launchd_plist(
                    &spec,
                    Path::new("/tmp/rings-daemon.log"),
                    Path::new("/tmp/rings-daemon.error.log"),
                ),
                Err(DaemonError::Launchd(LaunchdError::Definition(
                    LaunchdDefinitionError::XmlIncompatibleValue { .. }
                )))
            ));
        }
        Ok(())
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
    fn definition_accepts_xml_1_0_path_boundaries() -> Result<(), DaemonError> {
        for character in [
            '\u{9}',
            '\u{a}',
            '\u{d}',
            '\u{20}',
            '\u{d7ff}',
            '\u{e000}',
            '\u{fffd}',
            '\u{10000}',
            '\u{10ffff}',
        ] {
            let mut spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
            spec.working_directory = format!("/tmp/rings{character}daemon");
            assert!(render_launchd_plist(
                &spec,
                Path::new("/tmp/rings-daemon.log"),
                Path::new("/tmp/rings-daemon.error.log"),
            )
            .is_ok());
        }
        Ok(())
    }

    #[test]
    fn start_waits_for_bootout_then_bootstraps_without_kickstart() -> Result<(), DaemonError> {
        let root = test_root("start-sequence");
        let domain = TEST_DOMAIN;
        let target = TEST_TARGET.to_owned();
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
    fn restart_preserves_disabled_autostart_and_ignores_action_signal() -> Result<(), DaemonError> {
        let root = test_root("restart-sequence");
        let domain = TEST_DOMAIN;
        let target = TEST_TARGET.to_owned();
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
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = running\nruns = 5\nlast terminating signal = Terminated: 15\n",
            ),
            disabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);
        install_test_definition(&root)?;

        let status = manager.restart()?;
        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Running, AutostartState::Disabled)
        );
        super::super::report_started(&manager, status)
    }

    #[test]
    fn restart_does_not_attribute_an_action_translated_exit_code() -> Result<(), DaemonError> {
        let root = test_root("restart-current-failure");
        let target = TEST_TARGET.to_owned();
        let mut steps = vec![
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = running\nruns = 4\n",
            ),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        ];
        for _ in 0..=super::super::OBSERVATION_RETRIES {
            steps.push(CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nruns = 5\nlast exit code = 1\n",
            ));
            steps.push(enabled_autostart(TEST_DOMAIN));
        }
        let runner = ScriptedCommandRunner::new(steps);
        let manager = test_manager(&root, runner);
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
    fn restart_reports_signal_crash_after_sequence_advances() -> Result<(), DaemonError> {
        let root = test_root("restart-signal-failure");
        let target = TEST_TARGET.to_owned();
        let mut steps = vec![
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = running\nruns = 3\n",
            ),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        ];
        for _ in 0..=super::super::OBSERVATION_RETRIES {
            steps.push(CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nruns = 4\nlast terminating signal = Segmentation fault: 11\n",
            ));
            steps.push(enabled_autostart(TEST_DOMAIN));
        }
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
        let target = TEST_TARGET.to_owned();
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
        let target = TEST_TARGET.to_owned();
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nlast exit code = 9\n",
            ),
            enabled_autostart(TEST_DOMAIN),
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
        let target = TEST_TARGET.to_owned();
        let crash_loop = "state = spawn scheduled\nlast exit code = 1\n";
        let mut steps = vec![
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = waiting\n"),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        ];
        for _ in 0..=super::super::OBSERVATION_RETRIES {
            steps.push(CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                crash_loop,
            ));
            steps.push(enabled_autostart(TEST_DOMAIN));
        }
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
    fn start_reporting_observes_autostart_with_each_state() -> Result<(), DaemonError> {
        let root = test_root("start-reporting");
        let domain = TEST_DOMAIN;
        let target = TEST_TARGET.to_owned();
        install_test_definition(&root)?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = waiting\n"),
            enabled_autostart(domain),
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            enabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);

        let status = super::super::wait_for_running(|| {
            manager.observe_with_attribution(LaunchdAttribution::FreshInstance)
        })?;
        super::super::report_started(&manager, status)
    }

    #[test]
    fn start_reporting_waits_the_budget_for_a_throttled_retry() -> Result<(), DaemonError> {
        let root = test_root("start-reporting-failure");
        let domain = TEST_DOMAIN;
        let target = TEST_TARGET.to_owned();
        install_test_definition(&root)?;
        let runner = ScriptedCommandRunner::new(repeated_enabled_observations(
            domain,
            &target,
            "state = throttled\nlast exit code = 78\n",
        ));
        let manager = test_manager(&root, runner);

        let status = super::super::wait_for_running(|| {
            manager.observe_with_attribution(LaunchdAttribution::FreshInstance)
        })?;
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
    fn start_reporting_waits_the_budget_for_a_scheduled_retry() -> Result<(), DaemonError> {
        let root = test_root("start-reporting-signal-failure");
        let domain = TEST_DOMAIN;
        let target = TEST_TARGET.to_owned();
        install_test_definition(&root)?;
        let runner = ScriptedCommandRunner::new(repeated_enabled_observations(
            domain,
            &target,
            "state = spawn scheduled\nruns = 1\nlast terminating signal = Segmentation fault: 11\n",
        ));
        let manager = test_manager(&root, runner);

        let status = super::super::wait_for_running(|| {
            manager.observe_with_attribution(LaunchdAttribution::FreshInstance)
        })?;
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
        let target = TEST_TARGET.to_owned();
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
        let target = TEST_TARGET.to_owned();
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
        let target = TEST_TARGET.to_owned();
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
}
