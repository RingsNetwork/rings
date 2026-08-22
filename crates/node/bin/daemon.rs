//! User-level operating-system service management for the native node.

use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::thread;
use std::time::Duration;

use clap::Args;
use clap::Subcommand;
use thiserror::Error;

use super::ConfigArgs;

#[cfg(any(target_os = "macos", test))]
const LAUNCHD_LABEL: &str = "io.ringsnetwork.node";
#[cfg(any(target_os = "linux", test))]
const SYSTEMD_UNIT: &str = "rings-node.service";
const START_STATUS_ATTEMPTS: usize = 20;
const START_STATUS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
pub(super) enum DaemonCommand {
    #[command(about = "Installs, enables, and starts the user-level node service.")]
    Start(DaemonStartCommand),
    #[command(about = "Stops the user-level node service without disabling login startup.")]
    Stop,
    #[command(about = "Shows the service-manager and login-startup state.")]
    Status,
    #[command(about = "Restarts the installed user-level node service.")]
    Restart,
}

#[derive(Args, Debug)]
pub(super) struct DaemonStartCommand {
    #[command(flatten)]
    config_args: ConfigArgs,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct WorkerOptions {
    pub(super) log_level: &'static str,
    pub(super) runtime: &'static str,
}

#[derive(Debug, Error)]
enum DaemonError {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[error("user-level daemon management is supported only on macOS and Linux")]
    UnsupportedPlatform,
    #[error("could not determine the current user's home directory")]
    HomeDirectoryUnavailable,
    #[error("could not resolve the current working directory: {source}")]
    CurrentDirectory {
        #[source]
        source: io::Error,
    },
    #[error("could not resolve the running rings executable: {source}")]
    CurrentExecutable {
        #[source]
        source: io::Error,
    },
    #[error("could not expand configuration path {path}: {source}")]
    ExpandConfig {
        path: PathBuf,
        #[source]
        source: Box<rings_node::error::Error>,
    },
    #[error("configuration file does not exist: {path}; run `rings init` first")]
    ConfigNotFound { path: PathBuf },
    #[error("could not resolve configuration file {path}: {source}")]
    ResolveConfig {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("path is not valid UTF-8 and cannot be written to a service definition: {path}")]
    NonUtf8Path { path: PathBuf },
    #[error("service definition path has no parent directory: {path}")]
    MissingParentDirectory { path: PathBuf },
    #[error("could not create directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not write temporary service definition {path}: {source}")]
    WriteServiceDefinition {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not install service definition {path}: {source}")]
    InstallServiceDefinition {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not execute {program}: {source}")]
    ExecuteCommand {
        program: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("command failed: {command} ({status}){detail}")]
    CommandFailed {
        command: String,
        status: String,
        detail: CommandFailureDetail,
    },
    #[cfg(target_os = "macos")]
    #[error("could not read the current user id from `{output}`")]
    InvalidUserId { output: String },
    #[error("the daemon service is not installed at {path}; run `rings daemon start` first")]
    ServiceNotInstalled { path: PathBuf },
    #[error("the daemon did not reach the running state; current state: {state}")]
    ServiceDidNotStart { state: DaemonState },
}

#[derive(Debug)]
struct CommandFailureDetail(String);

impl fmt::Display for CommandFailureDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            Ok(())
        } else {
            write!(formatter, ": {}", self.0)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DaemonState {
    NotInstalled,
    Running,
    Stopped,
    #[cfg(any(target_os = "linux", test))]
    Failed,
    #[cfg(any(target_os = "linux", test))]
    Starting,
    #[cfg(any(target_os = "linux", test))]
    Stopping,
    Unknown(String),
}

impl DaemonState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    fn is_terminal_start_failure(&self) -> bool {
        match self {
            Self::NotInstalled => true,
            #[cfg(any(target_os = "linux", test))]
            Self::Failed => true,
            _ => false,
        }
    }
}

impl fmt::Display for DaemonState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("not installed"),
            Self::Running => formatter.write_str("running"),
            Self::Stopped => formatter.write_str("stopped"),
            #[cfg(any(target_os = "linux", test))]
            Self::Failed => formatter.write_str("failed"),
            #[cfg(any(target_os = "linux", test))]
            Self::Starting => formatter.write_str("starting"),
            #[cfg(any(target_os = "linux", test))]
            Self::Stopping => formatter.write_str("stopping"),
            Self::Unknown(state) => write!(formatter, "unknown ({state})"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutostartState {
    Enabled,
    Disabled,
    #[cfg(any(target_os = "linux", test))]
    Unknown,
}

impl fmt::Display for AutostartState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enabled => formatter.write_str("enabled"),
            Self::Disabled => formatter.write_str("disabled"),
            #[cfg(any(target_os = "linux", test))]
            Self::Unknown => formatter.write_str("unknown"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct DaemonStatus {
    state: DaemonState,
    autostart: AutostartState,
}

#[derive(Debug)]
struct ServiceLayout {
    #[cfg(any(target_os = "macos", test))]
    launchd_plist: PathBuf,
    #[cfg(any(target_os = "linux", test))]
    systemd_unit: PathBuf,
    #[cfg(any(target_os = "macos", test))]
    stdout_log: PathBuf,
    #[cfg(any(target_os = "macos", test))]
    stderr_log: PathBuf,
}

impl ServiceLayout {
    fn discover() -> Result<Self, DaemonError> {
        let home = home::home_dir().ok_or(DaemonError::HomeDirectoryUnavailable)?;
        let xdg_config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        Ok(Self::from_home(&home, xdg_config_home.as_deref()))
    }

    fn from_home(home: &Path, _xdg_config_home: Option<&Path>) -> Self {
        #[cfg(any(target_os = "linux", test))]
        let systemd_config = _xdg_config_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config"));
        #[cfg(any(target_os = "macos", test))]
        let rings_logs = home.join(".rings").join("logs");
        Self {
            #[cfg(any(target_os = "macos", test))]
            launchd_plist: home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            #[cfg(any(target_os = "linux", test))]
            systemd_unit: systemd_config
                .join("systemd")
                .join("user")
                .join(SYSTEMD_UNIT),
            #[cfg(any(target_os = "macos", test))]
            stdout_log: rings_logs.join("daemon.log"),
            #[cfg(any(target_os = "macos", test))]
            stderr_log: rings_logs.join("daemon.error.log"),
        }
    }
}

#[derive(Debug)]
struct ServiceSpec {
    executable: String,
    config: String,
    log_level: &'static str,
    runtime: &'static str,
    #[cfg(any(target_os = "macos", test))]
    stdout_log: String,
    #[cfg(any(target_os = "macos", test))]
    stderr_log: String,
}

impl ServiceSpec {
    fn discover(
        _layout: &ServiceLayout,
        config: &str,
        options: WorkerOptions,
    ) -> Result<Self, DaemonError> {
        let executable =
            env::current_exe().map_err(|source| DaemonError::CurrentExecutable { source })?;
        let config = resolve_config_path(config)?;
        Ok(Self {
            executable: path_text(&executable)?,
            config: path_text(&config)?,
            log_level: options.log_level,
            runtime: options.runtime,
            #[cfg(any(target_os = "macos", test))]
            stdout_log: path_text(&_layout.stdout_log)?,
            #[cfg(any(target_os = "macos", test))]
            stderr_log: path_text(&_layout.stderr_log)?,
        })
    }

    fn arguments(&self) -> [&str; 8] {
        [
            self.executable.as_str(),
            "--log-level",
            self.log_level,
            "--runtime",
            self.runtime,
            "run",
            "--config",
            self.config.as_str(),
        ]
    }
}

enum NativeServiceManager {
    #[cfg(target_os = "macos")]
    Launchd(LaunchdManager),
    #[cfg(target_os = "linux")]
    Systemd(SystemdManager),
}

impl NativeServiceManager {
    fn discover() -> Result<Self, DaemonError> {
        let layout = ServiceLayout::discover()?;
        current_service_manager(layout)
    }

    fn layout(&self) -> &ServiceLayout {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(manager) => &manager.layout,
            #[cfg(target_os = "linux")]
            Self::Systemd(manager) => &manager.layout,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(_) => "launchd",
            #[cfg(target_os = "linux")]
            Self::Systemd(_) => "systemd --user",
        }
    }

    fn definition_path(&self) -> &Path {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(manager) => &manager.layout.launchd_plist,
            #[cfg(target_os = "linux")]
            Self::Systemd(manager) => &manager.layout.systemd_unit,
        }
    }

    fn start(&self, spec: &ServiceSpec) -> Result<(), DaemonError> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(manager) => manager.start(spec),
            #[cfg(target_os = "linux")]
            Self::Systemd(manager) => manager.start(spec),
        }
    }

