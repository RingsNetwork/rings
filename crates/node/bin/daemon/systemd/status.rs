//! Parses one `systemctl show` snapshot into lifecycle and autostart state. Unknown manager
//! vocabulary remains visible, and no parser result assumes the unit came from our renderer.

use thiserror::Error;

use super::super::AutostartState;
use super::super::DaemonFailure;
use super::super::DaemonObservation;
use super::super::DaemonState;
#[cfg(test)]
use super::super::DaemonStatus;
use super::super::DaemonTransition;
use super::super::StartPollDisposition;

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
    NotInstalled,
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
    /// Post: transient states remain pending unless systemd itself reports a terminal lifecycle;
    /// no disposition depends on fields from a unit definition that this parser cannot identify.
    fn into_observation(
        self,
        output: &str,
        autostart: AutostartState,
    ) -> DaemonObservation<AutostartState> {
        let (state, start_poll) = match self {
            Self::NotInstalled => return DaemonObservation::NotInstalled,
            Self::Running => (DaemonState::Running, StartPollDisposition::Settled),
            Self::Stopped => (DaemonState::Stopped, StartPollDisposition::Pending),
            Self::Failed => (
                DaemonState::Failed(project_systemd_failure(output)),
                StartPollDisposition::Settled,
            ),
            Self::Starting => (
                DaemonState::Transitioning(DaemonTransition::Starting),
                StartPollDisposition::Pending,
            ),
            // `systemctl show` does not prove which unit definition supplied RestartSec. A foreign
            // unit may use systemd's short default, so the observation budget must decide.
            Self::AutoRestarting => (
                DaemonState::Restarting(project_systemd_failure(output)),
                StartPollDisposition::Pending,
            ),
            Self::Stopping => (
                DaemonState::Transitioning(DaemonTransition::Stopping),
                StartPollDisposition::Pending,
            ),
            Self::Other { load, active, sub } => (
                DaemonState::Unknown(format!(
                    "load state: {load}, active state: {active}, substate: {sub}"
                )),
                StartPollDisposition::Pending,
            ),
        };
        DaemonObservation::installed(state, start_poll, autostart)
    }
}

#[derive(Debug, Eq, PartialEq)]
enum SystemdResult {
    Success,
    Process(SystemdExecOutcome),
    StartLimit,
    Manager(SystemdManagerFailure),
    Missing,
}

#[derive(Debug, Eq, PartialEq)]
enum SystemdManagerFailure {
    Timeout,
    Watchdog,
    OutOfMemory,
    Protocol,
    Resources,
    Other(String),
}

