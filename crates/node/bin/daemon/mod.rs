//! Coordinates the cross-platform daemon model and its shared process, filesystem, and timing boundaries.

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

// Twenty 100 ms retries cover manager bookkeeping and ordinary in-flight spawns. Adapter lifecycle
// models decide whether an observation is stable or still expected to advance inside this window.
const MANAGER_OBSERVATION_SCHEDULE: PollSchedule = PollSchedule {
    retries: 20,
    interval: Duration::from_millis(100),
};

#[cfg(test)]
const TEST_OBSERVATION_SCHEDULE: PollSchedule = PollSchedule {
    retries: 20,
    interval: Duration::ZERO,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PollSchedule {
    retries: usize,
    interval: Duration,
}

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
    #[error("could not resolve the current working directory")]
    CurrentDirectory {
        #[source]
        source: io::Error,
    },
    #[error("could not resolve the running rings executable")]
    CurrentExecutable {
        #[source]
        source: io::Error,
    },
    #[error("could not expand configuration path {path}")]
    ExpandConfig {
        path: PathBuf,
        #[source]
        source: Box<rings_node::error::Error>,
    },
    #[error("configuration file does not exist: {path}; run `rings init` first")]
    ConfigNotFound { path: PathBuf },
    #[error("could not resolve configuration file {path}")]
    ResolveConfig {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("path is not valid UTF-8 and cannot be written to a service definition: {path}")]
    NonUtf8Path { path: PathBuf },
    #[cfg(any(target_os = "macos", all(test, unix)))]
    #[error(transparent)]
    Launchd(#[from] launchd::LaunchdError),
    #[cfg(any(target_os = "linux", all(test, unix)))]
    #[error(transparent)]
    Systemd(#[from] systemd::SystemdError),
    #[error("could not derive a CLI name for {value}")]
    CliValueNameUnavailable { value: String },
    #[error("could not prepare the parent directory for {path}")]
    EnsureParentDirectory {
        path: PathBuf,
        #[source]
        source: Box<rings_node::error::Error>,
    },
    #[error("could not write temporary service definition {path}")]
    WriteServiceDefinition {
        path: PathBuf,
        #[source]
        failure: RecoveryFailure<io::Error>,
    },
    #[error("could not install service definition {path}")]
    InstallServiceDefinition {
        path: PathBuf,
        #[source]
        failure: RecoveryFailure<io::Error>,
    },
    #[error("could not execute {program}")]
    ExecuteCommand {
        program: &'static str,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    CommandFailed(#[from] CommandFailure),
    #[error("the daemon service is not installed at {path}; run `rings daemon start` first")]
    ServiceNotInstalled { path: PathBuf },
    #[error("the daemon did not reach the running state; current status: {status}")]
    ServiceDidNotStart { status: DaemonStatus },
}

/// A primary operation and its best-effort cleanup form one algebraic result.
///
/// Invariant: `Both` preserves the primary failure and the cleanup failure in operation order.
#[derive(Debug)]
enum RecoveryFailure<E> {
    Primary(E),
    Both { primary: E, recovery: E },
}

impl<E> fmt::Display for RecoveryFailure<E>
where E: std::error::Error + 'static
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Primary(primary) => fmt::Display::fmt(primary, formatter),
            Self::Both { primary, recovery } => {
                write!(formatter, "{primary}; recovery also failed: ")?;
                write_error_chain(formatter, recovery)
            }
        }
    }
}

impl<E> std::error::Error for RecoveryFailure<E>
where E: std::error::Error + 'static
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Primary(primary) => primary.source(),
            Self::Both { primary, .. } => primary.source(),
        }
    }
}

fn write_error_chain<E>(formatter: &mut fmt::Formatter<'_>, error: &E) -> fmt::Result
where E: std::error::Error + 'static {
    write!(formatter, "{error}")?;
    let mut source = error.source();
    while let Some(cause) = source {
        write!(formatter, ": {cause}")?;
        source = cause.source();
    }
    Ok(())
}

fn primary_with_recovery<E>(primary: E, recovery: Result<(), E>) -> RecoveryFailure<E> {
    // Law: the primary value is never replaced by cleanup; cleanup only enriches the failure.
    match recovery {
        Ok(()) => RecoveryFailure::Primary(primary),
        Err(recovery) => RecoveryFailure::Both { primary, recovery },
    }
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
    /// Returns `Err` only when the process could not be executed. Any completed process, including
    /// one with a non-zero exit status, is returned as `Ok(Output)` for adapter classification.
    fn run(&self, program: &'static str, args: &[&str]) -> Result<Output, DaemonError>;

    /// Converts a completed non-zero exit into `CommandFailed`. Immediate classifiers use `run`
    /// directly; higher-level recovery policy may inspect the typed `CommandFailed` status later.
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

/// User-facing projection of an adapter-owned closed failure model.
#[derive(Debug, Eq, PartialEq)]
struct DaemonFailure {
    description: String,
}

impl DaemonFailure {
    fn described(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
        }
    }

    fn from_display(failure: &impl fmt::Display) -> Self {
        Self::described(failure.to_string())
    }
}

impl fmt::Display for DaemonFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.description)
    }
}