    fn stop(&self) -> Result<(), DaemonError> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(manager) => manager.stop(),
            #[cfg(target_os = "linux")]
            Self::Systemd(manager) => manager.stop(),
        }
    }

    fn restart(&self) -> Result<(), DaemonError> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(manager) => manager.restart(),
            #[cfg(target_os = "linux")]
            Self::Systemd(manager) => manager.restart(),
        }
    }

    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Launchd(manager) => manager.status(),
            #[cfg(target_os = "linux")]
            Self::Systemd(manager) => manager.status(),
        }
    }
}

#[cfg(target_os = "macos")]
fn current_service_manager(layout: ServiceLayout) -> Result<NativeServiceManager, DaemonError> {
    Ok(NativeServiceManager::Launchd(LaunchdManager { layout }))
}

#[cfg(target_os = "linux")]
fn current_service_manager(layout: ServiceLayout) -> Result<NativeServiceManager, DaemonError> {
    Ok(NativeServiceManager::Systemd(SystemdManager { layout }))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn current_service_manager(_layout: ServiceLayout) -> Result<NativeServiceManager, DaemonError> {
    Err(DaemonError::UnsupportedPlatform)
}

pub(super) fn execute(command: DaemonCommand, options: WorkerOptions) -> anyhow::Result<()> {
    let manager = NativeServiceManager::discover()?;
    match command {
        DaemonCommand::Start(args) => {
            let spec = ServiceSpec::discover(manager.layout(), &args.config_args.config, options)?;
            manager.start(&spec)?;
            let status = wait_for_running(&manager)?;
            print_status(&manager, &status);
            if status.state.is_running() {
                Ok(())
            } else {
                Err(DaemonError::ServiceDidNotStart {
                    state: status.state,
                }
                .into())
            }
        }
        DaemonCommand::Stop => {
            manager.stop()?;
            let status = manager.status()?;
            print_status(&manager, &status);
            Ok(())
        }
        DaemonCommand::Status => {
            let status = manager.status()?;
            print_status(&manager, &status);
            Ok(())
        }
        DaemonCommand::Restart => {
            manager.restart()?;
            let status = wait_for_running(&manager)?;
            print_status(&manager, &status);
            if status.state.is_running() {
                Ok(())
            } else {
                Err(DaemonError::ServiceDidNotStart {
                    state: status.state,
                }
                .into())
            }
        }
    }
}

fn wait_for_running(manager: &NativeServiceManager) -> Result<DaemonStatus, DaemonError> {
    let mut status = manager.status()?;
    for _ in 0..START_STATUS_ATTEMPTS {
        if status.state.is_running() || status.state.is_terminal_start_failure() {
            return Ok(status);
        }
        thread::sleep(START_STATUS_INTERVAL);
        status = manager.status()?;
    }
    Ok(status)
}

fn print_status(manager: &NativeServiceManager, status: &DaemonStatus) {
    println!("rings daemon: {}", status.state);
    println!("manager: {}", manager.name());
    println!("login autostart: {}", status.autostart);
    println!("definition: {}", manager.definition_path().display());
}

fn resolve_config_path(config: &str) -> Result<PathBuf, DaemonError> {
    let supplied = PathBuf::from(config);
    let expanded =
        rings_node::util::expand_home(&supplied).map_err(|source| DaemonError::ExpandConfig {
            path: supplied,
            source: Box::new(source),
        })?;
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .map_err(|source| DaemonError::CurrentDirectory { source })?
            .join(expanded)
    };
    if !absolute.is_file() {
        return Err(DaemonError::ConfigNotFound { path: absolute });
    }
    absolute
        .canonicalize()
        .map_err(|source| DaemonError::ResolveConfig {
            path: absolute,
            source,
        })
}

