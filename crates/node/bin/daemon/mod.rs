//! Keeps daemon lifecycle policy platform-neutral and manager effects adapter-owned.

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

use clap::ValueEnum;
use rings_node::logging::LogLevel;
use thiserror::Error;

use super::ConfigArgs;
use super::RuntimeFlavor;

mod command;
pub(super) use command::DaemonCommand;

macro_rules! impl_daemon_error_from_adapter {
    ($source:ty => $adapter:ty) => {
        impl From<$source> for DaemonError {
            fn from(error: $source) -> Self {
                <$adapter>::from(error).into()
            }
        }
    };
}

#[cfg(any(target_os = "macos", all(test, unix)))]
mod launchd;
#[cfg(any(target_os = "linux", all(test, unix)))]
mod systemd;

// This is a CLI responsiveness budget, not a wall-clock or manager spawn-latency guarantee. Twenty
// 100 ms sleeps bound manager observation work and return the latest snapshot when bookkeeping or a
// respawn delay outlives it. It is intentionally shorter than the systemd restart delay rendered by
// Rings, so a crash loop is reported as restarting instead of being hidden by a later activation.
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
    #[error("service definition path has no file name: {path}")]
    ServiceDefinitionPathHasNoFileName { path: PathBuf },
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
    #[error("could not prepare service definition {location}")]
    WriteServiceDefinition {
        location: DefinitionFailureLocation,
        #[source]
        failure: RecoveryFailure<io::Error>,
    },
    #[error("could not install service definition {location}")]
    InstallServiceDefinition {
        location: DefinitionFailureLocation,
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
    #[error("the daemon service is not installed at {path}; run `rings daemon install` first")]
    ServiceNotInstalled { path: PathBuf },
    #[error("could not remove service definition {path}")]
    RemoveServiceDefinition {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the daemon did not reach the running state; current status: {status}")]
    ServiceDidNotStart { status: DaemonStatus },
}

#[derive(Debug, Eq, PartialEq)]
struct DefinitionFailureLocation {
    target: PathBuf,
    leftover_temporary: Option<PathBuf>,
}

impl fmt::Display for DefinitionFailureLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at {}", self.target.display())?;
        if let Some(temporary) = &self.leftover_temporary {
            write!(
                formatter,
                "; temporary artifact remains at {}",
                temporary.display()
            )?;
        }
        Ok(())
    }
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
            Self::Primary(primary) => write_error_chain(formatter, primary),
            Self::Both { primary, recovery } => {
                write_error_chain(formatter, primary)?;
                write!(formatter, "; recovery also failed: ")?;
                write_error_chain(formatter, recovery)
            }
        }
    }
}

impl<E> std::error::Error for RecoveryFailure<E>
where E: std::error::Error + 'static
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Display renders both complete chains in operation order. Exposing either branch as the
        // single `source` would make a chain walker misattribute or duplicate the other branch.
        None
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
/// `Restarting` means another spawn is scheduled or remains eligible under manager policy after the
/// optional last failure. Adapter-specific terminal vocabulary is preserved by `Reported`.
#[derive(Debug, Eq, PartialEq)]
enum DaemonState {
    Running,
    Stopped,
    Restarting(Option<DaemonFailure>),
    Transitioning(DaemonTransition),
    Reported {
        status: &'static str,
        detail: Option<DaemonFailure>,
    },
}

impl fmt::Display for DaemonState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => formatter.write_str("running"),
            Self::Stopped => formatter.write_str("stopped"),
            Self::Restarting(Some(failure)) => write!(formatter, "restarting ({failure})"),
            Self::Restarting(None) => formatter.write_str("restarting"),
            Self::Transitioning(state) => state.fmt(formatter),
            Self::Reported {
                status,
                detail: Some(detail),
            } => write!(formatter, "{status} ({detail})"),
            Self::Reported {
                status,
                detail: None,
            } => formatter.write_str(status),
        }
    }
}

/// A manager-reported lifecycle transition that has not reached its stable state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DaemonTransition(&'static str);

