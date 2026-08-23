//! User-level operating-system service management for the native node.
#![cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]

use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Output;
use std::thread;
use std::time::Duration;

use clap::Args;
use clap::Subcommand;
use clap::ValueEnum;
use rings_node::logging::LogLevel;
use thiserror::Error;

use super::ConfigArgs;
use super::RuntimeFlavor;

#[cfg(any(target_os = "macos", all(test, unix)))]
mod launchd;
#[cfg(any(target_os = "linux", all(test, unix)))]
mod systemd;

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

impl DaemonStartCommand {
    pub(super) fn config_path(&self) -> &str {
        &self.config_args.config
    }
}

#[derive(Clone, Debug)]
pub(super) struct WorkerOptions {
    pub(super) log_level: LogLevel,
    pub(super) runtime: RuntimeFlavor,
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
    #[cfg(any(target_os = "macos", all(test, unix)))]
    #[error(transparent)]
    LaunchdDefinition(#[from] launchd::LaunchdDefinitionError),
    #[cfg(any(target_os = "linux", all(test, unix)))]
    #[error(transparent)]
    SystemdDefinition(#[from] systemd::SystemdDefinitionError),
    #[error("could not derive a CLI name for {value}")]
    CliValueNameUnavailable { value: String },
    #[error("could not prepare the parent directory for {path}: {source}")]
    EnsureParentDirectory {
        path: PathBuf,
        #[source]
        source: Box<rings_node::error::Error>,
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
    #[error(
        "could not install service definition {path}: {source}; also could not remove temporary file {temporary}: {cleanup}"
    )]
    InstallAndCleanupServiceDefinition {
        path: PathBuf,
        temporary: PathBuf,
        #[source]
        source: io::Error,
        cleanup: io::Error,
    },
    #[error("could not execute {program}: {source}")]
    ExecuteCommand {
        program: &'static str,
        #[source]
        source: io::Error,
    },
    #[cfg(any(target_os = "linux", all(test, unix)))]
    #[error("{manager} returned malformed service status: {detail}")]
    MalformedServiceStatus {
        manager: &'static str,
        detail: &'static str,
    },
    #[error(transparent)]
    CommandFailed(#[from] CommandFailure),
    #[cfg(target_os = "macos")]
    #[error("could not read the current user id from `{output}`")]
    InvalidUserId { output: String },
    #[error("the daemon service is not installed at {path}; run `rings daemon start` first")]
    ServiceNotInstalled { path: PathBuf },
    #[error("the daemon did not reach the running state; current state: {state}")]
    ServiceDidNotStart { state: DaemonState },
    #[cfg(any(target_os = "macos", all(test, unix)))]
    #[error("launchd did not unload the daemon service")]
    ServiceDidNotUnload,
}

#[derive(Debug)]
struct CommandFailure {
    command: String,
    status: ExitStatus,
    detail: Option<String>,
}

impl fmt::Display for CommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "command failed: {} ({})",
            self.command, self.status
        )?;
        if let Some(detail) = &self.detail {
            write!(formatter, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CommandFailure {}

trait CommandRunner {
    fn run(&self, program: &'static str, args: &[&str]) -> Result<Output, DaemonError>;
}

struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, program: &'static str, args: &[&str]) -> Result<Output, DaemonError> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(|source| DaemonError::ExecuteCommand { program, source })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DaemonState {
    NotInstalled,
    Running,
    Stopped,
    Failed,
    Starting,
    #[cfg(any(target_os = "linux", all(test, unix)))]
    Stopping,
    Unknown(String),
}

impl DaemonState {
    #[cfg(any(target_os = "linux", all(test, unix)))]
    fn is_installed(&self) -> bool {
        !matches!(self, Self::NotInstalled)
    }

    fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    fn is_terminal_start_failure(&self) -> bool {
        matches!(self, Self::NotInstalled | Self::Failed)
    }
}

impl fmt::Display for DaemonState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("not installed"),
            Self::Running => formatter.write_str("running"),
            Self::Stopped => formatter.write_str("stopped"),
            Self::Failed => formatter.write_str("failed"),
            Self::Starting => formatter.write_str("starting"),
            #[cfg(any(target_os = "linux", all(test, unix)))]
            Self::Stopping => formatter.write_str("stopping"),
            Self::Unknown(state) => write!(formatter, "unknown ({state})"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutostartState {
    Enabled,
    Disabled,
    Unknown,
}

impl fmt::Display for AutostartState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enabled => formatter.write_str("enabled"),
            Self::Disabled => formatter.write_str("disabled"),
            Self::Unknown => formatter.write_str("unknown"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct DaemonStatus {
    state: DaemonState,
    autostart: AutostartState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureBoundary {
    Unambiguous,
    #[cfg(any(target_os = "macos", all(test, unix)))]
    PostAction {
        sequence: Option<u64>,
    },
}

#[derive(Debug)]
struct ServiceSpec {
    executable: String,
    config: String,
    working_directory: String,
    log_level: String,
    runtime: String,
}

impl ServiceSpec {
    fn discover(config: &str, options: WorkerOptions) -> Result<Self, DaemonError> {
        let executable =
            env::current_exe().map_err(|source| DaemonError::CurrentExecutable { source })?;
        let working_directory =
            env::current_dir().map_err(|source| DaemonError::CurrentDirectory { source })?;
        let config = resolve_config_path(config)?;
        Ok(Self {
            executable: path_text(&executable)?,
            config: path_text(&config)?,
            working_directory: path_text(&working_directory)?,
            log_level: cli_value_name(&options.log_level)?,
            runtime: cli_value_name(&options.runtime)?,
        })
    }

    fn arguments(&self) -> [&str; 8] {
        [
            self.executable.as_str(),
            "--log-level",
            self.log_level.as_str(),
            "--runtime",
            self.runtime.as_str(),
            "run",
            "--config",
            self.config.as_str(),
        ]
    }
}

fn cli_value_name<T>(value: &T) -> Result<String, DaemonError>
where T: ValueEnum + fmt::Debug {
    value
        .to_possible_value()
        .map(|possible| possible.get_name().to_owned())
        .ok_or_else(|| DaemonError::CliValueNameUnavailable {
            value: format!("{value:?}"),
        })
}

trait ServiceManager {
    fn name(&self) -> &'static str;
    fn definition_path(&self) -> &Path;
    fn start(&self, spec: &ServiceSpec) -> Result<FailureBoundary, DaemonError>;
    fn stop(&self) -> Result<(), DaemonError>;
    fn restart(&self) -> Result<FailureBoundary, DaemonError>;
    fn state(&self) -> Result<DaemonState, DaemonError>;
    fn autostart(&self) -> Result<AutostartState, DaemonError>;

    fn state_after_action(&self, _boundary: FailureBoundary) -> Result<DaemonState, DaemonError> {
        self.state()
    }

    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        Ok(DaemonStatus {
            state: self.state()?,
            autostart: self.autostart()?,
        })
    }
}

#[cfg(target_os = "macos")]
fn current_service_manager() -> Result<Box<dyn ServiceManager>, DaemonError> {
    Ok(Box::new(launchd::LaunchdManager::discover()?))
}

#[cfg(target_os = "linux")]
fn current_service_manager() -> Result<Box<dyn ServiceManager>, DaemonError> {
    Ok(Box::new(systemd::SystemdManager::discover()?))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn current_service_manager() -> Result<Box<dyn ServiceManager>, DaemonError> {
    Err(DaemonError::UnsupportedPlatform)
}

pub(super) fn execute(command: DaemonCommand, options: WorkerOptions) -> anyhow::Result<()> {
    let manager = current_service_manager()?;
    match command {
        DaemonCommand::Start(args) => {
            let spec = ServiceSpec::discover(args.config_path(), options)?;
            let boundary = manager.start(&spec)?;
            report_started(manager.as_ref(), boundary)
        }
        DaemonCommand::Stop => {
            manager.stop()?;
            print_status(manager.as_ref(), &manager.status()?);
            Ok(())
        }
        DaemonCommand::Status => {
            print_status(manager.as_ref(), &manager.status()?);
            Ok(())
        }
        DaemonCommand::Restart => {
            let boundary = manager.restart()?;
            report_started(manager.as_ref(), boundary)
        }
    }
}

fn report_started(manager: &dyn ServiceManager, boundary: FailureBoundary) -> anyhow::Result<()> {
    let status = DaemonStatus {
        state: wait_for_running(manager, boundary)?,
        autostart: manager.autostart()?,
    };
    print_status(manager, &status);
    if status.state.is_running() {
        Ok(())
    } else {
        Err(DaemonError::ServiceDidNotStart {
            state: status.state,
        }
        .into())
    }
}

fn wait_for_running(
    manager: &dyn ServiceManager,
    boundary: FailureBoundary,
) -> Result<DaemonState, DaemonError> {
    let mut state = manager.state_after_action(boundary)?;
    for _ in 0..START_STATUS_ATTEMPTS {
        if state.is_running() || state.is_terminal_start_failure() {
            return Ok(state);
        }
        thread::sleep(START_STATUS_INTERVAL);
        state = manager.state_after_action(boundary)?;
    }
    Ok(state)
}

fn print_status(manager: &dyn ServiceManager, status: &DaemonStatus) {
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

fn ensure_parent_directory(path: &Path) -> Result<(), DaemonError> {
    rings_node::util::ensure_parent_dir(path).map_err(|source| DaemonError::EnsureParentDirectory {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), DaemonError> {
    ensure_parent_directory(path)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| DaemonError::NonUtf8Path {
            path: path.to_path_buf(),
        })?;
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    fs::write(&temporary, contents).map_err(|source| DaemonError::WriteServiceDefinition {
        path: temporary.clone(),
        source,
    })?;
    if let Err(source) = fs::rename(&temporary, path) {
        return match fs::remove_file(&temporary) {
            Ok(()) => Err(DaemonError::InstallServiceDefinition {
                path: path.to_path_buf(),
                source,
            }),
            Err(cleanup) => Err(DaemonError::InstallAndCleanupServiceDefinition {
                path: path.to_path_buf(),
                temporary,
                source,
                cleanup,
            }),
        };
    }
    Ok(())
}

fn run_checked<R>(runner: &R, program: &'static str, args: &[&str]) -> Result<Output, DaemonError>
where R: CommandRunner + ?Sized {
    let output = runner.run(program, args)?;
    if output.status.success() {
        return Ok(output);
    }
    Err(command_failure(program, args, output).into())
}

fn command_failure(program: &str, args: &[&str], output: Output) -> CommandFailure {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let detail = if !stderr.is_empty() {
        Some(stderr)
    } else if !stdout.is_empty() {
        Some(stdout)
    } else {
        None
    };
    CommandFailure {
        command: format_command(program, args),
        status: output.status,
        detail,
    }
}

fn format_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::super::Cli;
    use super::*;

    #[cfg(unix)]
    pub(super) mod command_runner {
        use std::cell::RefCell;
        use std::collections::VecDeque;
        use std::os::unix::process::ExitStatusExt;

        use super::super::CommandRunner;
        use super::super::DaemonError;
        use super::super::Output;

        pub(crate) struct CommandStep {
            program: String,
            args: Vec<String>,
            output: Output,
        }

        impl CommandStep {
            pub(crate) fn success(program: &str, args: &[&str], stdout: &str) -> Self {
                Self::with_status(program, args, 0, stdout, "")
            }

            pub(crate) fn failure(program: &str, args: &[&str], status: i32, stderr: &str) -> Self {
                Self::with_status(program, args, status, "", stderr)
            }

            fn with_status(
                program: &str,
                args: &[&str],
                status: i32,
                stdout: &str,
                stderr: &str,
            ) -> Self {
                Self {
                    program: program.to_owned(),
                    args: args.iter().map(|argument| (*argument).to_owned()).collect(),
                    output: Output {
                        status: std::process::ExitStatus::from_raw(status << 8),
                        stdout: stdout.as_bytes().to_vec(),
                        stderr: stderr.as_bytes().to_vec(),
                    },
                }
            }
        }

        pub(crate) struct ScriptedCommandRunner {
            steps: RefCell<VecDeque<CommandStep>>,
        }

        impl ScriptedCommandRunner {
            pub(crate) fn new(steps: impl IntoIterator<Item = CommandStep>) -> Self {
                Self {
                    steps: RefCell::new(steps.into_iter().collect()),
                }
            }

            pub(crate) fn assert_exhausted(&self) {
                assert!(
                    self.steps.borrow().is_empty(),
                    "scripted command runner has unconsumed steps"
                );
            }
        }

        impl CommandRunner for ScriptedCommandRunner {
            fn run(&self, program: &'static str, args: &[&str]) -> Result<Output, DaemonError> {
                let Some(step) = self.steps.borrow_mut().pop_front() else {
                    return Err(DaemonError::ExecuteCommand {
                        program,
                        source: std::io::Error::other(format!(
                            "unexpected scripted command: {program} {args:?}"
                        )),
                    });
                };
                assert_eq!(program, step.program);
                assert_eq!(args, step.args);
                Ok(step.output)
            }
        }
    }

    pub(super) fn service_spec(
        log_level: &LogLevel,
        runtime: &RuntimeFlavor,
    ) -> Result<ServiceSpec, DaemonError> {
        Ok(ServiceSpec {
            executable: "/Users/test user/bin/rings".to_owned(),
            config: "/Users/test user/.rings/config&prod.yaml".to_owned(),
            working_directory: "/Users/test user/work".to_owned(),
            log_level: cli_value_name(log_level)?,
            runtime: cli_value_name(runtime)?,
        })
    }

    #[test]
    fn generated_worker_arguments_parse_for_every_cli_value() -> Result<(), DaemonError> {
        for log_level in LogLevel::value_variants() {
            for runtime in RuntimeFlavor::value_variants() {
                let spec = service_spec(log_level, runtime)?;
                let expected = Some((spec.log_level.clone(), spec.runtime.clone()));
                let parsed_names = Cli::try_parse_from(spec.arguments())
                    .ok()
                    .and_then(|parsed| {
                        Some((
                            parsed.log_level.to_possible_value()?.get_name().to_owned(),
                            parsed.runtime.to_possible_value()?.get_name().to_owned(),
                        ))
                    });
                assert_eq!(parsed_names, expected);
            }
        }
        Ok(())
    }

    #[test]
    fn human_command_format_does_not_apply_service_manager_escaping() {
        let command = format_command("rings", &["run", "$HOME/%n"]);

        assert_eq!(command, "\"rings\" \"run\" \"$HOME/%n\"");
    }

    #[test]
    fn atomic_write_removes_temporary_file_when_install_fails(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = env::temp_dir().join(format!(
            "rings-daemon-atomic-write-test-{}",
            std::process::id()
        ));
        let target = root.join("definition");
        let temporary = root.join(format!(".definition.{}.tmp", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&target)?;

        let result = write_atomic(&target, "definition");

        assert!(matches!(
            result,
            Err(DaemonError::InstallServiceDefinition { .. })
        ));
        assert!(!temporary.exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