fn path_text(path: &Path) -> Result<String, DaemonError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| DaemonError::NonUtf8Path {
            path: path.to_path_buf(),
        })
}

#[cfg(target_os = "macos")]
struct LaunchdManager {
    layout: ServiceLayout,
}

#[cfg(target_os = "macos")]
impl LaunchdManager {
    fn start(&self, spec: &ServiceSpec) -> Result<(), DaemonError> {
        create_directory(&self.layout.stdout_log)?;
        write_atomic(&self.layout.launchd_plist, &render_launchd_plist(spec))?;
        self.unload_if_loaded()?;
        let domain = launchd_domain()?;
        let plist = path_text(&self.layout.launchd_plist)?;
        run_checked("/bin/launchctl", &["bootstrap", &domain, &plist])?;
        let target = launchd_target(&domain);
        run_checked("/bin/launchctl", &["kickstart", "-k", &target])?;
        Ok(())
    }

    fn stop(&self) -> Result<(), DaemonError> {
        self.unload_if_loaded()
    }

    fn restart(&self) -> Result<(), DaemonError> {
        if !self.layout.launchd_plist.is_file() {
            return Err(DaemonError::ServiceNotInstalled {
                path: self.layout.launchd_plist.to_path_buf(),
            });
        }
        self.unload_if_loaded()?;
        let domain = launchd_domain()?;
        let plist = path_text(&self.layout.launchd_plist)?;
        run_checked("/bin/launchctl", &["bootstrap", &domain, &plist])?;
        let target = launchd_target(&domain);
        run_checked("/bin/launchctl", &["kickstart", "-k", &target])?;
        Ok(())
    }

    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        let definition_installed = self.layout.launchd_plist.is_file();
        let domain = launchd_domain()?;
        let output = run_command("/bin/launchctl", &["print", &launchd_target(&domain)])?;
        let state = if output.status.success() {
            parse_launchd_state(&String::from_utf8_lossy(&output.stdout))
        } else if definition_installed {
            DaemonState::Stopped
        } else {
            DaemonState::NotInstalled
        };
        Ok(DaemonStatus {
            state,
            autostart: if definition_installed {
                AutostartState::Enabled
            } else {
                AutostartState::Disabled
            },
        })
    }

    fn unload_if_loaded(&self) -> Result<(), DaemonError> {
        let domain = launchd_domain()?;
        let target = launchd_target(&domain);
        let output = run_command("/bin/launchctl", &["print", &target])?;
        if output.status.success() {
            run_checked("/bin/launchctl", &["bootout", &target])?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
struct SystemdManager {
    layout: ServiceLayout,
}

#[cfg(target_os = "linux")]
impl SystemdManager {
    fn start(&self, spec: &ServiceSpec) -> Result<(), DaemonError> {
        write_atomic(&self.layout.systemd_unit, &render_systemd_unit(spec))?;
        run_checked("systemctl", &["--user", "daemon-reload"])?;
        run_checked("systemctl", &["--user", "enable", SYSTEMD_UNIT])?;
        run_checked("systemctl", &["--user", "restart", SYSTEMD_UNIT])?;
        Ok(())
    }

    fn stop(&self) -> Result<(), DaemonError> {
        if self.layout.systemd_unit.is_file() {
            run_checked("systemctl", &["--user", "stop", SYSTEMD_UNIT])?;
        }
        Ok(())
    }

    fn restart(&self) -> Result<(), DaemonError> {
        if !self.layout.systemd_unit.is_file() {
            return Err(DaemonError::ServiceNotInstalled {
                path: self.layout.systemd_unit.to_path_buf(),
            });
        }
        run_checked("systemctl", &["--user", "daemon-reload"])?;
        run_checked("systemctl", &["--user", "enable", SYSTEMD_UNIT])?;
        run_checked("systemctl", &["--user", "restart", SYSTEMD_UNIT])?;
        Ok(())
    }

    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        if !self.layout.systemd_unit.is_file() {
            return Ok(DaemonStatus {
                state: DaemonState::NotInstalled,
                autostart: AutostartState::Disabled,
            });
        }
        let active = run_command("systemctl", &["--user", "is-active", SYSTEMD_UNIT])?;
        let enabled = run_command("systemctl", &["--user", "is-enabled", SYSTEMD_UNIT])?;
        Ok(DaemonStatus {
            state: parse_systemd_state(&String::from_utf8_lossy(&active.stdout)),
            autostart: parse_systemd_autostart(&String::from_utf8_lossy(&enabled.stdout)),
        })
    }
}

#[cfg(any(target_os = "macos", test))]
fn render_launchd_plist(spec: &ServiceSpec) -> String {
    let arguments = spec
        .arguments()
        .into_iter()
        .map(|argument| format!("    <string>{}</string>\n", xml_escape(argument)))
        .collect::<String>();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
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
        xml_escape(&spec.stdout_log),
        xml_escape(&spec.stderr_log),
    )
}