impl DaemonTransition {
    fn named(name: &'static str) -> Self {
        Self(name)
    }
}

impl fmt::Display for DaemonTransition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// Whether the installed definition is registered to start at login.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutostartState {
    Enabled,
    Disabled,
    Reported(&'static str),
}

impl fmt::Display for AutostartState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enabled => formatter.write_str("enabled"),
            Self::Disabled => formatter.write_str("disabled"),
            Self::Reported(state) => formatter.write_str(state),
        }
    }
}

/// A non-running lifecycle that may still advance during start settlement.
#[derive(Debug, Eq, PartialEq)]
enum PendingDaemonState {
    Restarting(Option<DaemonFailure>),
    Transitioning(DaemonTransition),
    Unknown(String),
}

impl PendingDaemonState {
    fn into_state(self) -> DaemonState {
        match self {
            Self::Restarting(failure) => DaemonState::Restarting(failure),
            Self::Transitioning(transition) => DaemonState::Transitioning(transition),
            Self::Unknown(state) => DaemonState::Reported {
                status: "unknown",
                detail: Some(DaemonFailure::described(state)),
            },
        }
    }
}

/// A lifecycle paired with its start-settlement proposition.
///
/// `PendingDaemonState` excludes `Running`, so a running service cannot consume the polling budget.
#[derive(Debug, Eq, PartialEq)]
enum ObservedDaemonState {
    Settled(DaemonState),
    Pending(PendingDaemonState),
}

impl ObservedDaemonState {
    fn running() -> Self {
        Self::Settled(DaemonState::Running)
    }

    fn stopped() -> Self {
        Self::Settled(DaemonState::Stopped)
    }

    fn pending(state: PendingDaemonState) -> Self {
        Self::Pending(state)
    }

    fn is_settled(&self) -> bool {
        matches!(self, Self::Settled(_))
    }

    fn into_state(self) -> DaemonState {
        match self {
            Self::Settled(state) => state,
            Self::Pending(state) => state.into_state(),
        }
    }
}

/// One service-manager lifecycle observation with an adapter-selected attachment.
///
/// `A = ()` represents a lifecycle-only observation. systemd attaches `AutostartState` from the
/// same `systemctl show` snapshot, while launchd reads that independent axis when producing status.
#[derive(Debug, Eq, PartialEq)]
enum DaemonObservation<A = ()> {
    NotInstalled,
    Installed {
        state: ObservedDaemonState,
        attachment: A,
    },
}

impl<A> DaemonObservation<A> {
    fn installed(state: ObservedDaemonState, attachment: A) -> Self {
        Self::Installed { state, attachment }
    }

    /// Central fold for an on-disk definition whose manager record is not yet observable.
    fn definition_without_record(attachment: A) -> Self {
        Self::installed(ObservedDaemonState::stopped(), attachment)
    }

    /// Post: absence and every `Settled` lifecycle terminate start polling.
    fn settles_start_poll(&self) -> bool {
        match self {
            Self::NotInstalled => true,
            Self::Installed { state, .. } => state.is_settled(),
        }
    }

    #[cfg(any(target_os = "macos", all(test, unix)))]
    fn is_running(&self) -> bool {
        matches!(self, Self::Installed {
            state: ObservedDaemonState::Settled(DaemonState::Running),
            ..
        })
    }
}

