//! Pure launchd lifecycle parsing and failure-attribution policy.

use super::super::command_output_value;
use super::super::AutostartState;
use super::super::DaemonFailure;
use super::super::DaemonObservation;
use super::super::DaemonState;

const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LaunchdAttribution {
    /// Attribute the current record without suppressing action-shaped termination data.
    Unfiltered,
    /// Attribute only history newer than the observed pre-restart run counter.
    SinceRun(u64),
    /// Do not attribute history when launchd did not expose a run-counter baseline.
    Unattributable,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum LaunchdRecord<T> {
    Missing,
    Loaded(T),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaunchdTermination<'a> {
    ExitCode(i32),
    Signal { name: Option<&'a str>, number: i32 },
}

impl LaunchdTermination<'_> {
    fn failure(self) -> Option<DaemonFailure> {
        match self {
            Self::ExitCode(0) => None,
            Self::ExitCode(code) => Some(DaemonFailure::ExitCode(code)),
            Self::Signal { number, .. } if number <= 0 => None,
            Self::Signal { name, number } => Some(DaemonFailure::Signal {
                name: name.map(str::to_owned),
                number,
                core_dumped: false,
            }),
        }
    }

    fn may_originate_from_restart_action(self) -> bool {
        match self {
            // A SIGTERM handler may translate the action into any numeric exit code.
            Self::ExitCode(_) => true,
            Self::Signal { number, .. } => matches!(number, SIGKILL | SIGTERM),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaunchdLifecycle<'a> {
    Running,
    SpawnScheduled,
    Stopped,
    Throttled,
    Other(&'a str),
    Missing,
}

impl LaunchdLifecycle<'_> {
    fn into_state(
        self,
        observed_sequence: Option<u64>,
        attributed_failure: Option<DaemonFailure>,
    ) -> DaemonState {
        match self {
            Self::Running => DaemonState::Running,
            Self::SpawnScheduled => match (attributed_failure, observed_sequence) {
                (Some(failure), _) => DaemonState::Restarting(Some(failure)),
                (None, Some(sequence)) if sequence > 0 => DaemonState::Restarting(None),
                (None, _) => DaemonState::Starting,
            },
            Self::Stopped => attributed_failure
                .map(|failure| DaemonState::Failed(Some(failure)))
                .unwrap_or(DaemonState::Stopped),
            // Throttled means launchd has delayed, not abandoned, the next spawn.
            Self::Throttled => DaemonState::Restarting(attributed_failure),
            Self::Other(other) => DaemonState::Unknown(other.to_owned()),
            Self::Missing => DaemonState::Unknown("loaded without a state field".to_owned()),
        }
    }
}

impl LaunchdAttribution {
    fn attributes(
        self,
        observed_sequence: Option<u64>,
        termination: LaunchdTermination<'_>,
    ) -> bool {
        match self {
            Self::Unfiltered => true,
            // Observed on macOS 15.6.1 (24G90): one kickstart action can advance `runs` twice.
            // Sequence advancement alone therefore cannot distinguish an action-translated exit
            // or action signal from a new-instance failure. Only non-action signals are attributable.
            Self::SinceRun(baseline) => {
                observed_sequence.is_some_and(|sequence| sequence > baseline)
                    && !termination.may_originate_from_restart_action()
            }
            // Without a baseline, post-action reporting degrades to lifecycle alone.
            Self::Unattributable => false,
        }
    }
}

fn launchd_value<'a>(output: &'a str, field: &str) -> Option<&'a str> {
    command_output_value(output, field).filter(|value| !value.is_empty())
}

fn parse_launchd_lifecycle(output: &str) -> LaunchdLifecycle<'_> {
    // Observed on macOS 15.6.1 (24G90). Unknown vocabulary remains visible to the user.
    match launchd_value(output, "state") {
        Some("running") => LaunchdLifecycle::Running,
        Some("spawn scheduled") => LaunchdLifecycle::SpawnScheduled,
        Some("waiting" | "exited" | "not running") => LaunchdLifecycle::Stopped,
        Some("throttled") => LaunchdLifecycle::Throttled,
        Some(other) => LaunchdLifecycle::Other(other),
        None => LaunchdLifecycle::Missing,
    }
}