#[cfg(any(target_os = "linux", test))]
fn render_systemd_unit(spec: &ServiceSpec) -> String {
    let command = spec
        .arguments()
        .into_iter()
        .map(systemd_quote)
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
ExecStart={command}\n\
Restart=on-failure\n\
RestartSec=5\n\
TimeoutStopSec=30\n\
\n\
[Install]\n\
WantedBy=default.target\n"
    )
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("$$"),
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

#[cfg(any(target_os = "macos", test))]
fn parse_launchd_state(output: &str) -> DaemonState {
    let state = output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("state = ")
            .map(str::trim)
            .filter(|state| !state.is_empty())
    });
    match state {
        Some("running") => DaemonState::Running,
        Some("waiting") | Some("exited") | Some("not running") => DaemonState::Stopped,
        Some(other) => DaemonState::Unknown(other.to_owned()),
        None => DaemonState::Unknown("loaded without a state field".to_owned()),
    }
}

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "linux", test))]
fn parse_systemd_autostart(output: &str) -> AutostartState {
    match output.trim() {
        "enabled" | "enabled-runtime" | "linked" | "linked-runtime" | "alias" => {
            AutostartState::Enabled
        }
        "disabled" | "masked" | "masked-runtime" => AutostartState::Disabled,
        _ => AutostartState::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> Result<String, DaemonError> {
    let output = run_checked("/usr/bin/id", &["-u"])?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
        return Err(DaemonError::InvalidUserId { output: value });
    }
    Ok(format!("gui/{value}"))
}

#[cfg(target_os = "macos")]
fn launchd_target(domain: &str) -> String {
    format!("{domain}/{LAUNCHD_LABEL}")
}

fn create_directory(path: &Path) -> Result<(), DaemonError> {
    let parent = path
        .parent()
        .ok_or_else(|| DaemonError::MissingParentDirectory {
            path: path.to_path_buf(),
        })?;
    fs::create_dir_all(parent).map_err(|source| DaemonError::CreateDirectory {
        path: parent.to_path_buf(),
        source,
    })
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), DaemonError> {
    create_directory(path)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| DaemonError::NonUtf8Path {
            path: path.to_path_buf(),
        })?;
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&temporary, contents).map_err(|source| DaemonError::WriteServiceDefinition {
        path: temporary.to_path_buf(),
        source,
    })?;
    fs::rename(&temporary, path).map_err(|source| DaemonError::InstallServiceDefinition {
        path: path.to_path_buf(),
        source,
    })
}

