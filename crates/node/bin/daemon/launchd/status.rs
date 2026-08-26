//! Reduces `launchctl print` output into the platform-neutral daemon observation.

use super::super::AutostartState;
use super::super::DaemonFailure;
use super::super::DaemonObservation;
#[cfg(test)]
use super::super::DaemonState;
use super::super::DaemonTransition;
use super::super::ObservedDaemonState;
use super::super::PendingDaemonState;

const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;
/// Selects which termination records may be projected after a manager action.
///
/// Law: `Unfiltered` accepts every valid failure record. `SinceRun(n)` accepts non-action-shaped
/// termination regardless of the counter and accepts action-shaped termination only after the
/// bounded `kickstart -k` sequence window. `Unattributable` accepts only termination that cannot
/// originate from the action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LaunchdAttribution {
    /// Attribute the current record without suppressing action-shaped termination data.
    Unfiltered,
    /// Attribute only history newer than the observed pre-restart run counter.
    SinceRun(u64),
    /// Attribute only failures whose shape excludes the current restart action.
    Unattributable,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum LaunchdRecord<T> {
    Missing,
    Loaded(T),
}

impl<T> LaunchdRecord<T> {
    pub(super) fn map<U>(self, transform: impl FnOnce(T) -> U) -> LaunchdRecord<U> {
        match self {
            Self::Missing => LaunchdRecord::Missing,
            Self::Loaded(value) => LaunchdRecord::Loaded(transform(value)),
        }
    }
}

/// Installation evidence available when launchd has no record for the label.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MissingRecordEvidence {
    None,
    DefinitionPresent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RespawnPolicy {
    KeepAliveOnUnsuccessfulExit,
    Unknown,
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
            Self::ExitCode(_) => Some(DaemonFailure::from_display(&self)),
            // Observed on macOS 15.6.1 (24G90): a non-positive signal field is launchd's absence
            // sentinel, not a process-termination cause.
            Self::Signal { number, .. } if number <= 0 => None,
            Self::Signal { .. } => Some(DaemonFailure::from_display(&self)),
        }
    }

    fn may_originate_from_restart_action(self) -> bool {
        match self {
            // A SIGTERM handler may translate the action into any numeric exit code.
            Self::ExitCode(_) => true,
            // Observed on macOS 15.6.1 (24G90): kickstart replacement can leave SIGTERM or
            // SIGKILL-shaped history, so neither alone proves an external failure.
            Self::Signal { number, .. } => matches!(number, SIGKILL | SIGTERM),
        }
    }
}

/// The three termination-history propositions needed by lifecycle reduction.
#[derive(Debug, Eq, PartialEq)]
enum AttributedTermination {
    /// No failure record exists, or the latest record is a clean exit.
    None,
    /// A failure record exists but is indistinguishable from the current restart action.
    SuppressedAsAction,
    /// A failure record is attributable to the managed process.
    Attributed(DaemonFailure),
}

impl AttributedTermination {
    fn projected_failure(self) -> Option<DaemonFailure> {
        match self {
            Self::Attributed(failure) => Some(failure),
            Self::None | Self::SuppressedAsAction => None,
        }
    }
}

impl std::fmt::Display for LaunchdTermination<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExitCode(code) => write!(formatter, "exit code {code}"),
            Self::Signal {
                name: Some(name),
                number,
            } => write!(formatter, "signal {name}: {number}"),
            Self::Signal { name: None, number } => write!(formatter, "signal {number}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaunchdLifecycle<'a> {
    Running,
    SpawnScheduled,
    Waiting,
    Exited,
    NotRunning,
    Throttled,
    Other(&'a str),
    Missing,
}

impl LaunchdLifecycle<'_> {
    fn into_observation(
        self,
        respawn_policy: RespawnPolicy,
        observed_sequence: Option<u64>,
        termination: AttributedTermination,
    ) -> DaemonObservation {
        let state = match self {
            Self::Running => ObservedDaemonState::running(),
            Self::SpawnScheduled => match (termination, observed_sequence) {
                (AttributedTermination::Attributed(failure), _) => {
                    ObservedDaemonState::pending(PendingDaemonState::Restarting(Some(failure)))
                }
                // Observed on macOS 15.6.1 (24G90): runs == 0 means the loaded job has never
                // spawned; a positive value means this is a respawn rather than its first start.
                (_, Some(sequence)) if sequence > 0 => {
                    ObservedDaemonState::pending(PendingDaemonState::Restarting(None))
                }
                (_, _) => ObservedDaemonState::pending(PendingDaemonState::Transitioning(
                    DaemonTransition::Starting,
                )),
            },
            // `exited` proves that a run completed. The rendered SuccessfulExit=false policy makes
            // a clean exit terminal and an unsuccessful exit eligible for respawn.
            Self::Exited => exited_observation(respawn_policy, termination),
            // `waiting` and `not running` can precede the first spawn, so neither is terminal even
            // when no termination history is present.
            Self::Waiting | Self::NotRunning => {
                ObservedDaemonState::pending(pending_non_running_state(respawn_policy, termination))
            }
            // Observed on macOS 15.6.1 (24G90): `throttled` means launchd scheduled a delayed
            // respawn. The record does not prove whether the delay fits this command's budget.
            Self::Throttled => ObservedDaemonState::pending(PendingDaemonState::Restarting(
                termination.projected_failure(),
            )),
            Self::Other(other) => {
                ObservedDaemonState::pending(PendingDaemonState::Unknown(other.to_owned()))
            }
            Self::Missing => ObservedDaemonState::pending(PendingDaemonState::Unknown(
                "loaded without a state field".to_owned(),
            )),
        };
        DaemonObservation::installed(state, ())
    }
}