fn parse_launchd_runs(output: &str) -> Option<u64> {
    launchd_value(output, "runs")?.parse().ok()
}

fn parse_launchd_exit(output: &str) -> Option<LaunchdTermination<'_>> {
    launchd_value(output, "last exit code")
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .map(LaunchdTermination::ExitCode)
}

fn parse_launchd_signal(output: &str) -> Option<LaunchdTermination<'_>> {
    let value = launchd_value(output, "last terminating signal")?;
    let (name, number) = value
        .rsplit_once(':')
        .map_or((None, value), |(name, number)| {
            let name = name.trim();
            ((!name.is_empty()).then_some(name), number)
        });
    Some(LaunchdTermination::Signal {
        name,
        number: number.trim().parse().ok()?,
    })
}

fn parse_launchd_termination(output: &str) -> Option<LaunchdTermination<'_>> {
    // Observed on macOS 15.6.1 (24G90): the fields are mutually exclusive in normal output, and a
    // signal displaces the prior exit record. Empty or malformed signal presence must still prevent
    // fallback from reactivating that stale exit record.
    if command_output_value(output, "last terminating signal").is_some() {
        parse_launchd_signal(output)
    } else {
        parse_launchd_exit(output)
    }
}

pub(super) fn attribution_after_restart(output: &str) -> LaunchdAttribution {
    parse_launchd_runs(output)
        .map(LaunchdAttribution::SinceRun)
        .unwrap_or(LaunchdAttribution::Unattributable)
}

pub(super) fn parse_launchd_observation(
    record: LaunchdRecord<&str>,
    definition_present: bool,
    attribution: LaunchdAttribution,
) -> DaemonObservation {
    let LaunchdRecord::Loaded(output) = record else {
        return if definition_present {
            DaemonObservation::Installed(DaemonState::Stopped)
        } else {
            DaemonObservation::NotInstalled
        };
    };
    let lifecycle = parse_launchd_lifecycle(output);
    let observed_sequence = parse_launchd_runs(output);
    let attributed_failure = parse_launchd_termination(output)
        .and_then(|termination| termination.failure().map(|failure| (termination, failure)))
        .filter(|(termination, _)| attribution.attributes(observed_sequence, *termination))
        .map(|(_, failure)| failure);
    let state = lifecycle.into_state(observed_sequence, attributed_failure);
    DaemonObservation::Installed(state)
}