fn run_checked(program: &'static str, args: &[&str]) -> Result<Output, DaemonError> {
    let output = run_command(program, args)?;
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    Err(DaemonError::CommandFailed {
        command: format_command(program, args),
        status: output.status.to_string(),
        detail: CommandFailureDetail(detail),
    })
}

fn run_command(program: &'static str, args: &[&str]) -> Result<Output, DaemonError> {
    Command::new(program)
        .args(args)
        .output()
        .map_err(|source| DaemonError::ExecuteCommand { program, source })
}

fn format_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .map(systemd_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_spec() -> ServiceSpec {
        ServiceSpec {
            executable: "/Users/test user/bin/rings".to_owned(),
            config: "/Users/test user/.rings/config&prod.yaml".to_owned(),
            log_level: "warn",
            runtime: "current-thread",
            stdout_log: "/Users/test user/.rings/logs/daemon.log".to_owned(),
            stderr_log: "/Users/test user/.rings/logs/daemon.error.log".to_owned(),
        }
    }

    #[test]
    fn launchd_definition_preserves_worker_arguments_and_escapes_xml() {
        let plist = render_launchd_plist(&service_spec());

        assert!(plist.contains("<string>/Users/test user/bin/rings</string>"));
        assert!(plist.contains("<string>/Users/test user/.rings/config&amp;prod.yaml</string>"));
        assert!(plist.contains("<string>current-thread</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn systemd_definition_quotes_arguments_and_enables_login_startup() {
        let unit = render_systemd_unit(&service_spec());

        assert!(unit.contains("ExecStart=\"/Users/test user/bin/rings\""));
        assert!(unit.contains("\"/Users/test user/.rings/config&prod.yaml\""));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn service_layout_honors_absolute_xdg_config_home() {
        let layout =
            ServiceLayout::from_home(Path::new("/home/test"), Some(Path::new("/srv/test-config")));

        assert_eq!(
            layout.systemd_unit,
            Path::new("/srv/test-config/systemd/user/rings-node.service")
        );
        assert_eq!(
            layout.launchd_plist,
            Path::new("/home/test/Library/LaunchAgents/io.ringsnetwork.node.plist")
        );
    }

    #[test]
    fn launchd_state_parser_preserves_running_stopped_and_unknown_states() {
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
    fn systemd_state_parser_preserves_lifecycle_states() {
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
    fn systemd_quote_escapes_service_manager_expansion_characters() {
        assert_eq!(
            systemd_quote("/tmp/a $HOME/%n/\"rings\""),
            "\"/tmp/a $$HOME/%%n/\\\"rings\\\"\""
        );
    }
}
