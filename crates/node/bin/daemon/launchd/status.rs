use super::super::AutostartState;
use super::super::DaemonState;
use super::super::FailureBoundary;
use super::LAUNCHD_LABEL;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TerminationAttribution {
    CurrentStatus,
    Action(FailureBoundary),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaunchdTermination {
    ExitCode(i32),
    Signal(i32),
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
    fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped)
    }
}

impl TerminationAttribution {
    fn accepts(
        self,
        lifecycle: LaunchdLifecycle<'_>,
        observed_sequence: Option<u64>,
        termination: LaunchdTermination,
    ) -> bool {
        match termination {
            LaunchdTermination::ExitCode(code) => {
                code != 0 && self.accepts_exit_code(observed_sequence)
            }
            LaunchdTermination::Signal(signal) => signal > 0 && self.accepts_signal(lifecycle),
        }
    }

    fn accepts_exit_code(self, observed_sequence: Option<u64>) -> bool {
        match self {
            // Status chooses to trust the recorded exit. An unambiguous action knows history was
            // reset. These are distinct propositions that intentionally reach the same decision.
            Self::CurrentStatus | Self::Action(FailureBoundary::Unambiguous) => true,
            Self::Action(FailureBoundary::PostAction {
                sequence: Some(baseline),
            }) => {
                // Observed on macOS 15.6.1 (24G90): `kickstart -k` terminated the previous run by
                // signal, and its signal record had displaced the numeric exit record by the first
                // observation where `runs` advanced. A numeric exit beyond that boundary therefore
                // belongs to a later run. `runs` is only monotonic; one action advanced it twice.
                observed_sequence.is_some_and(|sequence| sequence > baseline)
            }
            Self::Action(FailureBoundary::PostAction { sequence: None }) => false,
        }
    }

    fn accepts_signal(self, lifecycle: LaunchdLifecycle<'_>) -> bool {
        match self {
            // A fresh bootstrap has no action-created termination history, so its signal belongs
            // to the new instance even while launchd has already scheduled another spawn.
            Self::Action(FailureBoundary::Unambiguous) => true,
            // Plain status requires a terminal state because a signal on `spawn scheduled` is
            // ambiguous. A post-kickstart action uses the same guard for a different reason: the
            // signal can belong to the instance killed by the action itself.
            Self::CurrentStatus
            | Self::Action(FailureBoundary::PostAction { sequence: Some(_) }) => {
                lifecycle.is_terminal()
            }
            // Without a sequence baseline, post-action reporting intentionally degrades to state
            // alone rather than attributing possibly self-inflicted termination history.
            Self::Action(FailureBoundary::PostAction { sequence: None }) => false,
        }
    }
}

fn launchd_value<'a>(output: &'a str, field: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix(field)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn parse_launchd_lifecycle(output: &str) -> LaunchdLifecycle<'_> {
    match launchd_value(output, "state = ") {
        Some("running") => LaunchdLifecycle::Running,
        Some("spawn scheduled") => LaunchdLifecycle::SpawnScheduled,
        Some("waiting" | "exited" | "not running") => LaunchdLifecycle::Stopped,
        Some("throttled") => LaunchdLifecycle::Throttled,
        Some(other) => LaunchdLifecycle::Other(other),
        None => LaunchdLifecycle::Missing,
    }
}

pub(super) fn parse_launchd_runs(output: &str) -> Option<u64> {
    launchd_value(output, "runs = ")?.parse().ok()
}

fn parse_launchd_termination(output: &str) -> Option<LaunchdTermination> {
    let exit_code = launchd_value(output, "last exit code = ")
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok());
    if exit_code.is_some() {
        return exit_code.map(LaunchdTermination::ExitCode);
    }
    launchd_value(output, "last terminating signal = ")?
        .split_whitespace()
        .next_back()?
        .parse()
        .ok()
        .map(LaunchdTermination::Signal)
}

pub(super) fn parse_launchd_state(
    output: &str,
    termination_attribution: TerminationAttribution,
) -> DaemonState {
    let lifecycle = parse_launchd_lifecycle(output);
    let has_attributed_failure = parse_launchd_termination(output).is_some_and(|termination| {
        termination_attribution.accepts(lifecycle, parse_launchd_runs(output), termination)
    });
    match lifecycle {
        LaunchdLifecycle::Running => DaemonState::Running,
        LaunchdLifecycle::SpawnScheduled if has_attributed_failure => DaemonState::Failed,
        LaunchdLifecycle::SpawnScheduled => DaemonState::Starting,
        LaunchdLifecycle::Stopped if has_attributed_failure => DaemonState::Failed,
        LaunchdLifecycle::Stopped => DaemonState::Stopped,
        LaunchdLifecycle::Throttled => DaemonState::Failed,
        LaunchdLifecycle::Other(other) => DaemonState::Unknown(other.to_owned()),
        LaunchdLifecycle::Missing => {
            DaemonState::Unknown("loaded without a state field".to_owned())
        }
    }
}