fn exited_observation(
    respawn_policy: RespawnPolicy,
    termination: AttributedTermination,
) -> ObservedDaemonState {
    match (respawn_policy, termination) {
        (RespawnPolicy::KeepAliveOnUnsuccessfulExit, AttributedTermination::None) => {
            ObservedDaemonState::stopped()
        }
        (policy, termination) => {
            ObservedDaemonState::pending(pending_non_running_state(policy, termination))
        }
    }
}

fn pending_non_running_state(
    respawn_policy: RespawnPolicy,
    termination: AttributedTermination,
) -> PendingDaemonState {
    match (respawn_policy, termination) {
        (
            RespawnPolicy::KeepAliveOnUnsuccessfulExit,
            AttributedTermination::Attributed(failure),
        ) => PendingDaemonState::Restarting(Some(failure)),
        (RespawnPolicy::KeepAliveOnUnsuccessfulExit, AttributedTermination::SuppressedAsAction) => {
            PendingDaemonState::Restarting(None)
        }
        (RespawnPolicy::KeepAliveOnUnsuccessfulExit, AttributedTermination::None) => {
            PendingDaemonState::Stopped
        }
        (RespawnPolicy::Unknown, AttributedTermination::Attributed(failure)) => {
            PendingDaemonState::Unknown(format!(
                "respawn policy unknown; last process failure: {failure}"
            ))
        }
        (RespawnPolicy::Unknown, AttributedTermination::SuppressedAsAction) => {
            PendingDaemonState::Transitioning(DaemonTransition::Starting)
        }
        (RespawnPolicy::Unknown, AttributedTermination::None) => PendingDaemonState::Stopped,
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
            // Observed on macOS 15.6.1 (24G90): one kickstart action can advance `runs` twice and
            // leave SIGTERM, SIGKILL, or a translated numeric exit. Lifecycle vocabulary does not
            // shorten that ambiguity window; a non-action signal remains attributable immediately.
            Self::SinceRun(baseline) => {
                !termination.may_originate_from_restart_action()
                    || observed_sequence
                        .is_some_and(|sequence| sequence > baseline.saturating_add(2))
            }
            // A baseline is needed only for action-shaped history. Other termination shapes cannot
            // have been produced by kickstart replacement and remain attributable immediately.
            Self::Unattributable => !termination.may_originate_from_restart_action(),
        }
    }
}

fn launchd_value<'a>(output: &'a str, field: &str) -> Option<&'a str> {
    // Empty root properties count as absent. Their raw presence remains observable through
    // `launchd_property`, which prevents malformed signal fields from reviving stale exit fields.
    launchd_property(output, field).filter(|value| !value.is_empty())
}

#[derive(Clone, Copy)]
struct LaunchdProperty<'a> {
    depth: usize,
    parent: Option<&'a str>,
    name: &'a str,
    value: &'a str,
}

fn launchd_assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.trim().trim_end_matches(';');
    let (name, value) = line.split_once('=')?;
    let value = value.trim();
    let value = value.strip_prefix('>').map(str::trim).unwrap_or(value);
    Some((name.trim(), value))
}

