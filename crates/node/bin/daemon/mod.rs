//! User-level operating-system service management for the native node.

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

// A start or restart gets a two-second observation budget. Stopped is not terminal during this
// window because service managers can report it before the first spawn is recorded.
const START_OBSERVATION_ATTEMPTS: usize = 20;
const START_OBSERVATION_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
pub(super) enum DaemonCommand {
    #[command(about = "Installs, enables, and starts the user-level node service.")]
    Start(DaemonStartCommand),
    #[command(about = "Stops the user-level node service without disabling login startup.")]
    Stop,
    #[command(about = "Shows the service-manager and login-startup state.")]
    Status,
    #[command(about = "Restarts the installed service without changing login startup.")]
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

#[derive(Debug)]
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
    #[error(
        "could not write temporary service definition {path}: {source}; also could not remove it: {cleanup}"
    )]
    WriteAndCleanupServiceDefinition {
        path: PathBuf,
        #[source]
        source: io::Error,
        cleanup: io::Error,
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
    #[cfg(any(target_os = "macos", all(test, unix)))]
    #[error("could not restore disabled login autostart after bootstrapping: {source}")]
    RestoreAutostart {
        #[source]
        source: Box<DaemonError>,
    },
    #[cfg(any(target_os = "macos", all(test, unix)))]
    #[error(
        "could not bootstrap the disabled service: {bootstrap}; also could not restore disabled login autostart: {restore}"
    )]
    BootstrapAndRestoreAutostart {
        bootstrap: Box<DaemonError>,
        restore: Box<DaemonError>,
    },
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

/// Runs service-manager commands at the process boundary.
trait CommandRunner {
    fn run(&self, program: &'static str, args: &[&str]) -> Result<Output, DaemonError>;

    fn run_checked(&self, program: &'static str, args: &[&str]) -> Result<Output, DaemonError> {
        let output = self.run(program, args)?;
        if output.status.success() {
            return Ok(output);
        }
        Err(command_failure(program, args, output).into())
    }
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

/// A concrete cause for a failed state, attributed either to the process or its manager.
#[derive(Debug, Eq, PartialEq)]
enum DaemonFailure {
    ExitCode(i32),
    Signal {
        name: Option<String>,
        number: i32,
        core_dumped: bool,
    },
    #[cfg(any(target_os = "linux", all(test, unix)))]
    Manager(DaemonManagerFailure),
}

/// A failure caused or diagnosed by the service manager rather than by a process exit record.
#[cfg(any(target_os = "linux", all(test, unix)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DaemonManagerFailure {
    Timeout,
    Watchdog,
    OutOfMemory,
    StartLimit,
    Protocol,
    Resources,
}

impl fmt::Display for DaemonFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExitCode(code) => write!(formatter, "exit code {code}"),
            Self::Signal {
                name: Some(name),
                number,
                core_dumped: false,
            } => write!(formatter, "signal {name}: {number}"),
            Self::Signal {
                name: None,
                number,
                core_dumped: false,
            } => write!(formatter, "signal {number}"),
            Self::Signal {
                name: Some(name),
                number,
                core_dumped: true,
            } => write!(formatter, "signal {name}: {number}, core dumped"),
            Self::Signal {
                name: None,
                number,
                core_dumped: true,
            } => write!(formatter, "signal {number}, core dumped"),
            #[cfg(any(target_os = "linux", all(test, unix)))]
            Self::Manager(failure) => write!(formatter, "{failure}"),
        }
    }
}

#[cfg(any(target_os = "linux", all(test, unix)))]
impl fmt::Display for DaemonManagerFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("service-manager timeout"),
            Self::Watchdog => formatter.write_str("watchdog failure"),
            Self::OutOfMemory => formatter.write_str("out-of-memory kill"),
            Self::StartLimit => formatter.write_str("start limit reached"),
            Self::Protocol => formatter.write_str("service protocol failure"),
            Self::Resources => formatter.write_str("service resource failure"),
        }
    }
}

/// The user-visible lifecycle of the installed daemon.
///
/// `Stopped` means a definition exists but no process is running. `NotInstalled` means the
/// manager has no installed definition. `Failed(None)` means the manager reports failure without
/// an attributable cause.
#[derive(Debug, Eq, PartialEq)]
enum DaemonState {
    NotInstalled,
    Running,
    Stopped,
    Failed(Option<DaemonFailure>),
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
        // Stopped can be transient immediately after bootstrap or restart, before the manager has
        // recorded its first spawn. Only absence or a manager-declared failure ends polling.
        matches!(self, Self::NotInstalled | Self::Failed(_))
    }
}

impl fmt::Display for DaemonState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("not installed"),
            Self::Running => formatter.write_str("running"),
            Self::Stopped => formatter.write_str("stopped"),
            Self::Failed(Some(failure)) => write!(formatter, "failed ({failure})"),
            Self::Failed(None) => formatter.write_str("failed"),
            Self::Starting => formatter.write_str("starting"),
            #[cfg(any(target_os = "linux", all(test, unix)))]
            Self::Stopping => formatter.write_str("stopping"),
            Self::Unknown(state) => write!(formatter, "unknown ({state})"),
        }
    }
}

/// Whether the installed definition is registered to start at login.
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

/// One observation containing both lifecycle and login-startup state.
#[derive(Debug, Eq, PartialEq)]
struct DaemonStatus {
    state: DaemonState,
    autostart: AutostartState,
}

/// The complete foreground command encoded into an OS service definition.
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

