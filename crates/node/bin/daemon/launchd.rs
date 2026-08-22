#![cfg(target_os = "macos")]

use std::path::Path;
use std::path::PathBuf;
use std::thread;

use super::ensure_parent_directory;
use super::path_text;
use super::run_checked;
use super::run_command;
use super::write_atomic;
use super::AutostartState;
use super::DaemonError;
use super::DaemonState;
use super::DaemonStatus;
use super::ServiceManager;
use super::ServiceSpec;
use super::START_STATUS_ATTEMPTS;
use super::START_STATUS_INTERVAL;

const LAUNCHD_LABEL: &str = "io.ringsnetwork.node";

pub(super) struct LaunchdManager {
    definition_path: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
    domain: String,
    target: String,
}

impl LaunchdManager {
    pub(super) fn discover() -> Result<Self, DaemonError> {
        let home = home::home_dir().ok_or(DaemonError::HomeDirectoryUnavailable)?;
        let output = run_checked("/usr/bin/id", &["-u"])?;
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
        })
    }

    fn is_loaded(&self) -> Result<bool, DaemonError> {
        Ok(run_command("/bin/launchctl", &["print", &self.target])?
            .status
            .success())
    }

    fn unload_if_loaded(&self) -> Result<(), DaemonError> {
        if !self.is_loaded()? {
            return Ok(());
        }
        run_checked("/bin/launchctl", &["bootout", &self.target])?;
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
        run_checked("/bin/launchctl", &["enable", &self.target])?;
        run_checked("/bin/launchctl", &["bootstrap", &self.domain, &definition])?;
        Ok(())
    }

    fn autostart(&self) -> Result<AutostartState, DaemonError> {
        if !self.definition_path.is_file() {
            return Ok(AutostartState::Disabled);
        }
        let output = run_command("/bin/launchctl", &["print-disabled", &self.domain])?;
        if !output.status.success() {
            return Ok(AutostartState::Unknown);
        }
        Ok(parse_launchd_autostart(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

impl ServiceManager for LaunchdManager {
    fn name(&self) -> &'static str {
        "launchd"
    }

    fn definition_path(&self) -> &Path {
        &self.definition_path
    }

    fn start(&self, spec: &ServiceSpec) -> Result<(), DaemonError> {
        ensure_parent_directory(&self.stdout_log)?;
        ensure_parent_directory(&self.stderr_log)?;
        write_atomic(
            &self.definition_path,
            &render_launchd_plist(spec, &self.stdout_log, &self.stderr_log)?,
        )?;
        self.unload_if_loaded()?;
        self.bootstrap()
    }

    fn stop(&self) -> Result<(), DaemonError> {
        self.unload_if_loaded()
    }

    fn restart(&self) -> Result<(), DaemonError> {
        if !self.definition_path.is_file() {
            return Err(DaemonError::ServiceNotInstalled {
                path: self.definition_path.clone(),
            });
        }
        run_checked("/bin/launchctl", &["enable", &self.target])?;
        if self.is_loaded()? {
            run_checked("/bin/launchctl", &["kickstart", "-k", &self.target])?;
            Ok(())
        } else {
            self.bootstrap()
        }
    }

    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        let installed = self.definition_path.is_file();
        let output = run_command("/bin/launchctl", &["print", &self.target])?;
        let state = if output.status.success() {
            parse_launchd_state(&String::from_utf8_lossy(&output.stdout))
        } else if installed {
            DaemonState::Stopped
        } else {
            DaemonState::NotInstalled
        };
        Ok(DaemonStatus {
            state,
            autostart: self.autostart()?,
        })
    }
}

fn render_launchd_plist(
    spec: &ServiceSpec,
    stdout_log: &Path,
    stderr_log: &Path,
) -> Result<String, DaemonError> {
    let arguments = spec
        .arguments()
        .into_iter()
        .map(|argument| format!("    <string>{}</string>\n", xml_escape(argument)))
        .collect::<String>();
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
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
        xml_escape(&spec.working_directory),
        xml_escape(&path_text(stdout_log)?),
        xml_escape(&path_text(stderr_log)?),
    ))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn parse_launchd_state(output: &str) -> DaemonState {
    let state = output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("state = ")
            .map(str::trim)
            .filter(|state| !state.is_empty())
    });
    match state {
        Some("running") => DaemonState::Running,
        Some("waiting" | "exited" | "not running") => DaemonState::Stopped,
        Some(other) => DaemonState::Unknown(other.to_owned()),
        None => DaemonState::Unknown("loaded without a state field".to_owned()),
    }
}

fn parse_launchd_autostart(output: &str) -> AutostartState {
    for line in output.lines() {
        let Some((label, value)) = line.trim().trim_end_matches(';').split_once("=>") else {
            continue;
        };
        if label.trim().trim_matches('"') != LAUNCHD_LABEL {
            continue;
        }
        return match value.trim() {
            "true" => AutostartState::Disabled,
            "false" => AutostartState::Enabled,
            _ => AutostartState::Unknown,
        };
    }
    AutostartState::Enabled
}

#[cfg(test)]
mod tests {
    use rings_node::logging::LogLevel;

    use super::super::super::RuntimeFlavor;
    use super::super::tests::service_spec;
    use super::*;

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
    fn state_parser_preserves_running_stopped_and_unknown_states() {
        assert_eq!(
            parse_launchd_state("state = running\n"),
            DaemonState::Running
        );
        assert_eq!(
            parse_launchd_state("state = waiting\n"),
            DaemonState::Stopped
        );
        assert_eq!(
            parse_launchd_state("state = throttled\n"),
            DaemonState::Unknown("throttled".to_owned())
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
            parse_launchd_autostart("\"io.ringsnetwork.node\" => malformed"),
            AutostartState::Unknown
        );
    }
}
