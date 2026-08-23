#![cfg(any(target_os = "macos", all(test, unix)))]

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::thread;

use thiserror::Error;

use super::command_failure;
use super::ensure_parent_directory;
use super::path_text;
use super::run_checked;
use super::write_atomic;
use super::AutostartState;
use super::CommandRunner;
use super::DaemonError;
use super::DaemonState;
use super::FailureBoundary;
use super::ProcessCommandRunner;
use super::ServiceManager;
use super::ServiceSpec;
use super::START_STATUS_ATTEMPTS;
use super::START_STATUS_INTERVAL;

const LAUNCHD_LABEL: &str = "io.ringsnetwork.node";
// `launchctl error 113` is "Could not find specified service"; other failures are real errors.
const LAUNCHD_SERVICE_NOT_FOUND: i32 = 113;

#[derive(Debug, Error)]
pub(super) enum LaunchdDefinitionError {
    // Debug formatting keeps the rejected control character escaped in diagnostics.
    #[error("value contains a character forbidden by XML 1.0 launchd plists: {value:?}")]
    XmlIncompatibleValue { value: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureAttribution {
    CurrentStatus,
    Action(FailureBoundary),
}

impl FailureAttribution {
    fn failed_exit_is_current(self, observed_sequence: Option<u64>) -> bool {
        match self {
            Self::CurrentStatus | Self::Action(FailureBoundary::Unambiguous) => true,
            Self::Action(FailureBoundary::PostAction {
                sequence: Some(baseline),
            }) => observed_sequence.is_some_and(|sequence| sequence > baseline),
            Self::Action(FailureBoundary::PostAction { sequence: None }) => false,
        }
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
        let output = run_checked(&runner, "/usr/bin/id", &["-u"])?;
        let user_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if user_id.is_empty() || !user_id.chars().all(|character| character.is_ascii_digit()) {
            return Err(DaemonError::InvalidUserId { output: user_id });
        }
        let domain = format!("gui/{user_id}");
        let target = format!("{domain}/{LAUNCHD_LABEL}");
        let logs = home.join(".rings").join("logs");
        Ok(Self {
            definition_path: home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            stdout_log: logs.join("daemon.log"),
            stderr_log: logs.join("daemon.error.log"),
            domain,
            target,
            runner,
        })
    }
}

impl<R> LaunchdManager<R>
where R: CommandRunner
{
    fn service_output(&self) -> Result<Option<Output>, DaemonError> {
        let arguments = ["print", self.target.as_str()];
        let output = self.runner.run("/bin/launchctl", &arguments)?;
        if output.status.success() {
            return Ok(Some(output));
        }
        if output.status.code() == Some(LAUNCHD_SERVICE_NOT_FOUND) {
            return Ok(None);
        }
        Err(command_failure("/bin/launchctl", &arguments, output).into())
    }

    fn is_loaded(&self) -> Result<bool, DaemonError> {
        self.service_output().map(|output| output.is_some())
    }

    fn unload_if_loaded(&self) -> Result<(), DaemonError> {
        if !self.is_loaded()? {
            return Ok(());
        }
        run_checked(&self.runner, "/bin/launchctl", &["bootout", &self.target])?;
        for _ in 0..START_STATUS_ATTEMPTS {
            if !self.is_loaded()? {
                return Ok(());
            }
            thread::sleep(START_STATUS_INTERVAL);
        }
        Err(DaemonError::ServiceDidNotUnload)
    }

    fn bootstrap(&self) -> Result<(), DaemonError> {
        let definition = path_text(&self.definition_path)?;
        run_checked(&self.runner, "/bin/launchctl", &["enable", &self.target])?;
        run_checked(&self.runner, "/bin/launchctl", &[
            "bootstrap",
            &self.domain,
            &definition,
        ])?;
        Ok(())
    }

    fn autostart_state(&self) -> Result<AutostartState, DaemonError> {
        if !self.definition_path.is_file() {
            return Ok(AutostartState::Disabled);
        }
        let output = run_checked(&self.runner, "/bin/launchctl", &[
            "print-disabled",
            &self.domain,
        ])?;
        Ok(parse_launchd_autostart(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    fn state_with_attribution(
        &self,
        attribution: FailureAttribution,
    ) -> Result<DaemonState, DaemonError> {
        let installed = self.definition_path.is_file();
        Ok(match self.service_output()? {
            Some(output) => {
                parse_launchd_state(&String::from_utf8_lossy(&output.stdout), attribution)
            }
            None if installed => DaemonState::Stopped,
            None => DaemonState::NotInstalled,
        })
    }
}

impl<R> ServiceManager for LaunchdManager<R>
where R: CommandRunner
{
    fn name(&self) -> &'static str {
        "launchd"
    }

    fn definition_path(&self) -> &Path {
        &self.definition_path
    }

    fn start(&self, spec: &ServiceSpec) -> Result<FailureBoundary, DaemonError> {
        let definition = render_launchd_plist(spec, &self.stdout_log, &self.stderr_log)?;
        ensure_parent_directory(&self.stdout_log)?;
        ensure_parent_directory(&self.stderr_log)?;
        write_atomic(&self.definition_path, &definition)?;
        self.unload_if_loaded()?;
        self.bootstrap()?;
        Ok(FailureBoundary::Unambiguous)
    }

    fn stop(&self) -> Result<(), DaemonError> {
        self.unload_if_loaded()
    }

    fn restart(&self) -> Result<FailureBoundary, DaemonError> {
        if let Some(output) = self.service_output()? {
            let sequence = parse_launchd_runs(&String::from_utf8_lossy(&output.stdout));
            run_checked(&self.runner, "/bin/launchctl", &["enable", &self.target])?;
            run_checked(&self.runner, "/bin/launchctl", &[
                "kickstart",
                "-k",
                &self.target,
            ])?;
            Ok(FailureBoundary::PostAction { sequence })
        } else if self.definition_path.is_file() {
            self.bootstrap()?;
            Ok(FailureBoundary::Unambiguous)
        } else {
            Err(DaemonError::ServiceNotInstalled {
                path: self.definition_path.clone(),
            })
        }
    }

    fn state(&self) -> Result<DaemonState, DaemonError> {
        self.state_with_attribution(FailureAttribution::CurrentStatus)
    }

    fn autostart(&self) -> Result<AutostartState, DaemonError> {
        self.autostart_state()
    }

    fn state_after_action(&self, boundary: FailureBoundary) -> Result<DaemonState, DaemonError> {
        self.state_with_attribution(FailureAttribution::Action(boundary))
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

fn launchd_value<'a>(output: &'a str, field: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix(field)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn parse_launchd_runs(output: &str) -> Option<u64> {
    launchd_value(output, "runs = ")?.parse().ok()
}

fn failed_exit_belongs_to_attempt(output: &str, attribution: FailureAttribution) -> bool {
    let has_failed_exit = launchd_value(output, "last exit code = ")
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<i32>().ok())
        .is_some_and(|code| code != 0);
    has_failed_exit && attribution.failed_exit_is_current(parse_launchd_runs(output))
}

fn parse_launchd_state(output: &str, attribution: FailureAttribution) -> DaemonState {
    let state = launchd_value(output, "state = ");
    let has_current_failed_exit = failed_exit_belongs_to_attempt(output, attribution);
    match state {
        Some("running") => DaemonState::Running,
        Some("spawn scheduled") if has_current_failed_exit => DaemonState::Failed,
        Some("spawn scheduled") => DaemonState::Starting,
        Some("waiting" | "exited" | "not running") if has_current_failed_exit => {
            DaemonState::Failed
        }
        Some("waiting" | "exited" | "not running") => DaemonState::Stopped,
        Some("throttled") => DaemonState::Failed,
        Some(other) => DaemonState::Unknown(other.to_owned()),
        None => DaemonState::Unknown("loaded without a state field".to_owned()),
    }
}

fn parse_launchd_autostart(output: &str) -> AutostartState {
    let mut recognized_listing = false;
    for line in output.lines() {
        let line = line.trim().trim_end_matches(';');
        if is_disabled_services_listing(line) {
            recognized_listing = true;
            continue;
        }
        let Some((label, value)) = line.split_once("=>") else {
            continue;
        };
        if label.trim().trim_matches('"') != LAUNCHD_LABEL {
            continue;
        }
        return match value.trim() {
            "true" | "disabled" => AutostartState::Disabled,
            "false" | "enabled" => AutostartState::Enabled,
            _ => AutostartState::Unknown,
        };
    }
    if recognized_listing {
        AutostartState::Enabled
    } else {
        AutostartState::Unknown
    }
}

fn is_disabled_services_listing(line: &str) -> bool {
    let Some((heading, listing)) = line.split_once('=') else {
        return false;
    };
    heading.trim() == "disabled services" && matches!(listing.trim(), "{" | "{}")
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
    use super::super::DaemonStatus;
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "rings-daemon-launchd-{name}-{}",
            std::process::id()
        ))
    }

    fn test_manager(
        root: &Path,
        runner: ScriptedCommandRunner,
    ) -> LaunchdManager<ScriptedCommandRunner> {
        let domain = "gui/501".to_owned();
        LaunchdManager {
            definition_path: root
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            stdout_log: root.join(".rings/logs/daemon.log"),
            stderr_log: root.join(".rings/logs/daemon.error.log"),
            target: format!("{domain}/{LAUNCHD_LABEL}"),
            domain,
            runner,
        }
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
        let _ = fs::remove_dir_all(&root);
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
    fn state_parser_preserves_launchd_lifecycle_and_failure_states() {
        assert_eq!(
            parse_launchd_state("state = running\n", FailureAttribution::CurrentStatus),
            DaemonState::Running
        );
        assert_eq!(
            parse_launchd_state("state = waiting\n", FailureAttribution::CurrentStatus),
            DaemonState::Stopped
        );
        assert_eq!(
            parse_launchd_state(
                "state = throttled\nlast exit code = 78\n",
                FailureAttribution::CurrentStatus,
            ),
            DaemonState::Failed
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\n",
                FailureAttribution::CurrentStatus,
            ),
            DaemonState::Starting
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nlast exit code = 1\n",
                FailureAttribution::CurrentStatus,
            ),
            DaemonState::Failed
        );
        assert_eq!(
            parse_launchd_state(
                "state = exited\nlast exit code = 0\n",
                FailureAttribution::CurrentStatus,
            ),
            DaemonState::Stopped
        );
        assert_eq!(
            parse_launchd_state(
                "state = exited\nlast exit code = 78\n",
                FailureAttribution::CurrentStatus,
            ),
            DaemonState::Failed
        );
    }

    #[test]
    fn state_parser_assigns_only_newer_runs_exit_to_restart_attempt() {
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nlast exit code = 1\n",
                FailureAttribution::Action(FailureBoundary::Unambiguous),
            ),
            DaemonState::Failed
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nlast exit code = 9\n",
                FailureAttribution::Action(FailureBoundary::PostAction { sequence: None }),
            ),
            DaemonState::Starting
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nruns = 4\nlast exit code = 9\n",
                FailureAttribution::Action(FailureBoundary::PostAction { sequence: Some(4) }),
            ),
            DaemonState::Starting
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nruns = 5\nlast exit code = 1\n",
                FailureAttribution::Action(FailureBoundary::PostAction { sequence: Some(4) }),
            ),
            DaemonState::Failed
        );
    }

    #[test]
    fn autostart_parser_matches_only_the_rings_service() {
        let output = r#"disabled services = {
    "unrelated.service" => true
    "io.ringsnetwork.node" => false
}"#;

        assert_eq!(parse_launchd_autostart(output), AutostartState::Enabled);
        assert_eq!(
            parse_launchd_autostart("disabled services = {}"),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_launchd_autostart("disabled services = {\n}"),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_launchd_autostart("\"io.ringsnetwork.node\" => disabled"),
            AutostartState::Disabled
        );
        assert_eq!(
            parse_launchd_autostart("\"io.ringsnetwork.node\" => enabled"),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_launchd_autostart("\"io.ringsnetwork.node\" => malformed"),
            AutostartState::Unknown
        );
        assert_eq!(
            parse_launchd_autostart("unrecognized launchctl output"),
            AutostartState::Unknown
        );
    }

    #[test]
    fn start_waits_for_bootout_then_bootstraps_without_kickstart() -> Result<(), DaemonError> {
        let root = test_root("start-sequence");
        let _ = fs::remove_dir_all(&root);
        let domain = "gui/501";
        let target = format!("{domain}/{LAUNCHD_LABEL}");
        let definition = root
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"));
        let definition_text = path_text(&definition)?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success("/bin/launchctl", &["print", &target], "state = running\n"),
            CommandStep::success("/bin/launchctl", &["bootout", &target], ""),
            CommandStep::failure(
                "/bin/launchctl",
                &["print", &target],
                LAUNCHD_SERVICE_NOT_FOUND,
                "Could not find specified service",
            ),
            CommandStep::success("/bin/launchctl", &["enable", &target], ""),
            CommandStep::success(
                "/bin/launchctl",
                &["bootstrap", domain, &definition_text],
                "",
            ),
        ]);
        let manager = test_manager(&root, runner);
        let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

        let boundary = manager.start(&spec)?;

        assert!(manager.definition_path.is_file());
        assert_eq!(boundary, FailureBoundary::Unambiguous);
        manager.runner.assert_exhausted();
        assert!(fs::remove_dir_all(&root).is_ok());
        Ok(())
    }

    #[test]
    fn restart_ignores_stale_exit_until_a_new_run_is_observed() -> anyhow::Result<()> {
        let root = test_root("restart-sequence");
        let _ = fs::remove_dir_all(&root);
        let domain = "gui/501";
        let target = format!("{domain}/{LAUNCHD_LABEL}");
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                "/bin/launchctl",
                &["print", &target],
                "state = running\nruns = 4\n",
            ),
            CommandStep::success("/bin/launchctl", &["enable", &target], ""),
            CommandStep::success("/bin/launchctl", &["kickstart", "-k", &target], ""),
            CommandStep::success(
                "/bin/launchctl",
                &["print", &target],
                "state = spawn scheduled\nruns = 4\nlast exit code = 9\n",
            ),
            CommandStep::success(
                "/bin/launchctl",
                &["print", &target],
                "state = running\nruns = 5\n",
            ),
        ]);
        let manager = test_manager(&root, runner);

        let boundary = manager.restart()?;
        let result = super::super::report_started(&manager, boundary);

        assert_eq!(boundary, FailureBoundary::PostAction { sequence: Some(4) });
        manager.runner.assert_exhausted();
        result
    }

    #[test]
    fn restart_bootstraps_an_installed_but_unloaded_service() -> Result<(), DaemonError> {
        let root = test_root("restart-bootstrap-sequence");
        let _ = fs::remove_dir_all(&root);
        let domain = "gui/501";
        let target = format!("{domain}/{LAUNCHD_LABEL}");
        let definition = root
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"));
        let definition_text = path_text(&definition)?;
        write_atomic(&definition, "installed")?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::failure(
                "/bin/launchctl",
                &["print", &target],
                LAUNCHD_SERVICE_NOT_FOUND,
                "Could not find specified service",
            ),
            CommandStep::success("/bin/launchctl", &["enable", &target], ""),
            CommandStep::success(
                "/bin/launchctl",
                &["bootstrap", domain, &definition_text],
                "",
            ),
        ]);
        let manager = test_manager(&root, runner);

        let boundary = manager.restart()?;

        assert_eq!(boundary, FailureBoundary::Unambiguous);
        manager.runner.assert_exhausted();
        assert!(fs::remove_dir_all(&root).is_ok());
        Ok(())
    }

    #[test]
    fn restart_rejects_an_unloaded_service_without_a_definition() {
        let root = test_root("restart-not-installed");
        let target = format!("gui/501/{LAUNCHD_LABEL}");
        let runner = ScriptedCommandRunner::new([CommandStep::failure(
            "/bin/launchctl",
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
    fn restart_without_a_sequence_baseline_uses_state_only() -> anyhow::Result<()> {
        let root = test_root("restart-missing-runs");
        let target = format!("gui/501/{LAUNCHD_LABEL}");
        let runner = ScriptedCommandRunner::new([
            CommandStep::success("/bin/launchctl", &["print", &target], "state = running\n"),
            CommandStep::success("/bin/launchctl", &["enable", &target], ""),
            CommandStep::success("/bin/launchctl", &["kickstart", "-k", &target], ""),
            CommandStep::success(
                "/bin/launchctl",
                &["print", &target],
                "state = spawn scheduled\nlast exit code = 9\n",
            ),
            CommandStep::success("/bin/launchctl", &["print", &target], "state = running\n"),
        ]);
        let manager = test_manager(&root, runner);

        let boundary = manager.restart()?;
        let result = super::super::report_started(&manager, boundary);

        assert_eq!(boundary, FailureBoundary::PostAction { sequence: None });
        manager.runner.assert_exhausted();
        result
    }

    #[test]
    fn start_reporting_polls_state_then_reads_autostart_once() -> anyhow::Result<()> {
        let root = test_root("start-reporting");
        let _ = fs::remove_dir_all(&root);
        let domain = "gui/501";
        let target = format!("{domain}/{LAUNCHD_LABEL}");
        let definition = root
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"));
        write_atomic(&definition, "installed")?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success("/bin/launchctl", &["print", &target], "state = waiting\n"),
            CommandStep::success("/bin/launchctl", &["print", &target], "state = running\n"),
            CommandStep::success(
                "/bin/launchctl",
                &["print-disabled", domain],
                "\"io.ringsnetwork.node\" => false\n",
            ),
        ]);
        let manager = test_manager(&root, runner);

        let result = super::super::report_started(&manager, FailureBoundary::Unambiguous);

        manager.runner.assert_exhausted();
        assert!(fs::remove_dir_all(&root).is_ok());
        result
    }

    #[test]
    fn start_reporting_treats_a_throttled_job_as_terminal_failure() -> anyhow::Result<()> {
        let root = test_root("start-reporting-failure");
        let _ = fs::remove_dir_all(&root);
        let domain = "gui/501";
        let target = format!("{domain}/{LAUNCHD_LABEL}");
        let definition = root
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"));
        write_atomic(&definition, "installed")?;
        let runner = ScriptedCommandRunner::new([
            CommandStep::success(
                "/bin/launchctl",
                &["print", &target],
                "state = throttled\nlast exit code = 78\n",
            ),
            CommandStep::success(
                "/bin/launchctl",
                &["print-disabled", domain],
                "\"io.ringsnetwork.node\" => false\n",
            ),
        ]);
        let manager = test_manager(&root, runner);

        let error = super::super::report_started(&manager, FailureBoundary::Unambiguous)
            .err()
            .map(|error| error.to_string());

        assert_eq!(
            error.as_deref(),
            Some("the daemon did not reach the running state; current state: failed")
        );
        manager.runner.assert_exhausted();
        assert!(fs::remove_dir_all(&root).is_ok());
        Ok(())
    }

    #[test]
    fn status_preserves_unexpected_launchctl_failures() {
        let root = test_root("status-failure");
        let target = format!("gui/501/{LAUNCHD_LABEL}");
        let runner = ScriptedCommandRunner::new([CommandStep::failure(
            "/bin/launchctl",
            &["print", &target],
            112,
            "Could not find specified domain",
        )]);
        let manager = test_manager(&root, runner);

        let result = manager.status();

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
    fn status_maps_only_launchd_service_not_found_to_not_installed() -> Result<(), DaemonError> {
        let root = test_root("status-not-found");
        let target = format!("gui/501/{LAUNCHD_LABEL}");
        let runner = ScriptedCommandRunner::new([CommandStep::failure(
            "/bin/launchctl",
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        )]);
        let manager = test_manager(&root, runner);

        let status = manager.status()?;

        assert_eq!(status, DaemonStatus {
            state: DaemonState::NotInstalled,
            autostart: AutostartState::Disabled,
        });
        manager.runner.assert_exhausted();
        Ok(())
    }
}
