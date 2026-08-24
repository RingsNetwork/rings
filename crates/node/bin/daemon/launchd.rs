//! launchd adapter for the macOS per-user daemon.
#![cfg(any(target_os = "macos", all(test, unix)))]

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::thread;

use thiserror::Error;

use super::command_failure;
use super::ensure_parent_directory;
use super::path_text;
use super::wait_for_running;
use super::write_atomic;
use super::AutostartState;
use super::CommandRunner;
use super::DaemonError;
use super::DaemonStatus;
use super::ProcessCommandRunner;
use super::ServiceManager;
use super::ServiceSpec;

mod status;

use status::attribution_after_restart;
use status::parse_launchd_autostart;
use status::parse_launchd_observation;
use status::LaunchdAttribution;

const LAUNCHD_LABEL: &str = "io.ringsnetwork.node";
// macOS provides launchctl at this SIP-protected path. PATH lookup would add a hijack surface.
const LAUNCHCTL: &str = "/bin/launchctl";
const LAUNCHD_MANAGER: &str = "launchd";
const UNLOAD_OBSERVATION_ATTEMPTS: usize = 20;
const UNLOAD_OBSERVATION_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
// Observed on macOS 15.6.1 (24G90): `launchctl error 113` is service-not-found.
const LAUNCHD_SERVICE_NOT_FOUND: i32 = 113;