impl DaemonObservation<AutostartState> {
    fn into_status(self) -> DaemonStatus {
        match self {
            Self::NotInstalled => DaemonStatus::NotInstalled,
            Self::Installed { state, attachment } => {
                DaemonStatus::installed(state.into_state(), attachment)
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

/// The common installation and lifecycle boundary.
///
/// Invariant: install enables autostart without starting the process; start, stop, and restart
/// preserve autostart unless a reported recovery failure says otherwise. Uninstall removes both
/// manager lifecycle and local installation evidence.
///
/// Post: successful `start` and `restart` return the lifecycle selected by their settling poll.
/// launchd samples the independent autostart axis when producing status; systemd reads both axes
/// from one manager snapshot. `observe` uses current installation evidence. `stop` reports the
/// manager record it acted upon, even when that record disappears as the result of stopping it.
trait ServiceManager {
    fn name(&self) -> &'static str;
    fn definition_path(&self) -> &Path;
    fn has_definition(&self) -> bool {
        self.definition_path().is_file()
    }
    fn install(&self, spec: &ServiceSpec) -> Result<DaemonStatus, DaemonError>;
    fn uninstall(&self) -> Result<DaemonStatus, DaemonError>;
    fn start(&self) -> Result<DaemonStatus, DaemonError>;
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
        DaemonCommand::Install(args) => {
            let spec = ServiceSpec::discover(args.config_path(), options)?;
            print_status(manager.as_ref(), &manager.install(&spec)?);
        }
        DaemonCommand::Uninstall => {
            print_status(manager.as_ref(), &manager.uninstall()?);
        }
        DaemonCommand::Start => {
            let status = manager.start()?;
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
fn wait_for_start_settlement<A, F>(
    schedule: PollSchedule,
    observe: F,
) -> Result<DaemonObservation<A>, DaemonError>
where
    F: FnMut() -> Result<DaemonObservation<A>, DaemonError>,
{
    poll_until(schedule, observe, DaemonObservation::settles_start_poll)
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
    let file_name =
        path.file_name()
            .ok_or_else(|| DaemonError::ServiceDefinitionPathHasNoFileName {
                path: path.to_path_buf(),
            })?;
    let file_name = file_name.to_str().ok_or_else(|| DaemonError::NonUtf8Path {
        path: path.to_path_buf(),
    })?;
    // The hidden `.tmp` path is not a launchd plist or systemd unit; the process id prevents two
    // CLI processes from sharing it. Rename is atomic to observers, not durable across power loss;
    // rerunning `daemon install` recovers an incomplete on-disk definition.
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    ensure_parent_directory(path)?;
    if let Err(source) = write(&temporary, contents) {
        let cleanup = remove_temporary(&temporary);
        let location = definition_failure_location(path, &temporary, &cleanup);
        let failure = primary_with_recovery(source, cleanup);
        return Err(DaemonError::WriteServiceDefinition { location, failure });
    }
    if let Err(source) = fs::rename(&temporary, path) {
        let cleanup = remove_temporary(&temporary);
        let location = definition_failure_location(path, &temporary, &cleanup);
        let failure = primary_with_recovery(source, cleanup);
        return Err(DaemonError::InstallServiceDefinition { location, failure });
    }
    Ok(())
}

/// Post: always names the requested target and names a temporary artifact only when cleanup failed.
fn definition_failure_location(
    target: &Path,
    temporary: &Path,
    cleanup: &io::Result<()>,
) -> DefinitionFailureLocation {
    DefinitionFailureLocation {
        target: target.to_path_buf(),
        leftover_temporary: cleanup.as_ref().err().map(|_| temporary.to_path_buf()),
    }
}

fn remove_temporary(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_service_definition(path: &Path) -> Result<(), DaemonError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DaemonError::RemoveServiceDefinition {
            path: path.to_path_buf(),
            source,
        }),
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
    //! Provides fixtures for the shared daemon command model.

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

    #[cfg(unix)]
    pub(super) fn fill_poll_budget(
        steps: &mut Vec<command_runner::CommandStep>,
        lifecycle_observations_already_queued: usize,
        mut observation: impl FnMut() -> command_runner::CommandStep,
    ) {
        // A poll performs retries + 1 observations. The explicit queued count documents the one
        // fixture that adds its first transition before filling the remaining budget.
        let total_observations = TEST_OBSERVATION_SCHEDULE.retries.saturating_add(1);
        assert!(lifecycle_observations_already_queued <= total_observations);
        for _ in lifecycle_observations_already_queued..total_observations {
            steps.push(observation());
        }
    }
}