impl std::fmt::Display for SystemdManagerFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("service-manager timeout"),
            Self::Watchdog => formatter.write_str("watchdog failure"),
            Self::OutOfMemory => formatter.write_str("out-of-memory kill"),
            Self::Protocol => formatter.write_str("service protocol failure"),
            Self::Resources => formatter.write_str("service resource failure"),
            Self::Other(result) => write!(formatter, "service-manager result {result}"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum SystemdFailure {
    Process(SystemdProcessFailure),
    Manager(SystemdManagerFailure),
    StartLimit {
        last_process: Option<SystemdProcessFailure>,
    },
}

impl std::fmt::Display for SystemdFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process(failure) => failure.fmt(formatter),
            Self::Manager(failure) => failure.fmt(formatter),
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

impl SystemdResult {
    fn failure(self, output: &str) -> Option<SystemdFailure> {
        match self {
            Self::Process(outcome) => Some(SystemdFailure::Process(
                outcome
                    .failure_from(output)
                    .unwrap_or_else(|| outcome.unavailable()),
            )),
            Self::StartLimit => Some(SystemdFailure::StartLimit {
                last_process: SystemdExecOutcome::failure_from_snapshot(output),
            }),
            Self::Manager(failure) => Some(SystemdFailure::Manager(failure)),
            Self::Success | Self::Missing => None,
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
    Unavailable(SystemdExecOutcome),
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
            Self::Unavailable(SystemdExecOutcome::Exited) => {
                formatter.write_str("process exited with an unavailable exit code")
            }
            Self::Unavailable(SystemdExecOutcome::Killed) => {
                formatter.write_str("process was killed by an unavailable signal")
            }
            Self::Unavailable(SystemdExecOutcome::Dumped) => {
                formatter.write_str("process dumped core from an unavailable signal")
            }
        }
    }
}

impl SystemdExecOutcome {
    fn si_code(self) -> i32 {
        match self {
            Self::Exited => SYSTEMD_EXEC_CODE_EXITED,
            Self::Killed => SYSTEMD_EXEC_CODE_KILLED,
            Self::Dumped => SYSTEMD_EXEC_CODE_DUMPED,
        }
    }

    fn unavailable(self) -> SystemdProcessFailure {
        SystemdProcessFailure::Unavailable(self)
    }

    fn failure_from(self, output: &str) -> Option<SystemdProcessFailure> {
        let code = property(output, "ExecMainCode")?.parse::<i32>().ok()?;
        let status = property(output, "ExecMainStatus")?.parse::<i32>().ok()?;
        if code != self.si_code() || status <= 0 {
            return None;
        }
        match self {
            Self::Exited => Some(SystemdProcessFailure::ExitCode(status)),
            Self::Killed => Some(SystemdProcessFailure::Signal {
                number: status,
                core_dumped: false,
            }),
            Self::Dumped => Some(SystemdProcessFailure::Signal {
                number: status,
                core_dumped: true,
            }),
        }
    }

    fn failure_from_snapshot(output: &str) -> Option<SystemdProcessFailure> {
        let code = property(output, "ExecMainCode")?.parse::<i32>().ok()?;
        match code {
            SYSTEMD_EXEC_CODE_EXITED => Self::Exited.failure_from(output),
            SYSTEMD_EXEC_CODE_KILLED => Self::Killed.failure_from(output),
            SYSTEMD_EXEC_CODE_DUMPED => Self::Dumped.failure_from(output),
            _ => None,
        }
    }
}

pub(super) fn parse_systemd_observation(
    output: &str,
) -> Result<DaemonObservation<AutostartState>, SystemdStatusError> {
    let load = required_property(output, "LoadState")?;
    let active = required_property(output, "ActiveState")?;
    let sub = required_property(output, "SubState")?;
    let lifecycle = parse_systemd_lifecycle(load, active, sub);
    if lifecycle == SystemdLifecycle::NotInstalled {
        return Ok(DaemonObservation::NotInstalled);
    }
    let autostart = parse_systemd_autostart(required_property(output, "UnitFileState")?);
    Ok(lifecycle.into_observation(output, autostart))
}

#[cfg(test)]
fn parse_systemd_status(output: &str) -> Result<DaemonStatus, SystemdStatusError> {
    Ok(parse_systemd_observation(output)?.into_status())
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
    // LoadState is not-found. Refreshing is an active state since systemd 254. Masking changes
    // loadability, not whether an already-loaded process is active. Unknown tuples remain visible.
    match (load, active, sub) {
        ("not-found" | "masked", "inactive", _) => SystemdLifecycle::NotInstalled,
        ("loaded" | "not-found" | "masked", "active" | "reloading" | "refreshing", _) => {
            SystemdLifecycle::Running
        }
        ("loaded", "inactive", _) => SystemdLifecycle::Stopped,
        ("loaded" | "not-found" | "masked", "failed", _) => SystemdLifecycle::Failed,
        (
            "loaded" | "not-found" | "masked",
            "activating",
            "auto-restart" | "auto-restart-queued",
        ) => SystemdLifecycle::AutoRestarting,
        ("loaded" | "not-found" | "masked", "activating", _) => SystemdLifecycle::Starting,
        ("loaded" | "not-found" | "masked", "deactivating", _) => SystemdLifecycle::Stopping,
        _ => SystemdLifecycle::Other { load, active, sub },
    }
}

fn parse_systemd_failure(output: &str) -> Option<SystemdFailure> {
    parse_systemd_result(property(output, "Result")).failure(output)
}

fn project_systemd_failure(output: &str) -> Option<DaemonFailure> {
    parse_systemd_failure(output).map(|failure| DaemonFailure::from_display(&failure))
}

fn parse_systemd_result(result: Option<&str>) -> SystemdResult {
    match result {
        Some("success") => SystemdResult::Success,
        Some("exit-code") => SystemdResult::Process(SystemdExecOutcome::Exited),
        Some("signal") => SystemdResult::Process(SystemdExecOutcome::Killed),
        Some("core-dump") => SystemdResult::Process(SystemdExecOutcome::Dumped),
        Some("timeout") => SystemdResult::Manager(SystemdManagerFailure::Timeout),
        Some("watchdog") => SystemdResult::Manager(SystemdManagerFailure::Watchdog),
        Some("oom-kill") => SystemdResult::Manager(SystemdManagerFailure::OutOfMemory),
        Some("start-limit-hit") => SystemdResult::StartLimit,
        Some("protocol") => SystemdResult::Manager(SystemdManagerFailure::Protocol),
        Some("resources") => SystemdResult::Manager(SystemdManagerFailure::Resources),
        Some("") | None => SystemdResult::Missing,
        Some(other) => SystemdResult::Manager(SystemdManagerFailure::Other(other.to_owned())),
    }
}

fn parse_systemd_autostart(output: &str) -> AutostartState {
    // Verified with systemd 257.13: linked, alias, static, indirect, and generated units have no
    // default-target enablement owned by this command, so they project to login autostart disabled.
    match output.trim() {
        "enabled" | "enabled-runtime" => AutostartState::Enabled,
        "disabled" | "linked" | "linked-runtime" | "alias" | "static" | "indirect"
        | "generated" => AutostartState::Disabled,
        "masked" | "masked-runtime" => AutostartState::Other("unavailable"),
        _ => AutostartState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    //! Witnesses systemd snapshot parsing, finite lifecycle reduction, and failure composition.

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
        assert!(matches!(
            parse_systemd_lifecycle("error", "inactive", "dead"),
            SystemdLifecycle::Other { .. }
        ));
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
    fn detached_and_refreshing_units_remain_running() -> Result<(), SystemdStatusError> {
        for (load, active, sub) in [
            ("not-found", "active", "running"),
            ("loaded", "reloading", "reload"),
            ("loaded", "refreshing", "reload"),
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
                DaemonStatus::installed(state, AutostartState::Other("unavailable"))
            );
        }

        let masked_inactive =
            parse_systemd_status("LoadState=masked\nActiveState=inactive\nSubState=dead\n")?;
        assert_eq!(masked_inactive, DaemonStatus::NotInstalled);
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
    fn unknown_result_preserves_manager_vocabulary_without_siginfo(
    ) -> Result<(), SystemdStatusError> {
        let output = "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nResult=future-result\n";
        let status = parse_systemd_status(output)?;

        assert_eq!(
            parse_systemd_failure(output),
            Some(SystemdFailure::Manager(SystemdManagerFailure::Other(
                "future-result".to_owned(),
            )))
        );
        assert_eq!(
            status.to_string(),
            "failed (service-manager result future-result)"
        );
        Ok(())
    }

    #[test]
    fn known_result_survives_a_mismatched_or_invalid_siginfo_record() {
        for (result, code, process_status, expected) in [
            ("exit-code", "2", "78", SystemdExecOutcome::Exited),
            ("exit-code", "1", "0", SystemdExecOutcome::Exited),
            ("signal", "3", "9", SystemdExecOutcome::Killed),
            ("core-dump", "2", "6", SystemdExecOutcome::Dumped),
            ("signal", "invalid", "9", SystemdExecOutcome::Killed),
            ("signal", "2", "invalid", SystemdExecOutcome::Killed),
        ] {
            let output = format!(
                "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\nExecMainCode={code}\nExecMainStatus={process_status}\nResult={result}\n"
            );
            assert_eq!(
                parse_systemd_failure(&output),
                Some(SystemdFailure::Process(SystemdProcessFailure::Unavailable(
                    expected
                )))
            );
        }
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
        use super::super::super::StartPollObservation;

        let observation = parse_systemd_observation(
            "LoadState=not-found\nActiveState=activating\nSubState=auto-restart\nUnitFileState=\nResult=success\n",
        )?;

        assert!(!observation.settles_start_poll());
        Ok(())
    }

    #[test]
    fn autostart_does_not_conflate_discovery_or_masking_with_enabled() {
        assert_eq!(parse_systemd_autostart("enabled"), AutostartState::Enabled);
        assert_eq!(
            parse_systemd_autostart("disabled"),
            AutostartState::Disabled
        );
        for state in [
            "linked",
            "linked-runtime",
            "alias",
            "static",
            "indirect",
            "generated",
        ] {
            assert_eq!(parse_systemd_autostart(state), AutostartState::Disabled);
        }
        assert_eq!(
            parse_systemd_autostart("masked"),
            AutostartState::Other("unavailable")
        );
    }
}