pub(super) fn parse_launchd_autostart(output: &str) -> AutostartState {
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
        if label.trim().trim_matches('"') != LAUNCHD_LABEL {
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
    use super::*;

    #[test]
    fn state_parser_preserves_launchd_lifecycle_and_exit_states() {
        assert_eq!(
            parse_launchd_state("state = running\n", TerminationAttribution::CurrentStatus),
            DaemonState::Running
        );
        assert_eq!(
            parse_launchd_state("state = waiting\n", TerminationAttribution::CurrentStatus),
            DaemonState::Stopped
        );
        assert_eq!(
            parse_launchd_state(
                "state = throttled\nlast exit code = 78\n",
                TerminationAttribution::CurrentStatus,
            ),
            DaemonState::Failed
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nlast exit code = 1\n",
                TerminationAttribution::CurrentStatus,
            ),
            DaemonState::Failed
        );
        assert_eq!(
            parse_launchd_state(
                "state = exited\nlast exit code = 0\n",
                TerminationAttribution::CurrentStatus,
            ),
            DaemonState::Stopped
        );
    }

    #[test]
    fn state_parser_assigns_only_newer_numeric_exit_to_restart_attempt() {
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nlast exit code = 1\n",
                TerminationAttribution::Action(FailureBoundary::Unambiguous),
            ),
            DaemonState::Failed
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nlast exit code = 9\n",
                TerminationAttribution::Action(FailureBoundary::PostAction { sequence: None }),
            ),
            DaemonState::Starting
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nruns = 4\nlast exit code = 9\n",
                TerminationAttribution::Action(FailureBoundary::PostAction { sequence: Some(4) }),
            ),
            DaemonState::Starting
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nruns = 5\nlast exit code = 1\n",
                TerminationAttribution::Action(FailureBoundary::PostAction { sequence: Some(4) }),
            ),
            DaemonState::Failed
        );
    }

    #[test]
    fn state_parser_attributes_signals_by_action_provenance_and_lifecycle() {
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nruns = 1\nlast terminating signal = Segmentation fault: 11\n",
                TerminationAttribution::Action(FailureBoundary::Unambiguous),
            ),
            DaemonState::Failed
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nruns = 4\nlast terminating signal = Terminated: 15\n",
                TerminationAttribution::CurrentStatus,
            ),
            DaemonState::Starting
        );
        assert_eq!(
            parse_launchd_state(
                "state = exited\nruns = 4\nlast terminating signal = Bus error: 10\n",
                TerminationAttribution::CurrentStatus,
            ),
            DaemonState::Failed
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nruns = 4\nlast terminating signal = Terminated: 15\n",
                TerminationAttribution::Action(FailureBoundary::PostAction { sequence: Some(3) }),
            ),
            DaemonState::Starting
        );
        assert_eq!(
            parse_launchd_state(
                "state = spawn scheduled\nruns = 5\nlast terminating signal = Segmentation fault: 11\n",
                TerminationAttribution::Action(FailureBoundary::PostAction { sequence: Some(3) }),
            ),
            DaemonState::Starting
        );
        assert_eq!(
            parse_launchd_state(
                "state = waiting\nruns = 5\nlast terminating signal = Segmentation fault: 11\n",
                TerminationAttribution::Action(FailureBoundary::PostAction { sequence: Some(3) }),
            ),
            DaemonState::Failed
        );
        assert_eq!(
            parse_launchd_state(
                "state = waiting\nlast terminating signal = Segmentation fault: 11\n",
                TerminationAttribution::Action(FailureBoundary::PostAction { sequence: None }),
            ),
            DaemonState::Stopped
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
            parse_launchd_autostart("disabled services = {\n}"),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_launchd_autostart("\"io.ringsnetwork.node\" => disabled"),
            AutostartState::Disabled
        );
        assert_eq!(
            parse_launchd_autostart("\"io.ringsnetwork.node\" => enabled"),
            AutostartState::Enabled
        );
        assert_eq!(
            parse_launchd_autostart("\"io.ringsnetwork.node\" => malformed"),
            AutostartState::Unknown
        );
        assert_eq!(
            parse_launchd_autostart("unrecognized launchctl output"),
            AutostartState::Unknown
        );
    }
}
