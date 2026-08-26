//! Reduces `systemctl show` output into the platform-neutral daemon observation.

use thiserror::Error;

use super::super::AutostartState;
use super::super::DaemonFailure;
use super::super::DaemonObservation;
#[cfg(test)]
use super::super::DaemonState;
#[cfg(test)]
use super::super::DaemonStatus;
use super::super::DaemonTransition;
use super::super::ObservedDaemonState;
use super::super::PendingDaemonState;

// Verified with systemd 257.13: ExecMainCode publishes Linux siginfo_t::si_code.
const SYSTEMD_EXEC_CODE_EXITED: i32 = 1;
const SYSTEMD_EXEC_CODE_KILLED: i32 = 2;
const SYSTEMD_EXEC_CODE_DUMPED: i32 = 3;

#[derive(Debug, Error)]
pub(crate) enum SystemdStatusError {
    #[error("status is missing required property {property}")]
    MissingProperty { property: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SystemdLifecycle<'a> {
    Missing,
    Unavailable,
    Running,
    Stopped,
    Failed,
    Starting,
    AutoRestarting,
    Stopping,
    Other {
        load: &'a str,
        active: &'a str,
        sub: &'a str,
    },
}

impl SystemdLifecycle<'_> {
    /// Pre: `output` is the same snapshot from which this lifecycle and `autostart` were parsed.
    ///
    /// Post: every transient lifecycle becomes a `PendingDaemonState`; manager-terminal lifecycle
    /// becomes `ObservedDaemonState::Settled`.
    fn into_record(self, output: &str, autostart: AutostartState) -> SystemdRecord {
        let state = match self {
            Self::Missing => return SystemdRecord::Missing { autostart },
            Self::Unavailable => return SystemdRecord::Unavailable { autostart },
            Self::Running => ObservedDaemonState::running(),
            Self::Stopped => ObservedDaemonState::stopped(),
            Self::Failed => ObservedDaemonState::failed(project_systemd_failure(output)),
            Self::Starting => ObservedDaemonState::pending(PendingDaemonState::Transitioning(
                DaemonTransition::Starting,
            )),
            // Verified with systemd 257.13: `auto-restart` and `auto-restart-queued` mean systemd
            // scheduled another activation. Poll while that transition fits the command budget.
            Self::AutoRestarting => ObservedDaemonState::pending(PendingDaemonState::Restarting(
                project_systemd_failure(output),
            )),
            Self::Stopping => ObservedDaemonState::pending(PendingDaemonState::Transitioning(
                DaemonTransition::Stopping,
            )),
            Self::Other { load, active, sub } => {
                ObservedDaemonState::pending(PendingDaemonState::Unknown(format!(
                    "load state: {load}, active state: {active}, substate: {sub}"
                )))
            }
        };
        SystemdRecord::Loaded(DaemonObservation::installed(state, autostart))
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum SystemdRecord {
    Missing { autostart: AutostartState },
    Unavailable { autostart: AutostartState },
    Loaded(DaemonObservation<AutostartState>),
}

impl SystemdRecord {
    pub(super) fn is_missing(&self) -> bool {
        matches!(self, Self::Missing { .. })
    }

    pub(super) fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    pub(super) fn is_inactive_without_process(&self) -> bool {
        matches!(self, Self::Missing { .. } | Self::Unavailable { .. })
    }

    pub(super) fn into_observation(
        self,
        definition_present: bool,
    ) -> DaemonObservation<AutostartState> {
        match self {
            Self::Missing { autostart } if definition_present => {
                DaemonObservation::definition_without_record(autostart)
            }
            Self::Missing { .. } => DaemonObservation::NotInstalled,
            Self::Unavailable { autostart } => {
                DaemonObservation::installed(ObservedDaemonState::unavailable(), autostart)
            }
            Self::Loaded(observation) => observation,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum SystemdManagerFailure {
    Timeout,
    Watchdog,
    Protocol,
    Resources,
}

impl std::fmt::Display for SystemdManagerFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("service-manager timeout"),
            Self::Watchdog => formatter.write_str("watchdog failure"),
            Self::Protocol => formatter.write_str("service protocol failure"),
            Self::Resources => formatter.write_str("service resource failure"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum SystemdKernelFailure {
    OutOfMemory,
}

impl std::fmt::Display for SystemdKernelFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfMemory => formatter.write_str("kernel out-of-memory kill"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum SystemdFailure {
    Process(SystemdProcessFailure),
    Manager(SystemdManagerFailure),
    Kernel(SystemdKernelFailure),
    UnknownResult(String),
    StartLimit {
        last_process: Option<SystemdProcessFailure>,
    },
}

impl std::fmt::Display for SystemdFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process(failure) => failure.fmt(formatter),
            Self::Manager(failure) => failure.fmt(formatter),
            Self::Kernel(failure) => failure.fmt(formatter),
            Self::UnknownResult(result) => write!(formatter, "unknown systemd result {result}"),
            Self::StartLimit {
                last_process: Some(failure),
            } => write!(
                formatter,
                "start limit reached; last process failure: {failure}"
            ),
            Self::StartLimit { last_process: None } => formatter.write_str("start limit reached"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SystemdExecOutcome {
    Exited,
    Killed,
    Dumped,
}

#[derive(Debug, Eq, PartialEq)]
enum SystemdProcessFailure {
    ExitCode(i32),
    Signal { number: i32, core_dumped: bool },
}

impl std::fmt::Display for SystemdProcessFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExitCode(code) => write!(formatter, "exit code {code}"),
            Self::Signal {
                number,
                core_dumped: false,
            } => write!(formatter, "signal {number}"),
            Self::Signal {
                number,
                core_dumped: true,
            } => write!(formatter, "signal {number}, core dumped"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SystemdExecSnapshot {
    outcome: SystemdExecOutcome,
    status: i32,
}

impl SystemdExecSnapshot {
    fn parse(output: &str) -> Option<Self> {
        let code = property(output, "ExecMainCode")?.parse::<i32>().ok()?;
        let status = property(output, "ExecMainStatus")?.parse::<i32>().ok()?;
        Some(Self {
            outcome: SystemdExecOutcome::from_si_code(code)?,
            status,
        })
    }

    fn process_failure(self) -> Option<SystemdProcessFailure> {
        if self.status <= 0 {
            return None;
        }
        match self.outcome {
            SystemdExecOutcome::Exited => Some(SystemdProcessFailure::ExitCode(self.status)),
            SystemdExecOutcome::Killed => Some(SystemdProcessFailure::Signal {
                number: self.status,
                core_dumped: false,
            }),
            SystemdExecOutcome::Dumped => Some(SystemdProcessFailure::Signal {
                number: self.status,
                core_dumped: true,
            }),
        }
    }
}

impl SystemdExecOutcome {
    fn from_si_code(code: i32) -> Option<Self> {
        match code {
            SYSTEMD_EXEC_CODE_EXITED => Some(Self::Exited),
            SYSTEMD_EXEC_CODE_KILLED => Some(Self::Killed),
            SYSTEMD_EXEC_CODE_DUMPED => Some(Self::Dumped),
            _ => None,
        }
    }
}

pub(super) fn parse_systemd_record(output: &str) -> Result<SystemdRecord, SystemdStatusError> {
    let load = required_property(output, "LoadState")?;
    let active = required_property(output, "ActiveState")?;
    let sub = required_property(output, "SubState")?;
    let lifecycle = parse_systemd_lifecycle(load, active, sub);
    let autostart = if lifecycle == SystemdLifecycle::Missing {
        property(output, "UnitFileState")
            .map(parse_systemd_autostart)
            .unwrap_or(AutostartState::Unknown)
    } else {
        parse_systemd_autostart(required_property(output, "UnitFileState")?)
    };
    Ok(lifecycle.into_record(output, autostart))
}

#[cfg(test)]
fn parse_systemd_status(output: &str) -> Result<DaemonStatus, SystemdStatusError> {
    Ok(super::status_from_observation(
        parse_systemd_record(output)?.into_observation(false),
    ))
}

fn required_property<'a>(
    output: &'a str,
    property_name: &'static str,
) -> Result<&'a str, SystemdStatusError> {
    property(output, property_name).ok_or(SystemdStatusError::MissingProperty {
        property: property_name,
    })
}

fn property<'a>(output: &'a str, property_name: &str) -> Option<&'a str> {
    // systemctl show emits a flat property list. Nested manager dictionaries are intentionally not
    // accepted here because indentation and depth are outside this parser's type.
    output.lines().find_map(|line| {
        let (name, value) = line.trim().split_once('=')?;
        (name.trim() == property_name).then(|| value.trim())
    })
}

fn parse_systemd_lifecycle<'a>(
    load: &'a str,
    active: &'a str,
    sub: &'a str,
) -> SystemdLifecycle<'a> {
    // Verified with systemd 257.13, including auto-restart-queued and an active unlinked unit whose
    // LoadState is not-found. ActiveState is authoritative whenever a process lifecycle exists;
    // LoadState answers only whether an inactive unit is absent, unavailable, or stopped.
    match active {
        "active" | "reloading" | "refreshing" => SystemdLifecycle::Running,
        "failed" => SystemdLifecycle::Failed,
        "activating" if matches!(sub, "auto-restart" | "auto-restart-queued") => {
            SystemdLifecycle::AutoRestarting
        }
        "activating" => SystemdLifecycle::Starting,
        "deactivating" => SystemdLifecycle::Stopping,
        "inactive" => match load {
            "not-found" => SystemdLifecycle::Missing,
            "masked" | "bad-setting" | "error" => SystemdLifecycle::Unavailable,
            "loaded" | "stub" | "merged" => SystemdLifecycle::Stopped,
            _ => SystemdLifecycle::Other { load, active, sub },
        },
        _ => SystemdLifecycle::Other { load, active, sub },
    }
}

fn parse_systemd_failure(output: &str) -> Option<SystemdFailure> {
    let process = |expected| {
        SystemdExecSnapshot::parse(output)
            .filter(|snapshot| snapshot.outcome == expected)
            .and_then(SystemdExecSnapshot::process_failure)
            .map(SystemdFailure::Process)
    };
    match property(output, "Result") {
        Some("exit-code") => process(SystemdExecOutcome::Exited),
        Some("signal") => process(SystemdExecOutcome::Killed),
        Some("core-dump") => process(SystemdExecOutcome::Dumped),
        Some("timeout") => Some(SystemdFailure::Manager(SystemdManagerFailure::Timeout)),
        Some("watchdog") => Some(SystemdFailure::Manager(SystemdManagerFailure::Watchdog)),
        Some("oom-kill") => Some(SystemdFailure::Kernel(SystemdKernelFailure::OutOfMemory)),
        Some("start-limit-hit") => Some(SystemdFailure::StartLimit {
            last_process: SystemdExecSnapshot::parse(output)
                .and_then(SystemdExecSnapshot::process_failure),
        }),
        Some("protocol") => Some(SystemdFailure::Manager(SystemdManagerFailure::Protocol)),
        Some("resources") => Some(SystemdFailure::Manager(SystemdManagerFailure::Resources)),
        Some("success" | "") | None => None,
        Some(other) => Some(SystemdFailure::UnknownResult(other.to_owned())),
    }
}

fn project_systemd_failure(output: &str) -> Option<DaemonFailure> {
    parse_systemd_failure(output).map(|failure| DaemonFailure::from_display(&failure))
}

fn parse_systemd_autostart(output: &str) -> AutostartState {
    // Verified with systemd 257.13: static and generated units cannot be enabled. Linked, alias,
    // and indirect units are discoverable but not currently enabled for the default target.
    match output.trim() {
        "enabled" | "enabled-runtime" => AutostartState::Enabled,
        "disabled" | "linked" | "linked-runtime" | "alias" | "indirect" => AutostartState::Disabled,
        "static" | "generated" | "masked" | "masked-runtime" => AutostartState::Unavailable,
        _ => AutostartState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    //! Proves systemd snapshot reduction.

    use super::*;

    #[test]
    fn lifecycle_reduction_preserves_manager_states() {
        assert_eq!(
            parse_systemd_lifecycle("loaded", "active", "running"),
            SystemdLifecycle::Running
        );
        assert_eq!(
            parse_systemd_lifecycle("loaded", "reloading", "reload"),
            SystemdLifecycle::Running
        );
        assert_eq!(
            parse_systemd_lifecycle("loaded", "activating", "auto-restart-queued"),
            SystemdLifecycle::AutoRestarting
        );
        assert_eq!(
            parse_systemd_lifecycle("error", "inactive", "dead"),
            SystemdLifecycle::Unavailable
        );
    }

    #[test]
    fn status_requires_every_structural_property() {
        for property in ["LoadState", "ActiveState", "SubState", "UnitFileState"] {
            let output =
                "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\n"
                    .lines()
                    .filter(|line| !line.starts_with(property))
                    .collect::<Vec<_>>()
                    .join("\n");
            assert!(matches!(
                parse_systemd_status(&output),
                Err(SystemdStatusError::MissingProperty { property: missing }) if missing == property
            ));
        }
    }

    #[test]
    fn not_installed_has_no_synthetic_autostart_state() -> Result<(), SystemdStatusError> {
        let status =
            parse_systemd_status("LoadState=not-found\nActiveState=inactive\nSubState=dead\n")?;

        assert_eq!(status, DaemonStatus::NotInstalled);
        Ok(())
    }

    #[test]
    fn active_axis_keeps_every_load_state_running() -> Result<(), SystemdStatusError> {
        for (load, active, sub) in [
            ("not-found", "active", "running"),
            ("loaded", "reloading", "reload"),
            ("loaded", "refreshing", "reload"),
            ("stub", "active", "running"),
            ("merged", "active", "running"),
            ("bad-setting", "active", "running"),
            ("error", "active", "running"),
        ] {
            let status = parse_systemd_status(&format!(
                "LoadState={load}\nActiveState={active}\nSubState={sub}\nUnitFileState=linked\n"
            ))?;
            assert_eq!(
                status,
                DaemonStatus::installed(DaemonState::Running, AutostartState::Disabled)
            );
        }
        Ok(())
    }

    #[test]
    fn masked_units_require_runtime_evidence_before_claiming_a_known_lifecycle(
    ) -> Result<(), SystemdStatusError> {
        for (active, sub, state) in [
            ("active", "running", DaemonState::Running),
            ("refreshing", "reload", DaemonState::Running),
            ("failed", "failed", DaemonState::Failed(None)),
        ] {
            let status = parse_systemd_status(&format!(
                "LoadState=masked\nActiveState={active}\nSubState={sub}\nUnitFileState=masked\n"
            ))?;
            assert_eq!(
                status,
                DaemonStatus::installed(state, AutostartState::Unavailable)
            );
        }

        let masked_inactive = parse_systemd_status(
            "LoadState=masked\nActiveState=inactive\nSubState=dead\nUnitFileState=masked\n",
        )?;
        assert_eq!(
            masked_inactive,
            DaemonStatus::installed(DaemonState::Unavailable, AutostartState::Unavailable)
        );
        Ok(())
    }

    #[test]
    fn unknown_manager_vocabulary_remains_user_visible() -> Result<(), SystemdStatusError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=future-active\nSubState=future-sub\nUnitFileState=enabled\n",
        )?;

        assert_eq!(
            status.to_string(),
            "unknown (load state: loaded, active state: future-active, substate: future-sub)"
        );
        Ok(())
    }

    #[test]
    fn auto_restart_preserves_failure_without_becoming_terminal() -> Result<(), SystemdStatusError>
    {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n",
        )?;

        assert_eq!(
            status,
            DaemonStatus::installed(
                DaemonState::Restarting(Some(DaemonFailure::described("exit code 78"))),
                AutostartState::Enabled,
            )
        );
        Ok(())
    }

    #[test]
    fn post_stop_success_does_not_attribute_systemd_sigterm() -> Result<(), SystemdStatusError> {
        let status = parse_systemd_status(
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=15\nResult=success\n",
        )?;

        assert_eq!(
            status,
            DaemonStatus::installed(DaemonState::Stopped, AutostartState::Enabled)
        );
        Ok(())
    }

    #[test]
    fn manager_timeout_remains_authoritative_over_process_record() -> Result<(), SystemdStatusError>
    {
        let output = "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode=2\nExecMainStatus=9\nResult=timeout\n";
        let status = parse_systemd_status(output)?;

        assert_eq!(
            parse_systemd_failure(output),
            Some(SystemdFailure::Manager(SystemdManagerFailure::Timeout))
        );
        assert_eq!(status.to_string(), "failed (service-manager timeout)");
        Ok(())
    }

    #[test]
    fn recognized_process_results_use_matching_siginfo() {
        let cases = [
            (
                "exit-code",
                "1",
                "78",
                SystemdFailure::Process(SystemdProcessFailure::ExitCode(78)),
            ),
            (
                "signal",
                "2",
                "15",
                SystemdFailure::Process(SystemdProcessFailure::Signal {
                    number: 15,
                    core_dumped: false,
                }),
            ),
            (
                "core-dump",
                "3",
                "6",
                SystemdFailure::Process(SystemdProcessFailure::Signal {
                    number: 6,
                    core_dumped: true,
                }),
            ),
        ];
        for (result, code, process_status, failure) in cases {
            let output = format!(
                "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode={code}\nExecMainStatus={process_status}\nResult={result}\n"
            );
            assert_eq!(parse_systemd_failure(&output), Some(failure));
        }
    }

    #[test]
    fn unknown_result_preserves_unknown_provenance_without_siginfo(
    ) -> Result<(), SystemdStatusError> {
        let output = "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nResult=future-result\n";
        let status = parse_systemd_status(output)?;

        assert_eq!(
            parse_systemd_failure(output),
            Some(SystemdFailure::UnknownResult("future-result".to_owned()))
        );
        assert_eq!(
            status.to_string(),
            "failed (unknown systemd result future-result)"
        );
        Ok(())
    }

    #[test]
    fn oom_kill_has_kernel_provenance() {
        assert_eq!(
            parse_systemd_failure("Result=oom-kill\n"),
            Some(SystemdFailure::Kernel(SystemdKernelFailure::OutOfMemory))
        );
    }

    #[test]
    fn start_limit_composes_manager_verdict_with_the_last_process_failure() {
        let output = "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=start-limit-hit\n";

        assert_eq!(
            parse_systemd_failure(output),
            Some(SystemdFailure::StartLimit {
                last_process: Some(SystemdProcessFailure::ExitCode(78)),
            })
        );
        assert_eq!(
            project_systemd_failure(output).map(|failure| failure.to_string()),
            Some("start limit reached; last process failure: exit code 78".to_owned())
        );
    }

    #[test]
    fn auto_restart_remains_pending_when_definition_provenance_is_unknown(
    ) -> Result<(), SystemdStatusError> {
        let observation = parse_systemd_record(
            "LoadState=not-found\nActiveState=activating\nSubState=auto-restart\nUnitFileState=\nResult=success\n",
        )?
        .into_observation(false);

        assert!(!observation.settles_start_poll());
        Ok(())
    }

    #[test]
    fn missing_record_with_definition_uses_the_shared_pending_fold(
    ) -> Result<(), SystemdStatusError> {
        let observation = parse_systemd_record(
            "LoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\n",
        )?
        .into_observation(true);

        assert_eq!(
            observation,
            DaemonObservation::definition_without_record(AutostartState::Unknown)
        );
        assert!(!observation.settles_start_poll());
        Ok(())
    }

    #[test]
    fn loaded_inactive_record_is_terminally_stopped() -> Result<(), SystemdStatusError> {
        let observation = parse_systemd_record(
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nUnitFileState=enabled\n",
        )?
        .into_observation(true);

        assert!(observation.settles_start_poll());
        Ok(())
    }

    #[test]
    fn autostart_does_not_conflate_discovery_or_masking_with_enabled() {
        assert_eq!(parse_systemd_autostart("enabled"), AutostartState::Enabled);
        assert_eq!(
            parse_systemd_autostart("disabled"),
            AutostartState::Disabled
        );
        for state in ["linked", "linked-runtime", "alias", "indirect"] {
            assert_eq!(parse_systemd_autostart(state), AutostartState::Disabled);
        }
        for state in ["static", "generated", "masked"] {
            assert_eq!(parse_systemd_autostart(state), AutostartState::Unavailable);
        }
    }
}