pub(super) fn parse_launchd_autostart(output: &str, service_label: &str) -> AutostartState {
    // Observed on macOS 15.6.1 (24G90): launchctl emits both boolean and enabled/disabled
    // vocabularies. A recognized listing that omits this label means it is enabled.
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
        if label.trim().trim_matches('"') != service_label {
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
    use super::super::model::LAUNCHD_LABEL;
    use super::*;

    fn parse_state(output: &str, attribution: LaunchdAttribution) -> DaemonState {
        match parse_launchd_observation(LaunchdRecord::Loaded(output), true, attribution) {
            DaemonObservation::Installed(state) => state,
            DaemonObservation::NotInstalled => unreachable!("output always represents a job"),
        }
    }

    fn signal_failure(name: &str, number: i32) -> DaemonState {
        DaemonState::Failed(Some(DaemonFailure::Signal {
            name: Some(name.to_owned()),
            number,
            core_dumped: false,
        }))
    }

    fn restarting_exit_failure(code: i32) -> DaemonState {
        DaemonState::Restarting(Some(DaemonFailure::ExitCode(code)))
    }

    fn restarting_signal_failure(name: &str, number: i32) -> DaemonState {
        DaemonState::Restarting(Some(DaemonFailure::Signal {
            name: Some(name.to_owned()),
            number,
            core_dumped: false,
        }))
    }

    fn unnamed_signal_failure(number: i32) -> DaemonState {
        DaemonState::Failed(Some(DaemonFailure::Signal {
            name: None,
            number,
            core_dumped: false,
        }))
    }

    #[test]
    fn state_parser_preserves_launchd_lifecycle_and_exit_states() {
        assert_eq!(
            parse_state("state = running\n", LaunchdAttribution::Unfiltered),
            DaemonState::Running
        );
        assert_eq!(
            parse_state("state = waiting\n", LaunchdAttribution::Unfiltered),
            DaemonState::Stopped
        );
        assert_eq!(
            parse_state(
                "state = throttled\nlast exit code = 78\n",
                LaunchdAttribution::Unfiltered,
            ),
            restarting_exit_failure(78)
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nlast exit code = 1\n",
                LaunchdAttribution::Unfiltered,
            ),
            restarting_exit_failure(1)
        );
        assert_eq!(
            parse_state(
                "state = exited\nlast exit code = 0\n",
                LaunchdAttribution::Unfiltered,
            ),
            DaemonState::Stopped
        );
    }

    #[test]
    fn post_action_numeric_exit_remains_unattributable() {
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nlast exit code = 1\n",
                LaunchdAttribution::Unfiltered,
            ),
            restarting_exit_failure(1)
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nlast exit code = 9\n",
                LaunchdAttribution::Unattributable,
            ),
            DaemonState::Starting
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 4\nlast exit code = 9\n",
                LaunchdAttribution::SinceRun(4),
            ),
            DaemonState::Restarting(None)
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 5\nlast exit code = 1\n",
                LaunchdAttribution::SinceRun(4),
            ),
            DaemonState::Restarting(None)
        );
    }

    #[test]
    fn state_parser_attributes_signals_by_action_provenance() {
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 1\nlast terminating signal = Segmentation fault: 11\n",
                LaunchdAttribution::Unfiltered,
            ),
            restarting_signal_failure("Segmentation fault", 11)
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 4\nlast terminating signal = Terminated: 15\n",
                LaunchdAttribution::Unfiltered,
            ),
            restarting_signal_failure("Terminated", 15)
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 4\nlast terminating signal = Segmentation fault: 11\n",
                LaunchdAttribution::Unfiltered,
            ),
            restarting_signal_failure("Segmentation fault", 11)
        );
        assert_eq!(
            parse_state(
                "state = exited\nruns = 4\nlast terminating signal = Bus error: 10\n",
                LaunchdAttribution::Unfiltered,
            ),
            signal_failure("Bus error", 10)
        );
        assert_eq!(
            parse_state(
                "state = throttled\nruns = 4\nlast terminating signal = Terminated: 15\n",
                LaunchdAttribution::Unfiltered,
            ),
            restarting_signal_failure("Terminated", 15)
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 4\nlast terminating signal = Terminated: 15\n",
                LaunchdAttribution::SinceRun(3),
            ),
            DaemonState::Restarting(None)
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 3\nlast terminating signal = Segmentation fault: 11\n",
                LaunchdAttribution::SinceRun(3),
            ),
            DaemonState::Restarting(None)
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 5\nlast terminating signal = Segmentation fault: 11\n",
                LaunchdAttribution::SinceRun(3),
            ),
            restarting_signal_failure("Segmentation fault", 11)
        );
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 5\nlast terminating signal = Killed: 9\n",
                LaunchdAttribution::SinceRun(3),
            ),
            DaemonState::Restarting(None)
        );
        assert_eq!(
            parse_state(
                "state = waiting\nruns = 5\nlast terminating signal = Segmentation fault: 11\n",
                LaunchdAttribution::SinceRun(3),
            ),
            signal_failure("Segmentation fault", 11)
        );
        assert_eq!(
            parse_state(
                "state = waiting\nruns = 4\nlast terminating signal = Terminated: 15\n",
                LaunchdAttribution::SinceRun(3),
            ),
            DaemonState::Stopped
        );
        assert_eq!(
            parse_state(
                "state = waiting\nlast terminating signal = Segmentation fault: 11\n",
                LaunchdAttribution::Unattributable,
            ),
            DaemonState::Stopped
        );
    }

    #[test]
    fn throttled_restart_does_not_attribute_its_own_sigterm() {
        assert_eq!(
            parse_state(
                "state = throttled\nruns = 5\nlast terminating signal = Terminated: 15\n",
                LaunchdAttribution::SinceRun(3),
            ),
            DaemonState::Restarting(None)
        );
    }

    #[test]
    fn restart_attribution_and_missing_service_triage_are_pure() {
        assert_eq!(
            attribution_after_restart("state = running\nruns = 7\n"),
            LaunchdAttribution::SinceRun(7)
        );
        assert_eq!(
            attribution_after_restart("state = running\n"),
            LaunchdAttribution::Unattributable
        );
        assert_eq!(
            parse_launchd_observation(LaunchdRecord::Missing, true, LaunchdAttribution::Unfiltered,),
            DaemonObservation::Installed(DaemonState::Stopped)
        );
        assert_eq!(
            parse_launchd_observation(
                LaunchdRecord::Missing,
                false,
                LaunchdAttribution::Unfiltered,
            ),
            DaemonObservation::NotInstalled
        );
    }

    #[test]
    fn current_status_attributes_external_sigkill() {
        assert_eq!(
            parse_state(
                "state = throttled\nruns = 5\nlast terminating signal = Killed: 9\n",
                LaunchdAttribution::Unfiltered,
            ),
            restarting_signal_failure("Killed", 9)
        );
    }

    #[test]
    fn signal_record_wins_if_launchd_exposes_both_termination_fields() {
        assert_eq!(
            parse_launchd_termination(
                "last exit code = 1\nlast terminating signal = Segmentation fault: 11\n"
            ),
            Some(LaunchdTermination::Signal {
                name: Some("Segmentation fault"),
                number: 11,
            })
        );
    }

    #[test]
    fn unnamed_numeric_signal_is_preserved_as_a_failure_cause() {
        let expected = unnamed_signal_failure(11);
        let state = parse_state(
            "state = exited\nlast terminating signal = 11\n",
            LaunchdAttribution::Unfiltered,
        );

        assert_eq!(state, expected);
        assert_eq!(expected.to_string(), "failed (signal 11)");
    }

    #[test]
    fn malformed_newer_signal_does_not_reactivate_a_stale_exit_record() {
        for signal in ["malformed", ""] {
            assert_eq!(
                parse_launchd_termination(&format!(
                    "last exit code = 1\nlast terminating signal = {signal}\n"
                )),
                None
            );
        }
    }

    #[test]
    fn autostart_parser_matches_only_the_rings_service() {
        let output = r#"disabled services = {
    "unrelated.service" => true
    "io.ringsnetwork.node" => false
}"#;

        assert_eq!(
            parse_launchd_autostart(output, LAUNCHD_LABEL),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_launchd_autostart("disabled services = {}", LAUNCHD_LABEL),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_launchd_autostart("disabled services = {\n}", LAUNCHD_LABEL),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_launchd_autostart("\"io.ringsnetwork.node\" => disabled", LAUNCHD_LABEL,),
            AutostartState::Disabled
        );
        assert_eq!(
            parse_launchd_autostart("\"io.ringsnetwork.node\" => enabled", LAUNCHD_LABEL,),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_launchd_autostart("\"io.ringsnetwork.node\" => malformed", LAUNCHD_LABEL,),
            AutostartState::Unknown
        );
        assert_eq!(
            parse_launchd_autostart("unrecognized launchctl output", LAUNCHD_LABEL),
            AutostartState::Unknown
        );
    }
}