/// The common lifecycle operations, each returning one complete status observation.
trait ServiceManager {
    fn name(&self) -> &'static str;
    fn definition_path(&self) -> &Path;
    fn start(&self, spec: &ServiceSpec) -> Result<DaemonStatus, DaemonError>;
    fn stop(&self) -> Result<DaemonStatus, DaemonError>;
    fn restart(&self) -> Result<DaemonStatus, DaemonError>;
    fn observe(&self) -> Result<DaemonStatus, DaemonError>;
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
            let status = manager.start(&spec)?;
            report_started(manager.as_ref(), status)?;
        }
        DaemonCommand::Stop => {
            let status = manager.stop()?;
            print_status(manager.as_ref(), &status);
        }
        DaemonCommand::Status => {
            print_status(manager.as_ref(), &manager.observe()?);
        }
        DaemonCommand::Restart => {
            let status = manager.restart()?;
            report_started(manager.as_ref(), status)?;
        }
    }
    Ok(())
}

fn report_started(manager: &dyn ServiceManager, status: DaemonStatus) -> Result<(), DaemonError> {
    print_status(manager, &status);
    if status.state.is_running() {
        Ok(())
    } else {
        Err(DaemonError::ServiceDidNotStart {
            state: status.state,
        })
    }
}

fn wait_for_running<F>(mut observe: F) -> Result<DaemonStatus, DaemonError>
where F: FnMut() -> Result<DaemonStatus, DaemonError> {
    let mut status = observe()?;
    for _ in 0..START_OBSERVATION_ATTEMPTS {
        if status.state.is_running() || status.state.is_terminal_start_failure() {
            return Ok(status);
        }
        thread::sleep(START_OBSERVATION_INTERVAL);
        status = observe()?;
    }
    Ok(status)
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
    write_atomic_with(path, contents, |temporary, value| {
        fs::write(temporary, value)
    })
}

fn write_atomic_with<F>(path: &Path, contents: &str, write: F) -> Result<(), DaemonError>
where F: FnOnce(&Path, &str) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| DaemonError::NonUtf8Path {
            path: path.to_path_buf(),
        })?;
    // LaunchAgents is scanned by launchd. The dot keeps a partial file hidden from that scan, and
    // the process id prevents concurrent CLI invocations from sharing a temporary path.
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    ensure_parent_directory(path)?;
    if let Err(source) = write(&temporary, contents) {
        return match remove_temporary(&temporary) {
            Ok(()) => Err(DaemonError::WriteServiceDefinition {
                path: temporary,
                source,
            }),
            Err(cleanup) => Err(DaemonError::WriteAndCleanupServiceDefinition {
                path: temporary,
                source,
                cleanup,
            }),
        };
    }
    if let Err(source) = fs::rename(&temporary, path) {
        return match remove_temporary(&temporary) {
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

fn remove_temporary(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn command_output_value<'a>(output: &'a str, field: &str, separator: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        let (name, value) = line.trim().split_once(separator)?;
        (name.trim() == field).then(|| value.trim())
    })
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
    use std::ops::Deref;

    use clap::Parser;

    use super::super::Cli;
    use super::*;

    pub(super) struct TestRoot {
        path: PathBuf,
    }

    impl TestRoot {
        pub(super) fn new(area: &str, name: &str) -> Self {
            let path =
                env::temp_dir().join(format!("rings-daemon-{area}-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Deref for TestRoot {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.path
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

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
    fn atomic_write_removes_temporary_file_when_install_fails() -> io::Result<()> {
        let root = TestRoot::new("shared", "atomic-install-failure");
        let target = root.join("definition");
        let temporary = root.join(format!(".definition.{}.tmp", std::process::id()));
        fs::create_dir_all(&target)?;

        let result = write_atomic(&target, "definition");

        assert!(matches!(
            result,
            Err(DaemonError::InstallServiceDefinition { .. })
        ));
        assert!(!temporary.exists());
        Ok(())
    }

    #[test]
    fn atomic_write_removes_partial_file_when_write_fails() {
        let root = TestRoot::new("shared", "atomic-write-failure");
        let target = root.join("definition");
        let temporary = root.join(format!(".definition.{}.tmp", std::process::id()));

        let result = write_atomic_with(&target, "definition", |path, contents| {
            fs::write(path, contents)?;
            Err(io::Error::other("injected write failure"))
        });

        assert!(matches!(
            result,
            Err(DaemonError::WriteServiceDefinition { .. })
        ));
        assert!(!temporary.exists());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_validates_non_utf8_name_before_creating_parent() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let root = TestRoot::new("shared", "atomic-non-utf8");
        let parent = root.join("new-parent");
        let target = parent.join(OsStr::from_bytes(b"definition-\xff"));

        let result = write_atomic(&target, "definition");

        assert!(matches!(result, Err(DaemonError::NonUtf8Path { .. })));
        assert!(!parent.exists());
    }

    #[test]
    fn config_path_reports_missing_file_with_init_guidance() {
        let root = TestRoot::new("shared", "missing-config");
        let missing = root.join("config.yaml");

        let error = resolve_config_path(missing.to_string_lossy().as_ref());

        assert!(matches!(
            &error,
            Err(DaemonError::ConfigNotFound { path }) if *path == missing
        ));
        assert!(error
            .as_ref()
            .err()
            .is_some_and(|error| error.to_string().contains("run `rings init` first")));
    }

    #[test]
    fn config_path_canonicalizes_an_existing_file() -> io::Result<()> {
        let root = TestRoot::new("shared", "existing-config");
        fs::create_dir_all(&*root)?;
        let config = root.join("config.yaml");
        fs::write(&config, "config")?;

        let resolved = resolve_config_path(config.to_string_lossy().as_ref());

        assert_eq!(resolved.ok(), config.canonicalize().ok());
        Ok(())
    }
}