#[derive(Debug, Error)]
pub(super) enum LaunchdDefinitionError {
    // Debug formatting keeps the rejected control character escaped in diagnostics.
    #[error("value contains a character forbidden by XML 1.0 launchd plists: {value:?}")]
    XmlIncompatibleValue { value: String },
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
            return Err(DaemonError::InvalidUserId { output: user_id });
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
        for _ in 0..UNLOAD_OBSERVATION_ATTEMPTS {
            if !self.is_loaded()? {
                return Ok(());
            }
            thread::sleep(UNLOAD_OBSERVATION_INTERVAL);
        }
        Err(DaemonError::ServiceDidNotUnload)
    }

    fn bootstrap(&self, enable_autostart: bool) -> Result<(), DaemonError> {
        let definition = path_text(&self.definition_path)?;
        if enable_autostart {
            self.set_autostart(true)?;
        }
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
        if self.autostart_state()? != AutostartState::Disabled {
            return self.bootstrap(false);
        }
        // macOS 15.6.1 rejects bootstrap with exit 5 while a label is disabled. Temporarily enable
        // the unloaded job, load it explicitly, then restore the independent autostart setting.
        self.set_autostart(true)?;
        let bootstrap = self.bootstrap(false);
        let restore = self.set_autostart(false);
        finish_disabled_bootstrap(bootstrap, restore)
    }

    fn has_definition(&self) -> bool {
        // An unloaded launchd job has no manager record, so the plist is the installation evidence.
        self.definition_path.is_file()
    }

    fn autostart_state(&self) -> Result<AutostartState, DaemonError> {
        if !self.has_definition() {
            return Ok(AutostartState::Disabled);
        }
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
        let state = parse_launchd_observation(
            output
                .as_ref()
                .map(|output| String::from_utf8_lossy(&output.stdout))
                .as_deref(),
            definition_present,
            attribution,
        );
        let autostart = self.autostart_state()?;
        Ok(DaemonStatus { state, autostart })
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
        self.bootstrap(true)?;
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

fn finish_disabled_bootstrap(
    bootstrap: Result<(), DaemonError>,
    restore: Result<(), DaemonError>,
) -> Result<(), DaemonError> {
    match (bootstrap, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(bootstrap), Ok(())) => Err(bootstrap),
        (Ok(()), Err(source)) => Err(DaemonError::RestoreAutostart {
            source: Box::new(source),
        }),
        (Err(bootstrap), Err(restore)) => Err(DaemonError::BootstrapAndRestoreAutostart {
            bootstrap: Box::new(bootstrap),
            restore: Box::new(restore),
        }),
    }
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

    fn test_root(name: &str) -> TestRoot {
        TestRoot::new("launchd", name)
    }

    fn test_manager(
        root: &Path,
        runner: ScriptedCommandRunner,
    ) -> LaunchdManager<ScriptedCommandRunner> {
        let domain = "gui/501".to_owned();
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
                Err(DaemonError::LaunchdDefinition(
                    LaunchdDefinitionError::XmlIncompatibleValue { .. }
                ))
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
            Err(DaemonError::LaunchdDefinition(
                LaunchdDefinitionError::XmlIncompatibleValue { .. }
            ))
        ));
        assert!(!root.exists());
        manager.runner.assert_exhausted();
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
        let domain = "gui/501";
        let target = launchd_target(domain);
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
        assert_eq!(status.state, DaemonState::Running);
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn restart_preserves_disabled_autostart_and_ignores_action_signal() -> Result<(), DaemonError> {
        let root = test_root("restart-sequence");
        let domain = "gui/501";
        let target = launchd_target(domain);
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
        write_atomic(&manager.definition_path, "installed")?;

        let status = manager.restart()?;
        assert_eq!(status.autostart, AutostartState::Disabled);
        let result = super::super::report_started(&manager, status);

        manager.runner.assert_exhausted();
        result
    }

    #[test]
    fn restart_reports_failure_after_a_new_run_exits_nonzero() -> Result<(), DaemonError> {
        let root = test_root("restart-current-failure");
        let target = launchd_target("gui/501");
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = running\nruns = 4\n",
            ),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nruns = 5\nlast exit code = 1\n",
            ),
            enabled_autostart("gui/501"),
        ]);
        let manager = test_manager(&root, runner);
        write_atomic(&manager.definition_path, "installed")?;

        let status = manager.restart()?;
        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                state: DaemonState::Failed(Some(DaemonFailure::ExitCode(1)))
            })
        ));
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn restart_reports_signal_crash_after_sequence_advances() -> Result<(), DaemonError> {
        let root = test_root("restart-signal-failure");
        let target = launchd_target("gui/501");
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
                "state = spawn scheduled\nruns = 4\nlast terminating signal = Segmentation fault: 11\n",
            ),
            enabled_autostart("gui/501"),
        ]);
        let manager = test_manager(&root, runner);
        write_atomic(&manager.definition_path, "installed")?;

        let status = manager.restart()?;
        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                state: DaemonState::Failed(Some(DaemonFailure::Signal {
                    name: Some(name),
                    number: 11,
                    core_dumped: false,
                }))
            }) if name == "Segmentation fault"
        ));
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn restart_bootstraps_an_installed_but_unloaded_service_without_enabling_autostart(
    ) -> Result<(), DaemonError> {
        let root = test_root("restart-bootstrap-sequence");
        let domain = "gui/501";
        let target = launchd_target(domain);
        let definition = launchd_definition_path(&root);
        let definition_text = path_text(&definition)?;
        write_atomic(&definition, "installed")?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::failure(
                LAUNCHCTL,
                &["print", &target],
                LAUNCHD_SERVICE_NOT_FOUND,
                "Could not find specified service",
            ),
            disabled_autostart(domain),
            CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
            CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
            CommandStep::success(LAUNCHCTL, &["disable", &target], ""),
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            disabled_autostart(domain),
        ]);
        let manager = test_manager(&root, runner);

        let status = manager.restart()?;

        assert_eq!(status.state, DaemonState::Running);
        assert_eq!(status.autostart, AutostartState::Disabled);
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn disabled_bootstrap_reports_restore_failure() {
        let result = finish_disabled_bootstrap(Ok(()), Err(DaemonError::ServiceDidNotUnload));

        assert!(matches!(
            result,
            Err(DaemonError::RestoreAutostart { source })
                if matches!(*source, DaemonError::ServiceDidNotUnload)
        ));
    }

    #[test]
    fn disabled_bootstrap_preserves_bootstrap_and_restore_failures() {
        let result = finish_disabled_bootstrap(
            Err(DaemonError::ServiceDidNotUnload),
            Err(DaemonError::ServiceDidNotUnload),
        );

        assert!(matches!(
            result,
            Err(DaemonError::BootstrapAndRestoreAutostart { bootstrap, restore })
                if matches!(*bootstrap, DaemonError::ServiceDidNotUnload)
                    && matches!(*restore, DaemonError::ServiceDidNotUnload)
        ));
    }

    #[test]
    fn failed_bootstrap_still_restores_disabled_autostart() -> Result<(), DaemonError> {
        let root = test_root("restart-bootstrap-failure");
        let domain = "gui/501";
        let target = launchd_target(domain);
        let definition = launchd_definition_path(&root);
        let definition_text = path_text(&definition)?;
        write_atomic(&definition, "installed")?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::failure(
                LAUNCHCTL,
                &["print", &target],
                LAUNCHD_SERVICE_NOT_FOUND,
                "Could not find specified service",
            ),
            disabled_autostart(domain),
            CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
            CommandStep::failure(
                LAUNCHCTL,
                &["bootstrap", domain, &definition_text],
                5,
                "Input/output error",
            ),
            CommandStep::success(LAUNCHCTL, &["disable", &target], ""),
        ]);
        let manager = test_manager(&root, runner);

        let result = manager.restart();

        assert!(matches!(result, Err(DaemonError::CommandFailed(_))));
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn restart_rejects_an_unloaded_service_without_a_definition() {
        let root = test_root("restart-not-installed");
        let target = launchd_target("gui/501");
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
        manager.runner.assert_exhausted();
    }

    #[test]
    fn restart_without_a_sequence_baseline_uses_state_only() -> Result<(), DaemonError> {
        let root = test_root("restart-missing-runs");
        let target = launchd_target("gui/501");
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nlast exit code = 9\n",
            ),
            enabled_autostart("gui/501"),
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
            enabled_autostart("gui/501"),
        ]);
        let manager = test_manager(&root, runner);
        write_atomic(&manager.definition_path, "installed")?;

        let status = manager.restart()?;
        let result = super::super::report_started(&manager, status);

        manager.runner.assert_exhausted();
        result
    }

    #[test]
    fn restart_without_a_sequence_baseline_reports_crash_loop_as_starting(
    ) -> Result<(), DaemonError> {
        let root = test_root("restart-missing-runs-crash-loop");
        let target = launchd_target("gui/501");
        let crash_loop = "state = spawn scheduled\nlast exit code = 1\n";
        let mut steps = vec![
            CommandStep::success(LAUNCHCTL, &["print", &target], "state = waiting\n"),
            CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        ];
        for _ in 0..=super::super::START_OBSERVATION_ATTEMPTS {
            steps.push(CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                crash_loop,
            ));
            steps.push(enabled_autostart("gui/501"));
        }
        let manager = test_manager(&root, ScriptedCommandRunner::new(steps));
        write_atomic(&manager.definition_path, "installed")?;

        let status = manager.restart()?;
        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                state: DaemonState::Starting
            })
        ));
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn start_reporting_observes_autostart_with_each_state() -> Result<(), DaemonError> {
        let root = test_root("start-reporting");
        let domain = "gui/501";
        let target = launchd_target(domain);
        let definition = launchd_definition_path(&root);
        write_atomic(&definition, "installed")?;
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
        let result = super::super::report_started(&manager, status);

        manager.runner.assert_exhausted();
        result
    }

    #[test]
    fn start_reporting_treats_a_throttled_job_as_terminal_failure() -> Result<(), DaemonError> {
        let root = test_root("start-reporting-failure");
        let domain = "gui/501";
        let target = launchd_target(domain);
        let definition = launchd_definition_path(&root);
        write_atomic(&definition, "installed")?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = throttled\nlast exit code = 78\n",
            ),
            CommandStep::success(
                LAUNCHCTL,
                &["print-disabled", domain],
                "\"io.ringsnetwork.node\" => false\n",
            ),
        ]);
        let manager = test_manager(&root, runner);

        let status = super::super::wait_for_running(|| {
            manager.observe_with_attribution(LaunchdAttribution::FreshInstance)
        })?;
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
    fn start_reporting_treats_signal_death_as_terminal_failure() -> Result<(), DaemonError> {
        let root = test_root("start-reporting-signal-failure");
        let domain = "gui/501";
        let target = launchd_target(domain);
        let definition = launchd_definition_path(&root);
        write_atomic(&definition, "installed")?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                LAUNCHCTL,
                &["print", &target],
                "state = spawn scheduled\nruns = 1\nlast terminating signal = Segmentation fault: 11\n",
            ),
            CommandStep::success(
                LAUNCHCTL,
                &["print-disabled", domain],
                "\"io.ringsnetwork.node\" => false\n",
            ),
        ]);
        let manager = test_manager(&root, runner);

        let status = super::super::wait_for_running(|| {
            manager.observe_with_attribution(LaunchdAttribution::FreshInstance)
        })?;
        let error = super::super::report_started(&manager, status);

        assert!(matches!(
            error,
            Err(DaemonError::ServiceDidNotStart {
                state: DaemonState::Failed(Some(DaemonFailure::Signal {
                    name: Some(name),
                    number: 11,
                    core_dumped: false,
                }))
            }) if name == "Segmentation fault"
        ));
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn status_preserves_unexpected_launchctl_failures() {
        let root = test_root("status-failure");
        let target = launchd_target("gui/501");
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
        manager.runner.assert_exhausted();
    }

    #[test]
    fn status_reports_unambiguous_signal_crash() -> Result<(), DaemonError> {
        let root = test_root("status-signal-failure");
        let target = launchd_target("gui/501");
        let runner = ScriptedCommandRunner::new([CommandStep::success(
            LAUNCHCTL,
            &["print", &target],
            "state = spawn scheduled\nruns = 7\nlast terminating signal = Bus error: 10\n",
        )]);
        let manager = test_manager(&root, runner);

        let status = manager.observe()?;

        assert_eq!(status, DaemonStatus {
            state: DaemonState::Failed(Some(DaemonFailure::Signal {
                name: Some("Bus error".to_owned()),
                number: 10,
                core_dumped: false,
            })),
            autostart: AutostartState::Disabled,
        });
        manager.runner.assert_exhausted();
        Ok(())
    }

    #[test]
    fn status_maps_only_launchd_service_not_found_to_not_installed() -> Result<(), DaemonError> {
        let root = test_root("status-not-found");
        let target = launchd_target("gui/501");
        let runner = ScriptedCommandRunner::new([CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        )]);
        let manager = test_manager(&root, runner);

        let status = manager.observe()?;

        assert_eq!(status, DaemonStatus {
            state: DaemonState::NotInstalled,
            autostart: AutostartState::Disabled,
        });
        manager.runner.assert_exhausted();
        Ok(())
    }
}