/// The user-visible lifecycle of the installed daemon.
///
/// `Stopped` means the manager retains an installed or loaded job but no process is running.
/// `Restarting` means the manager has scheduled another spawn after the optional last failure.
/// `Failed(None)` means the manager reports a terminal failure without an attributable cause.
#[derive(Debug, Eq, PartialEq)]
enum DaemonState {
    Running,
    Stopped,
    Restarting(Option<DaemonFailure>),
    Failed(Option<DaemonFailure>),
    Transitioning(&'static str),
    Unknown(String),
}

impl DaemonState {
    fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

impl fmt::Display for DaemonState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => formatter.write_str("running"),
            Self::Stopped => formatter.write_str("stopped"),
            Self::Restarting(Some(failure)) => write!(formatter, "restarting ({failure})"),
            Self::Restarting(None) => formatter.write_str("restarting"),
            Self::Failed(Some(failure)) => write!(formatter, "failed ({failure})"),
            Self::Failed(None) => formatter.write_str("failed"),
            Self::Transitioning(state) => formatter.write_str(state),
            Self::Unknown(state) => write!(formatter, "unknown ({state})"),
        }
    }
}

/// Whether the installed definition is registered to start at login.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutostartState {
    Enabled,
    Disabled,
    Other(&'static str),
}

impl fmt::Display for AutostartState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enabled => formatter.write_str("enabled"),
            Self::Disabled => formatter.write_str("disabled"),
            Self::Other(state) => formatter.write_str(state),
        }
    }
}

/// Whether the manager lifecycle is expected to advance inside the observation window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartPollDisposition {
    Pending,
    Settled,
}

/// One service-manager lifecycle observation before the independent autostart axis is read.
#[derive(Debug, Eq, PartialEq)]
enum DaemonObservation {
    NotInstalled,
    Installed {
        state: DaemonState,
        start_poll: StartPollDisposition,
    },
}

impl DaemonObservation {
    fn installed(state: DaemonState, start_poll: StartPollDisposition) -> Self {
        Self::Installed { state, start_poll }
    }
}

trait StartPollObservation {
    /// Post: returns true exactly when another observation inside the configured window cannot
    /// improve the start result, including absence and a running process.
    fn settles_start_poll(&self) -> bool;
}

impl StartPollObservation for DaemonObservation {
    fn settles_start_poll(&self) -> bool {
        match self {
            Self::NotInstalled => true,
            Self::Installed { state, start_poll } => {
                state.is_running() || *start_poll == StartPollDisposition::Settled
            }
        }
    }
}

/// One manager observation; autostart exists only when a definition or loaded job exists.
#[derive(Debug, Eq, PartialEq)]
enum DaemonStatus {
    NotInstalled,
    Installed {
        state: DaemonState,
        autostart: AutostartState,
    },
}

impl DaemonStatus {
    fn installed(state: DaemonState, autostart: AutostartState) -> Self {
        Self::Installed { state, autostart }
    }

    fn is_running(&self) -> bool {
        matches!(self, Self::Installed {
            state: DaemonState::Running,
            ..
        })
    }
}