fn launchd_properties(output: &str) -> Vec<LaunchdProperty<'_>> {
    let mut depth = 0usize;
    let mut dictionaries = Vec::new();
    let mut properties = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed == "}" {
            depth = depth.saturating_sub(1);
            dictionaries.pop();
            continue;
        }
        let Some((name, value)) = launchd_assignment(trimmed) else {
            continue;
        };
        properties.push(LaunchdProperty {
            depth,
            parent: dictionaries.last().copied(),
            name,
            value,
        });
        if value == "{" {
            depth = depth.saturating_add(1);
            dictionaries.push(name);
        }
    }
    properties
}

fn launchd_root_depth(properties: &[LaunchdProperty<'_>]) -> usize {
    usize::from(properties.first().is_some_and(|property| {
        property.depth == 0
            && property.value == "{"
            && !properties
                .iter()
                .skip(1)
                .any(|candidate| candidate.depth == 0)
    }))
}

fn launchd_property<'a>(output: &'a str, field: &str) -> Option<&'a str> {
    // Observed on macOS 15.6.1 (24G90): `launchctl print` wraps service properties in one root
    // dictionary and may emit nested dictionaries with repeated keys. Only the root property depth
    // is authoritative. Flat fixtures use depth zero and follow the same rule.
    let properties = launchd_properties(output);
    let root_depth = launchd_root_depth(&properties);
    properties
        .into_iter()
        .find(|property| {
            property.depth == root_depth && property.name == field && property.value != "{"
        })
        .map(|property| property.value)
}

fn launchd_nested_property<'a>(output: &'a str, dictionary: &str, field: &str) -> Option<&'a str> {
    let properties = launchd_properties(output);
    let root_depth = launchd_root_depth(&properties);
    let dictionary_depth = properties
        .iter()
        .find(|property| {
            property.depth == root_depth && property.name == dictionary && property.value == "{"
        })?
        .depth
        .saturating_add(1);
    properties
        .into_iter()
        .find(|property| {
            property.depth == dictionary_depth
                && property.parent == Some(dictionary)
                && property.name == field
        })
        .map(|property| property.value)
}

fn parse_launchd_lifecycle(output: &str) -> LaunchdLifecycle<'_> {
    // Observed on macOS 15.6.1 (24G90). Unknown vocabulary remains visible to the user.
    match launchd_value(output, "state") {
        Some("running") => LaunchdLifecycle::Running,
        Some("spawn scheduled") => LaunchdLifecycle::SpawnScheduled,
        Some("waiting") => LaunchdLifecycle::Waiting,
        Some("exited") => LaunchdLifecycle::Exited,
        Some("not running") => LaunchdLifecycle::NotRunning,
        Some("throttled") => LaunchdLifecycle::Throttled,
        Some(other) => LaunchdLifecycle::Other(other),
        None => LaunchdLifecycle::Missing,
    }
}

fn parse_launchd_runs(output: &str) -> Option<u64> {
    launchd_value(output, "runs")?.parse().ok()
}

