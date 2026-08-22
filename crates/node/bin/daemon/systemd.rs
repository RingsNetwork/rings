#![cfg(target_os = "linux")]

use std::env;
use std::path::Path;
use std::path::PathBuf;

use super::run_checked;
use super::run_command;
use super::write_atomic;
use super::AutostartState;
use super::DaemonError;
use super::DaemonState;
use super::DaemonStatus;
use super::ServiceManager;
use super::ServiceSpec;

const SYSTEMD_UNIT: &str = "rings-node.service";

pub(super) struct SystemdManager {
    unit_path: PathBuf,
}

impl SystemdManager {
    pub(super) fn discover() -> Result<Self, DaemonError> {
        let home = home::home_dir().ok_or(DaemonError::HomeDirectoryUnavailable)?;
        let xdg_config_home = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let config_home = systemd_config_home(&home, xdg_config_home.as_deref());
        Ok(Self {
            unit_path: config_home.join("systemd").join("user").join(SYSTEMD_UNIT),
        })
    }
}

impl ServiceManager for SystemdManager {
    fn name(&self) -> &'static str {
        "systemd --user"
    }

    fn definition_path(&self) -> &Path {
        &self.unit_path
    }

    fn start(&self, spec: &ServiceSpec) -> Result<(), DaemonError> {
        write_atomic(&self.unit_path, &render_systemd_unit(spec))?;
        reload_enable_restart()
    }

    fn stop(&self) -> Result<(), DaemonError> {
        if self.unit_path.is_file() {
            systemctl(&["--user", "stop", SYSTEMD_UNIT])?;
        }
        Ok(())
    }

    fn restart(&self) -> Result<(), DaemonError> {
        if !self.unit_path.is_file() {
            return Err(DaemonError::ServiceNotInstalled {
                path: self.unit_path.clone(),
            });
        }
        reload_enable_restart()
    }

    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        if !self.unit_path.is_file() {
            return Ok(DaemonStatus {
                state: DaemonState::NotInstalled,
                autostart: AutostartState::Disabled,
            });
        }
        let active = systemctl_output(&["--user", "is-active", SYSTEMD_UNIT])?;
        let enabled = systemctl_output(&["--user", "is-enabled", SYSTEMD_UNIT])?;
        Ok(DaemonStatus {
            state: parse_systemd_state(&String::from_utf8_lossy(&active.stdout)),
            autostart: parse_systemd_autostart(&String::from_utf8_lossy(&enabled.stdout)),
        })
    }
}

fn reload_enable_restart() -> Result<(), DaemonError> {
    systemctl(&["--user", "daemon-reload"])?;
    systemctl(&["--user", "enable", SYSTEMD_UNIT])?;
    systemctl(&["--user", "restart", SYSTEMD_UNIT])?;
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<(), DaemonError> {
    systemctl_output_checked(args).map(|_| ())
}

// systemctl is intentionally resolved through PATH for non-FHS systems such as NixOS.
fn systemctl_output(args: &[&str]) -> Result<std::process::Output, DaemonError> {
    run_command("systemctl", args)
}

fn systemctl_output_checked(args: &[&str]) -> Result<std::process::Output, DaemonError> {
    run_checked("systemctl", args)
}

fn systemd_config_home(home: &Path, candidate: Option<&Path>) -> PathBuf {
    candidate
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"))
}

fn render_systemd_unit(spec: &ServiceSpec) -> String {
    let command = spec
        .arguments()
        .into_iter()
        .map(systemd_exec_quote)
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
Description=Rings Network node\n\
Wants=network-online.target\n\
After=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
WorkingDirectory={}\n\
ExecStart={command}\n\
Restart=on-failure\n\
RestartSec=5\n\
TimeoutStopSec=30\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        systemd_path_quote(&spec.working_directory)
    )
}

fn systemd_exec_quote(value: &str) -> String {
    systemd_quote(value, true)
}

fn systemd_path_quote(value: &str) -> String {
    systemd_quote(value, false)
}

fn systemd_quote(value: &str, escape_dollar: bool) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' if escape_dollar => quoted.push_str("$$"),
            '%' => quoted.push_str("%%"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn parse_systemd_state(output: &str) -> DaemonState {
    match output.trim() {
        "active" => DaemonState::Running,
        "inactive" => DaemonState::Stopped,
        "failed" => DaemonState::Failed,
        "activating" | "reloading" => DaemonState::Starting,
        "deactivating" => DaemonState::Stopping,
        "" => DaemonState::Unknown("empty systemctl response".to_owned()),
        other => DaemonState::Unknown(other.to_owned()),
    }
}

fn parse_systemd_autostart(output: &str) -> AutostartState {
    match output.trim() {
        "enabled" | "enabled-runtime" | "linked" | "linked-runtime" | "alias" => {
            AutostartState::Enabled
        }
        "disabled" | "masked" | "masked-runtime" => AutostartState::Disabled,
        _ => AutostartState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use rings_node::logging::LogLevel;

    use super::super::super::RuntimeFlavor;
    use super::super::tests::service_spec;
    use super::*;

    #[test]
    fn definition_quotes_arguments_and_sets_working_directory() -> Result<(), DaemonError> {
        let spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
        let unit = render_systemd_unit(&spec);

        assert!(unit.contains("ExecStart=\"/Users/test user/bin/rings\""));
        assert!(unit.contains("\"/Users/test user/.rings/config&prod.yaml\""));
        assert!(unit.contains("WorkingDirectory=\"/Users/test user/work\""));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        Ok(())
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
    fn state_parser_preserves_lifecycle_states() {
        assert_eq!(parse_systemd_state("active\n"), DaemonState::Running);
        assert_eq!(parse_systemd_state("inactive\n"), DaemonState::Stopped);
        assert_eq!(parse_systemd_state("failed\n"), DaemonState::Failed);
        assert_eq!(parse_systemd_state("activating\n"), DaemonState::Starting);
        assert_eq!(parse_systemd_state("deactivating\n"), DaemonState::Stopping);
        assert_eq!(
            parse_systemd_autostart("enabled\n"),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_systemd_autostart("indirect\n"),
            AutostartState::Unknown
        );
    }

    #[test]
    fn quote_escapes_service_manager_expansion_characters() {
        assert_eq!(
            systemd_exec_quote("/tmp/a $HOME/%n/\"rings\""),
            "\"/tmp/a $$HOME/%%n/\\\"rings\\\"\""
        );
        assert_eq!(
            systemd_path_quote("/tmp/a $HOME/%n/\"rings\""),
            "\"/tmp/a $HOME/%%n/\\\"rings\\\"\""
        );
    }
}