impl fmt::Display for DaemonStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("not installed"),
            Self::Installed { state, .. } => state.fmt(formatter),
        }
    }
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
        Self::discover_with(config, options, env::current_exe, env::current_dir)
    }

    fn discover_with<Executable, WorkingDirectory>(
        config: &str,
        options: WorkerOptions,
        current_executable: Executable,
        current_directory: WorkingDirectory,
    ) -> Result<Self, DaemonError>
    where
        Executable: FnOnce() -> io::Result<PathBuf>,
        WorkingDirectory: FnOnce() -> io::Result<PathBuf>,
    {
        let executable =
            current_executable().map_err(|source| DaemonError::CurrentExecutable { source })?;
        let working_directory =
            current_directory().map_err(|source| DaemonError::CurrentDirectory { source })?;
        Self::from_paths(config, options, &executable, &working_directory)
    }

    fn from_paths(
        config: &str,
        options: WorkerOptions,
        executable: &Path,
        working_directory: &Path,
    ) -> Result<Self, DaemonError> {
        let config = resolve_config_path(config, working_directory)?;
        Ok(Self {
            executable: path_text(executable)?,
            config: path_text(&config)?,
            working_directory: path_text(working_directory)?,
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

/// The common lifecycle boundary.
///
/// Invariant: start enables autostart; stop and restart preserve it unless a reported recovery
/// failure says otherwise.
///
/// Post: every successful operation returns the complete status derived from the same settled
/// manager observation, rather than a later untested re-query.
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
    if status.is_running() {
        Ok(())
    } else {
        Err(DaemonError::ServiceDidNotStart { status })
    }
}

/// Polls manager lifecycle only until the adapter says the lifecycle has settled for start.
fn wait_for_running<T, F>(schedule: PollSchedule, observe: F) -> Result<T, DaemonError>
where
    T: StartPollObservation,
    F: FnMut() -> Result<T, DaemonError>,
{
    poll_until(schedule, observe, StartPollObservation::settles_start_poll)
}

/// Performs at most `retries + 1` observations and at most `retries` sleeps. Every observation,
/// including the final one, is evaluated by `settled` before it is returned.
///
/// Post: `settled` is evaluated before the retry-exhaustion branch for every returned observation.
fn poll_until<T, E, F, P>(schedule: PollSchedule, mut observe: F, settled: P) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
    P: Fn(&T) -> bool,
{
    let mut retries_remaining = schedule.retries;
    loop {
        let value = observe()?;
        let is_settled = settled(&value);
        if is_settled || retries_remaining == 0 {
            return Ok(value);
        }
        thread::sleep(schedule.interval);
        retries_remaining -= 1;
    }
}

fn print_status(manager: &dyn ServiceManager, status: &DaemonStatus) {
    println!("rings daemon: {status}");
    println!("manager: {}", manager.name());
    match status {
        DaemonStatus::NotInstalled => println!("login autostart: not applicable"),
        DaemonStatus::Installed { autostart, .. } => {
            println!("login autostart: {autostart}");
        }
    }
    println!("definition: {}", manager.definition_path().display());
}

fn resolve_config_path(config: &str, working_directory: &Path) -> Result<PathBuf, DaemonError> {
    let supplied = PathBuf::from(config);
    let expanded =
        rings_node::util::expand_home(&supplied).map_err(|source| DaemonError::ExpandConfig {
            path: supplied,
            source: Box::new(source),
        })?;
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        working_directory.join(expanded)
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
    // The hidden `.tmp` path is not a launchd plist or systemd unit; the process id prevents two
    // CLI processes from sharing it. Rename is atomic to observers, not durable across power loss;
    // rerunning `daemon start` recovers an incomplete on-disk definition.
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    ensure_parent_directory(path)?;
    if let Err(source) = write(&temporary, contents) {
        let failure = primary_with_recovery(source, remove_temporary(&temporary));
        return Err(DaemonError::WriteServiceDefinition {
            path: temporary,
            failure,
        });
    }
    if let Err(source) = fs::rename(&temporary, path) {
        let cleanup = remove_temporary(&temporary);
        let error_path = install_failure_path(path, &temporary, &cleanup);
        let failure = primary_with_recovery(source, cleanup);
        return Err(DaemonError::InstallServiceDefinition {
            path: error_path,
            failure,
        });
    }
    Ok(())
}

/// Post: names the temporary artifact exactly when cleanup failed and left it as the actionable
/// path; otherwise names the requested installation target whose rename failed.
fn install_failure_path(target: &Path, temporary: &Path, cleanup: &io::Result<()>) -> PathBuf {
    if cleanup.is_err() {
        temporary.to_path_buf()
    } else {
        target.to_path_buf()
    }
}

fn remove_temporary(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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
    mod model;

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
        }

        impl Drop for ScriptedCommandRunner {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    return;
                }

                assert!(
                    self.steps.get_mut().is_empty(),
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
    fn install_failure_names_the_artifact_that_requires_action() {
        let target = Path::new("definition.plist");
        let temporary = Path::new(".definition.plist.42.tmp");
        let cleanup_succeeded = Ok(());
        let cleanup_failed = Err(io::Error::other("cleanup failed"));

        assert_eq!(
            install_failure_path(target, temporary, &cleanup_succeeded),
            target
        );
        assert_eq!(
            install_failure_path(target, temporary, &cleanup_failed),
            temporary
        );
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

        let error = resolve_config_path(missing.to_string_lossy().as_ref(), &root);

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

        let resolved = resolve_config_path(config.to_string_lossy().as_ref(), &root);

        assert_eq!(resolved.ok(), config.canonicalize().ok());
        Ok(())
    }

    #[test]
    fn config_path_resolves_relative_to_the_captured_working_directory() -> io::Result<()> {
        let root = TestRoot::new("shared", "relative-config");
        fs::create_dir_all(&*root)?;
        let config = root.join("relative.yaml");
        fs::write(&config, "config")?;

        let resolved = resolve_config_path("relative.yaml", &root);

        assert_eq!(resolved.ok(), config.canonicalize().ok());
        Ok(())
    }

    #[test]
    fn config_path_expands_home_before_using_the_working_directory() {
        let root = TestRoot::new("shared", "home-config");
        let missing = "~/.rings/codex-daemon-review-missing.yaml";

        let error = resolve_config_path(missing, &root);

        assert!(matches!(
            error,
            Err(DaemonError::ConfigNotFound { path }) if path.is_absolute() && !path.starts_with(&*root)
        ));
    }
}