fn parse_launchd_respawn_policy(output: &str) -> RespawnPolicy {
    // Observed on macOS 15.6.1 (24G90): `launchctl print` normalizes the rendered
    // KeepAlive.SuccessfulExit=false setting to `semaphores = { successful exit => 0 }`.
    let unsuccessful_exit_keepalive = ["successful exit", "SuccessfulExit"]
        .into_iter()
        .filter_map(|field| launchd_nested_property(output, "semaphores", field))
        .any(|value| matches!(value, "false" | "0"));
    if unsuccessful_exit_keepalive {
        RespawnPolicy::KeepAliveOnUnsuccessfulExit
    } else {
        RespawnPolicy::Unknown
    }
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
    if launchd_property(output, "last terminating signal").is_some() {
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

pub(super) fn parse_launchd_observation<T>(
    record: LaunchdRecord<T>,
    missing_evidence: MissingRecordEvidence,
    attribution: LaunchdAttribution,
) -> DaemonObservation
where
    T: AsRef<str>,
{
    let LaunchdRecord::Loaded(output) = record else {
        return if missing_evidence == MissingRecordEvidence::DefinitionPresent {
            DaemonObservation::definition_without_record(())
        } else {
            DaemonObservation::NotInstalled
        };
    };
    let output = output.as_ref();
    let lifecycle = parse_launchd_lifecycle(output);
    let respawn_policy = parse_launchd_respawn_policy(output);
    let observed_sequence = parse_launchd_runs(output);
    let termination =
        parse_launchd_termination(output).map_or(AttributedTermination::None, |termination| {
            match termination.failure() {
                None => AttributedTermination::None,
                Some(failure) if attribution.attributes(observed_sequence, termination) => {
                    AttributedTermination::Attributed(failure)
                }
                Some(_) => AttributedTermination::SuppressedAsAction,
            }
        });
    lifecycle.into_observation(respawn_policy, observed_sequence, termination)
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
        let Some((label, value)) = launchd_assignment(line) else {
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
    //! Proves launchd record reduction.

    use super::super::model::LAUNCHD_LABEL;
    use super::*;

    fn unsuccessful_exit_respawn_record(output: &str) -> String {
        format!(
            "path = /Library/LaunchAgents/{LAUNCHD_LABEL}.plist\nsemaphores = {{\n    successful exit => 0\n}}\n{output}"
        )
    }

    fn parse_state(output: &str, attribution: LaunchdAttribution) -> DaemonState {
        match parse_launchd_observation(
            LaunchdRecord::Loaded(unsuccessful_exit_respawn_record(output)),
            MissingRecordEvidence::DefinitionPresent,
            attribution,
        ) {
            DaemonObservation::Installed { state, .. } => state.into_state(),
            DaemonObservation::NotInstalled => unreachable!("output always represents a job"),
        }
    }

    fn restarting_exit_failure(code: i32) -> DaemonState {
        DaemonState::Restarting(Some(DaemonFailure::described(format!("exit code {code}"))))
    }

    fn restarting_signal_failure(name: &str, number: i32) -> DaemonState {
        DaemonState::Restarting(Some(DaemonFailure::described(format!(
            "signal {name}: {number}"
        ))))
    }

    fn restarting_unnamed_signal_failure(number: i32) -> DaemonState {
        DaemonState::Restarting(Some(DaemonFailure::described(format!("signal {number}"))))
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
    fn keepalive_exit_spawn_scheduled_and_throttling_remain_pending() {
        for output in [
            "state = exited\nruns = 1\nlast exit code = 2\n",
            "state = spawn scheduled\nruns = 1\nlast exit code = 2\n",
        ] {
            let observation = parse_launchd_observation(
                LaunchdRecord::Loaded(unsuccessful_exit_respawn_record(output)),
                MissingRecordEvidence::DefinitionPresent,
                LaunchdAttribution::Unfiltered,
            );
            assert!(!observation.settles_start_poll());
        }

        let throttled = parse_launchd_observation(
            LaunchdRecord::Loaded(unsuccessful_exit_respawn_record(
                "state = throttled\nruns = 1\nlast exit code = 2\n",
            )),
            MissingRecordEvidence::DefinitionPresent,
            LaunchdAttribution::Unfiltered,
        );
        assert!(!throttled.settles_start_poll());
    }

    #[test]
    fn running_settles_but_pre_spawn_waiting_states_remain_pending() {
        let running = parse_launchd_observation(
            LaunchdRecord::Loaded(unsuccessful_exit_respawn_record("state = running\n")),
            MissingRecordEvidence::DefinitionPresent,
            LaunchdAttribution::Unfiltered,
        );
        assert!(running.settles_start_poll());

        for lifecycle in ["waiting", "not running"] {
            let observation = parse_launchd_observation(
                LaunchdRecord::Loaded(unsuccessful_exit_respawn_record(&format!(
                    "state = {lifecycle}\n"
                ))),
                MissingRecordEvidence::DefinitionPresent,
                LaunchdAttribution::Unfiltered,
            );
            assert!(!observation.settles_start_poll());
        }
    }

    #[test]
    fn numeric_exit_attribution_obeys_action_provenance() {
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
            DaemonState::Transitioning(DaemonTransition::Starting)
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
        assert_eq!(
            parse_state(
                "state = spawn scheduled\nruns = 7\nlast exit code = 78\n",
                LaunchdAttribution::SinceRun(4),
            ),
            restarting_exit_failure(78)
        );
        assert_eq!(
            parse_state(
                "state = waiting\nruns = 5\nlast exit code = 78\n",
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
            restarting_signal_failure("Bus error", 10)
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
            restarting_signal_failure("Segmentation fault", 11)
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
            restarting_signal_failure("Segmentation fault", 11)
        );
        assert_eq!(
            parse_state(
                "state = waiting\nruns = 4\nlast terminating signal = Terminated: 15\n",
                LaunchdAttribution::SinceRun(3),
            ),
            DaemonState::Restarting(None)
        );
        assert_eq!(
            parse_state(
                "state = waiting\nlast terminating signal = Segmentation fault: 11\n",
                LaunchdAttribution::Unattributable,
            ),
            restarting_signal_failure("Segmentation fault", 11)
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
    fn loaded_job_without_manager_policy_does_not_assume_keepalive() {
        let clean_exit = parse_launchd_observation(
            LaunchdRecord::Loaded("state = exited\nruns = 4\nlast exit code = 0\n"),
            MissingRecordEvidence::None,
            LaunchdAttribution::Unfiltered,
        );
        let failed_wait = parse_launchd_observation(
            LaunchdRecord::Loaded(
                "state = waiting\nruns = 4\nlast terminating signal = Segmentation fault: 11\n",
            ),
            MissingRecordEvidence::None,
            LaunchdAttribution::Unfiltered,
        );

        assert!(!clean_exit.settles_start_poll());
        assert!(!failed_wait.settles_start_poll());
        assert!(matches!(failed_wait, DaemonObservation::Installed {
            state: ObservedDaemonState::Pending(PendingDaemonState::Unknown(_)),
            ..
        }));
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
            parse_launchd_observation(
                LaunchdRecord::<&str>::Missing,
                MissingRecordEvidence::DefinitionPresent,
                LaunchdAttribution::Unfiltered,
            ),
            DaemonObservation::definition_without_record(())
        );
        assert_eq!(
            parse_launchd_observation(
                LaunchdRecord::<&str>::Missing,
                MissingRecordEvidence::None,
                LaunchdAttribution::Unfiltered,
            ),
            DaemonObservation::NotInstalled
        );
    }

    #[test]
    fn unfiltered_status_attributes_external_sigkill() {
        assert_eq!(
            parse_state(
                "state = throttled\nruns = 5\nlast terminating signal = Killed: 9\n",
                LaunchdAttribution::Unfiltered,
            ),
            restarting_signal_failure("Killed", 9)
        );
    }

    #[test]
    fn loaded_record_policy_requires_the_exact_keepalive_condition_but_not_a_path() {
        assert_eq!(
            parse_launchd_respawn_policy(
                "path = /tmp/rings.plist\nproperties = keepalive | runatload\n"
            ),
            RespawnPolicy::Unknown
        );
        assert_eq!(
            parse_launchd_respawn_policy(
                "io.ringsnetwork.node = {\n    path = /tmp/rings.plist\n    semaphores = {\n        successful exit => 0\n    }\n}\n"
            ),
            RespawnPolicy::KeepAliveOnUnsuccessfulExit
        );
        assert_eq!(
            parse_launchd_respawn_policy("path = /tmp/rings.plist\nproperties = runatload\n"),
            RespawnPolicy::Unknown
        );
        assert_eq!(
            parse_launchd_respawn_policy(
                "io.ringsnetwork.node = {\n    semaphores = {\n        SuccessfulExit => 0\n    }\n    properties = runatload | inferred program\n}\n"
            ),
            RespawnPolicy::KeepAliveOnUnsuccessfulExit
        );
    }

    #[test]
    fn captured_macos_record_proves_the_renderer_respawn_policy() {
        // Raw structural excerpt captured from `launchctl print
        // gui/501/io.ringsnetwork.node` on macOS 15.6.1 (24G90).
        let output = r#"gui/501/io.ringsnetwork.node = {
    active count = 1
    state = running
    runs = 1
    last exit code = (never exited)
    semaphores = {
        successful exit => 0
    }
    properties = runatload | inferred program
}"#;

        assert_eq!(
            parse_launchd_respawn_policy(output),
            RespawnPolicy::KeepAliveOnUnsuccessfulExit
        );
    }

    #[test]
    fn nested_lookup_does_not_escape_the_selected_dictionary() {
        let output = "path = /tmp/rings.plist\nsemaphores = {\n}\nunrelated = {\n    successful exit => 0\n}\n";

        assert_eq!(parse_launchd_respawn_policy(output), RespawnPolicy::Unknown);
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
        let expected = restarting_unnamed_signal_failure(11);
        let state = parse_state(
            "state = exited\nlast terminating signal = 11\n",
            LaunchdAttribution::Unfiltered,
        );

        assert_eq!(state, expected);
        assert_eq!(expected.to_string(), "restarting (signal 11)");
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
    fn root_launchd_properties_are_not_shadowed_by_nested_dictionaries() {
        let output = r#"io.ringsnetwork.node = {
    program = /tmp/{literal
    endpoints = {
        state = throttled
        runs = 999
        last exit code = 78
    }
    state = running
    runs = 4
}"#;

        assert_eq!(parse_launchd_lifecycle(output), LaunchdLifecycle::Running);
        assert_eq!(parse_launchd_runs(output), Some(4));
        assert_eq!(parse_launchd_termination(output), None);
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
